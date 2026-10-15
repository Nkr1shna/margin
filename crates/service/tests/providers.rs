//! Switchable providers on the service (`docs/agent-providers-brief.md` D9, D10, D13,
//! D14): the slots and keys in Settings, `provider_config`/`model_for` per provider and
//! route, the pipeline override staying with its provider, the model lists and the
//! window cache, chats fixed on their provider, the small and writing routes following
//! the default provider, traces per provider, and "Sign in with ChatGPT" end to end
//! against a fake OAuth server (loopback callback, PKCE, id_token check, refresh with
//! rotation and `earliest_refresh_at`, a refused refresh signing out, sign-out
//! revoking). Fakes only: no real provider is called.

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Form, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use common::*;
use jsonwebtoken::jwk::Jwk;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use margin_agent::Scripted;
use margin_agent::scripted::text_reply;
use margin_ai::{AiError, MODEL_WINDOW_FALLBACK, ModelInfo, ModelWindow};
use margin_service::providers::{self, CHATGPT_SCOPE, ChatGptTokens, DYNAMIC_CLIENT_ID, Endpoints};
use margin_service::secrets::{ANTHROPIC_API_KEY, CHATGPT_TOKENS, SecretStore};
use margin_service::state::{SETTINGS_ID, SettingsRow};
use margin_shared::context::{TraceKind, TraceRow};
use margin_shared::pipeline::Route;
use margin_shared::{
    ApiError, Chat, ChatDetail, ChatGptStart, ChatGptStatus, ChatItemKind, ChatState, Event,
    Provider, RouteModels, Settings, TitleSource, error_code, routes,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;

// ---------------------------------------------------------------------------
// A fake ChatGPT OAuth server
// ---------------------------------------------------------------------------

const FAKE_KID: &str = "fake-key-1";
const OWNER_EMAIL: &str = "owner@example.com";
const ISSUED_CLIENT_ID: &str = "client-issued-1";

struct Grant {
    client_id: String,
    challenge: String,
    redirect_uri: String,
    nonce: String,
}

struct FakeAuth {
    base: Mutex<String>,
    key: EncodingKey,
    jwk: Jwk,
    grants: Mutex<HashMap<String, Grant>>,
    /// The refresh token that is valid now (rotated on every refresh).
    refresh: Mutex<String>,
    issued: AtomicUsize,
    /// Every authorization request's query.
    authorize: Mutex<Vec<HashMap<String, String>>>,
    /// Every token request's form.
    token_forms: Mutex<Vec<HashMap<String, String>>>,
    revoked: Mutex<Vec<String>>,
    refuse_refresh: AtomicBool,
    /// The scope granted (the full one unless a test narrows it).
    scope: Mutex<String>,
    /// `expires_in` of the next tokens.
    expires_in: AtomicI64,
    /// `earliest_refresh_at` of the next tokens, as seconds from now.
    earliest_in: Mutex<Option<i64>>,
    /// Signs id_tokens with another nonce.
    wrong_nonce: AtomicBool,
    /// Held by a test to keep token requests waiting (each takes it while answering).
    gate: tokio::sync::Mutex<()>,
}

impl FakeAuth {
    fn new() -> Self {
        // TEST-ONLY: a throwaway RSA key generated for this fake server (the file says
        // so above its PEM block); nothing outside these tests trusts it.
        let pem = include_bytes!("common/fixtures/fake-auth-signing-key.pem");
        let key = EncodingKey::from_rsa_pem(pem).expect("the test key");
        let mut jwk = Jwk::from_encoding_key(&key, Algorithm::RS256).unwrap();
        jwk.common.key_id = Some(FAKE_KID.into());
        Self {
            base: Mutex::new(String::new()),
            key,
            jwk,
            grants: Mutex::new(HashMap::new()),
            refresh: Mutex::new(String::new()),
            issued: AtomicUsize::new(0),
            authorize: Mutex::new(Vec::new()),
            token_forms: Mutex::new(Vec::new()),
            revoked: Mutex::new(Vec::new()),
            refuse_refresh: AtomicBool::new(false),
            scope: Mutex::new(CHATGPT_SCOPE.into()),
            expires_in: AtomicI64::new(3600),
            earliest_in: Mutex::new(None),
            wrong_nonce: AtomicBool::new(false),
            gate: tokio::sync::Mutex::new(()),
        }
    }

    fn id_token(&self, client_id: &str, nonce: &str) -> String {
        let now = Utc::now().timestamp();
        let nonce = if self.wrong_nonce.load(Ordering::SeqCst) {
            "not-the-nonce"
        } else {
            nonce
        };
        let claims = json!({
            "iss": self.base.lock().unwrap().clone(),
            "aud": client_id,
            "sub": "user-123",
            "email": OWNER_EMAIL,
            "nonce": nonce,
            "iat": now,
            "exp": now + 3600,
        });
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(FAKE_KID.into());
        jsonwebtoken::encode(&header, &claims, &self.key).unwrap()
    }

    /// New tokens: `at-N`, `rt-N` (the valid refresh token from now on).
    fn tokens(&self, id_token: Option<String>) -> Value {
        let n = self.issued.fetch_add(1, Ordering::SeqCst) + 1;
        let refresh = format!("rt-{n}");
        *self.refresh.lock().unwrap() = refresh.clone();
        let mut body = json!({
            "access_token": format!("at-{n}"),
            "refresh_token": refresh,
            "token_type": "Bearer",
            "expires_in": self.expires_in.load(Ordering::SeqCst),
            "scope": self.scope.lock().unwrap().clone(),
        });
        if let Some(id) = id_token {
            body["id_token"] = json!(id);
        }
        if let Some(secs) = *self.earliest_in.lock().unwrap() {
            body["earliest_refresh_at"] = json!(Utc::now().timestamp() + secs);
        }
        body
    }
}

fn s256(verifier: &str) -> String {
    use base64::Engine;
    use sha2::Digest;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()))
}

async fn authorize(
    State(f): State<Arc<FakeAuth>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    f.authorize.lock().unwrap().push(q.clone());
    let n = f.grants.lock().unwrap().len() + 1;
    let code = format!("code-{n}-{}", f.issued.load(Ordering::SeqCst));
    let first = q["client_id"] == DYNAMIC_CLIENT_ID;
    let client_id = if first {
        ISSUED_CLIENT_ID.to_string()
    } else {
        q["client_id"].clone()
    };
    assert_eq!(q["code_challenge_method"], "S256");
    f.grants.lock().unwrap().insert(
        code.clone(),
        Grant {
            client_id: client_id.clone(),
            challenge: q["code_challenge"].clone(),
            redirect_uri: q["redirect_uri"].clone(),
            nonce: q["nonce"].clone(),
        },
    );
    let mut to = reqwest::Url::parse(&q["redirect_uri"]).unwrap();
    {
        let mut pairs = to.query_pairs_mut();
        pairs
            .append_pair("code", &code)
            .append_pair("state", &q["state"])
            .append_pair("scope", &f.scope.lock().unwrap());
        if first {
            pairs.append_pair("client_id", &client_id);
        }
    }
    (StatusCode::FOUND, [(header::LOCATION, to.to_string())]).into_response()
}

async fn token(
    State(f): State<Arc<FakeAuth>>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    f.token_forms.lock().unwrap().push(form.clone());
    let _gate = f.gate.lock().await;
    let refuse =
        |error: &str| (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response();
    match form["grant_type"].as_str() {
        "authorization_code" => {
            let Some(grant) = f.grants.lock().unwrap().remove(&form["code"]) else {
                return refuse("invalid_grant");
            };
            if form["client_id"] != grant.client_id
                || s256(&form["code_verifier"]) != grant.challenge
                || form["redirect_uri"] != grant.redirect_uri
            {
                return refuse("invalid_request");
            }
            let id = f.id_token(&grant.client_id, &grant.nonce);
            Json(f.tokens(Some(id))).into_response()
        }
        "refresh_token" => {
            if f.refuse_refresh.load(Ordering::SeqCst)
                || form["refresh_token"] != *f.refresh.lock().unwrap()
            {
                return refuse("invalid_grant");
            }
            Json(f.tokens(None)).into_response()
        }
        _ => refuse("unsupported_grant_type"),
    }
}

async fn jwks(State(f): State<Arc<FakeAuth>>) -> Json<Value> {
    Json(json!({"keys": [f.jwk]}))
}

async fn revoke(
    State(f): State<Arc<FakeAuth>>,
    Form(form): Form<HashMap<String, String>>,
) -> StatusCode {
    f.revoked.lock().unwrap().push(form["token"].clone());
    StatusCode::OK
}

async fn start_fake_auth() -> (String, Arc<FakeAuth>) {
    let fake = Arc::new(FakeAuth::new());
    let app = Router::new()
        .route("/api/accounts/authorize", get(authorize))
        .route("/api/accounts/oauth/token", post(token))
        .route("/api/accounts/oauth/revoke", post(revoke))
        .route("/.well-known/jwks.json", get(jwks))
        .with_state(fake.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    *fake.base.lock().unwrap() = url.clone();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, fake)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn signed_in() -> (Harness, String) {
    let h = start().await;
    *h.state.turns.retry.lock().unwrap() = margin_agent::RetryPolicy::immediate();
    let t = h.sign_in("Mac").await;
    h.configure_ai(&t).await;
    (h, t)
}

/// Points ChatGPT at the fake auth server (callback on a free port).
fn use_fake_auth(h: &Harness, auth: &str) {
    h.state.providers.set_endpoints(Endpoints {
        chatgpt_auth: auth.to_string(),
        chatgpt_callback_port: 0,
        ..Endpoints::default()
    });
}

async fn patch(h: &Harness, t: &str, body: Value) -> reqwest::Response {
    h.patch(t, routes::SETTINGS)
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn settings(h: &Harness, t: &str) -> Settings {
    h.get(t, routes::SETTINGS)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn status(h: &Harness, t: &str) -> ChatGptStatus {
    let r = h.get(t, routes::PROVIDERS_CHATGPT).send().await.unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

async fn start_sign_in(h: &Harness, t: &str) -> (ChatGptStart, HashMap<String, String>) {
    let r = h
        .post(t, routes::PROVIDERS_CHATGPT_START)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let start: ChatGptStart = r.json().await.unwrap();
    let query = reqwest::Url::parse(&start.authorize_url)
        .unwrap()
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    (start, query)
}

/// What the owner's browser does: open the authorization URL, follow its redirect to
/// the service's loopback callback. Returns the callback page.
async fn browser(h: &Harness, authorize_url: &str) -> (u16, String) {
    let r = h.http.get(authorize_url).send().await.unwrap();
    assert_eq!(r.status(), 302);
    let to = r.headers()[header::LOCATION].to_str().unwrap().to_string();
    assert!(to.starts_with("http://127.0.0.1:"), "{to}");
    let page = h.http.get(&to).send().await.unwrap();
    (page.status().as_u16(), page.text().await.unwrap())
}

/// `browser` off the test's task (for a token exchange held at the fake's gate).
fn browser_in_background(
    http: reqwest::Client,
    authorize_url: String,
) -> tokio::task::JoinHandle<(u16, String)> {
    tokio::spawn(async move {
        let r = http.get(&authorize_url).send().await.unwrap();
        assert_eq!(r.status(), 302);
        let to = r.headers()[header::LOCATION].to_str().unwrap().to_string();
        let page = http.get(&to).send().await.unwrap();
        (page.status().as_u16(), page.text().await.unwrap())
    })
}

/// Waits until the fake auth server has had `n` token requests.
async fn token_requests(fake: &FakeAuth, n: usize) {
    eventually(|| async { (fake.token_forms.lock().unwrap().len() >= n).then_some(()) }).await;
}

/// A whole sign-in.
async fn sign_in_chatgpt(h: &Harness, t: &str) {
    let (start, _) = start_sign_in(h, t).await;
    let (code, page) = browser(h, &start.authorize_url).await;
    assert_eq!(code, 200, "{page}");
    assert!(matches!(status(h, t).await, ChatGptStatus::SignedIn { .. }));
}

fn stored_tokens(h: &Harness) -> ChatGptTokens {
    serde_json::from_str(&h.secrets.get(CHATGPT_TOKENS).unwrap().unwrap()).unwrap()
}

/// Moves the stored access token's expiry to `secs` from now.
fn expire_in(h: &Harness, secs: i64) {
    let mut t = stored_tokens(h);
    t.expires_at = Some(Utc::now() + chrono::Duration::seconds(secs));
    h.secrets
        .set(CHATGPT_TOKENS, &serde_json::to_string(&t).unwrap())
        .unwrap();
}

/// Moves the stored `earliest_refresh_at` into the past.
fn refreshable_now(h: &Harness) {
    let mut t = stored_tokens(h);
    t.earliest_refresh_at = Some(Utc::now() - chrono::Duration::seconds(1));
    h.secrets
        .set(CHATGPT_TOKENS, &serde_json::to_string(&t).unwrap())
        .unwrap();
}

/// The stored id_token (kept after a sign-out, for the next sign-in).
fn kept_id_token(h: &Harness) -> String {
    stored_tokens(h).id_token.expect("an id_token")
}

async fn bearer(h: &Harness) -> Result<String, (String, String)> {
    match h.state.provider_config(Provider::ChatGpt).await {
        Ok(c) => Ok(c.credential.secret().to_string()),
        Err(e) => Err((e.code.to_string(), e.message)),
    }
}

async fn chat(h: &Harness, t: &str, body: Value) -> ChatDetail {
    let r = h.post(t, "/api/chats").json(&body).send().await.unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

async fn wait_chat(
    h: &Harness,
    t: &str,
    id: &str,
    done: impl Fn(&ChatDetail) -> bool,
) -> ChatDetail {
    eventually(|| async {
        let d: ChatDetail = h
            .get(t, &format!("/api/chats/{id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        done(&d).then_some(d)
    })
    .await
}

async fn traces(h: &Harness, t: &str, trace_id: &str) -> Vec<TraceRow> {
    let r = h
        .get(t, routes::TRACES)
        .query(&[("trace_id", trace_id)])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.text()
        .await
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

async fn add_claude_key(h: &Harness, t: &str) {
    let r = patch(h, t, json!({"anthropic_api_key": CLAUDE_KEY})).await;
    assert_eq!(r.status(), 200);
}

// ---------------------------------------------------------------------------
// Settings, keys, slots, errors
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn settings_hold_three_slots_per_provider_and_the_keys_stay_on_the_service() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    let s = settings(&h, &t).await;
    assert_eq!(s.default_provider, Provider::OpenAi);
    assert_eq!(s.claude_models, RouteModels::claude_default());
    assert_eq!(s.chatgpt_models, RouteModels::chatgpt_default());
    assert_eq!(s.openai_models.small, "gpt-6-luna");
    assert!(!s.has_claude_api_key && s.chatgpt.is_none());

    // The deprecated agent model becomes the OpenAI chat slot.
    let row = SettingsRow {
        openai_model: "gpt-w".into(),
        agent_model: Some("gpt-c".into()),
        ..SettingsRow::default()
    };
    h.state
        .store
        .put(margin_service::db::SETTINGS, SETTINGS_ID, &row)
        .await
        .unwrap();
    let s = settings(&h, &t).await;
    assert_eq!(
        s.openai_models,
        RouteModels {
            chat: "gpt-c".into(),
            write: "gpt-w".into(),
            small: "gpt-6-luna".into()
        }
    );

    // The Claude key is stored, never returned, and removable.
    let mut sock = h.socket(&t).await;
    let r = patch(
        &h,
        &t,
        json!({"anthropic_api_key": format!("  {CLAUDE_KEY} ")}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let body = r.text().await.unwrap();
    assert!(!body.contains(CLAUDE_KEY), "{body}");
    assert_eq!(
        h.secrets.get(ANTHROPIC_API_KEY).unwrap().as_deref(),
        Some(CLAUDE_KEY)
    );
    let s: Settings = serde_json::from_str(&body).unwrap();
    assert!(s.has_claude_api_key && s.has_credential(Provider::Claude));
    sock.until(|e| matches!(e, Event::SettingsChanged { settings } if settings.has_claude_api_key))
        .await;

    // Slots and the default provider.
    let slots = json!({"chat": " claude-sonnet-5-5 ", "write": "claude-opus-5-5", "small": "claude-haiku-5-5"});
    let s: Settings = patch(
        &h,
        &t,
        json!({"claude_models": slots, "default_provider": "claude"}),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(s.default_provider, Provider::Claude);
    assert_eq!(s.claude_models.chat, "claude-sonnet-5-5");
    // An empty slot is refused, and nothing changes.
    let r = patch(
        &h,
        &t,
        json!({"chatgpt_models": {"chat": "", "write": "w", "small": "s"}}),
    )
    .await;
    assert_eq!(r.status(), 400);
    let e: ApiError = r.json().await.unwrap();
    assert_eq!(e.code, "no_model");
    // The app's fake answers with the same words (`crates/app/src/api.rs`).
    assert_eq!(
        e.message,
        "Every ChatGPT slot needs a model: chats, writing and small tasks."
    );
    assert_eq!(
        settings(&h, &t).await.chatgpt_models,
        RouteModels::chatgpt_default()
    );

    let r = patch(&h, &t, json!({"anthropic_api_key": ""})).await;
    assert_eq!(r.status(), 200);
    assert_eq!(h.secrets.get(ANTHROPIC_API_KEY).unwrap(), None);
    assert!(!settings(&h, &t).await.has_claude_api_key);
}

#[tokio::test(flavor = "multi_thread")]
async fn model_for_names_the_missing_credential_or_slot_per_provider() {
    let (h, t) = signed_in().await;
    let err = |r: Result<_, margin_service::error::AppError>| {
        let e = r.expect_err("an error");
        (e.code.to_string(), e.message)
    };
    let (code, message) = err(h.state.model_for(Route::Chat, Some(Provider::Claude)).await);
    assert_eq!(code, error_code::NO_API_KEY);
    assert!(message.contains("Claude API key"), "{message}");
    let (code, message) = err(h
        .state
        .model_for(Route::Small, Some(Provider::ChatGpt))
        .await);
    assert_eq!(code, error_code::AUTH_FAILED);
    assert!(message.contains("Sign in with ChatGPT"), "{message}");

    // OpenAI: configured, each route on its slot.
    let m = h.state.model_for(Route::Write, None).await.unwrap();
    assert_eq!(
        (m.config.provider, m.model.as_str()),
        (Provider::OpenAi, "test-model")
    );
    // An empty OpenAI slot is `no_model`, naming the slot.
    patch(&h, &t, json!({"openai_model": ""})).await;
    let (code, message) = err(h.state.model_for(Route::Write, None).await);
    assert_eq!(code, error_code::NO_MODEL);
    assert!(
        message.contains("for writing") && message.contains("OpenAI"),
        "{message}"
    );

    // Claude once it has a key: its own slots, its own base URL, the key as the
    // credential; the default provider decides `None`.
    add_claude_key(&h, &t).await;
    patch(&h, &t, json!({"default_provider": "claude"})).await;
    for (route, model) in [
        (Route::Chat, "claude-opus-5-5"),
        (Route::Write, "claude-opus-5-5"),
        (Route::Small, "claude-haiku-5-5"),
    ] {
        let m = h.state.model_for(route, None).await.unwrap();
        assert_eq!(m.config.provider, Provider::Claude);
        assert_eq!(m.model, model);
        assert_eq!(m.config.base_url, "https://api.anthropic.com");
        assert_eq!(m.config.credential.secret(), CLAUDE_KEY);
        assert!(!format!("{m:?}").contains(CLAUDE_KEY));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pipeline_route_override_stays_with_its_provider() {
    let (h, t) = signed_in().await;
    add_claude_key(&h, &t).await;
    let set_routes = |chat: &'static str| {
        let h = &h;
        let t = &t;
        async move {
            let mut cfg = settings(h, t).await.pipeline;
            cfg.routes.chat = Some(chat.into());
            assert_eq!(patch(h, t, json!({"pipeline": cfg})).await.status(), 200);
        }
    };
    set_routes("claude-sonnet-5-5").await;
    let openai = h
        .state
        .model_for(Route::Chat, Some(Provider::OpenAi))
        .await
        .unwrap();
    assert_eq!(
        openai.model, "test-model",
        "a Claude id never lands on OpenAI"
    );
    let claude = h
        .state
        .model_for(Route::Chat, Some(Provider::Claude))
        .await
        .unwrap();
    assert_eq!(claude.model, "claude-sonnet-5-5");
    assert_eq!(
        claude.config.models.write, "claude-opus-5-5",
        "other slots kept"
    );

    set_routes("gpt-variant").await;
    let openai = h
        .state
        .model_for(Route::Chat, Some(Provider::OpenAi))
        .await
        .unwrap();
    assert_eq!(openai.model, "gpt-variant");
    let claude = h
        .state
        .model_for(Route::Chat, Some(Provider::Claude))
        .await
        .unwrap();
    assert_eq!(claude.model, "claude-opus-5-5");

    // Once the provider's list is known, an id not in it is ignored too.
    set_routes("claude-unknown-9").await;
    let _ = h
        .state
        .model_for(Route::Write, Some(Provider::Claude))
        .await
        .unwrap();
    let claude = h
        .state
        .model_for(Route::Chat, Some(Provider::Claude))
        .await
        .unwrap();
    assert_eq!(claude.model, "claude-opus-5-5");
}

// ---------------------------------------------------------------------------
// Model lists and windows
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_models_routes_list_each_providers_models_with_windows() {
    let (h, t) = signed_in().await;
    let get = |p: Provider| h.get(&t, &routes::providers_models(p)).send();

    let r = get(Provider::Claude).await.unwrap();
    assert_eq!(r.status(), 409);
    let e: ApiError = r.json().await.unwrap();
    assert_eq!(e.code, "not_configured");
    assert_eq!(get(Provider::ChatGpt).await.unwrap().status(), 409);

    add_claude_key(&h, &t).await;
    let list: Vec<ModelInfo> = get(Provider::Claude).await.unwrap().json().await.unwrap();
    let ids: Vec<&str> = list.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"],
        "defaults first"
    );
    assert_eq!(list[0].context_window, Some(500_000));
    assert_eq!(list[0].max_output_tokens, Some(64_000));

    // OpenAI's list carries no windows: the table's for ids it knows, none otherwise.
    h.generator.model_lists.lock().unwrap().insert(
        Provider::OpenAi,
        Some(vec![
            ModelInfo {
                id: "gpt-6-luna".into(),
                display_name: None,
                context_window: None,
                max_output_tokens: None,
            },
            ModelInfo {
                id: "my-local-model".into(),
                display_name: None,
                context_window: None,
                max_output_tokens: None,
            },
        ]),
    );
    let list: Vec<ModelInfo> = get(Provider::OpenAi).await.unwrap().json().await.unwrap();
    assert_eq!(list[0].context_window, Some(1_050_000));
    assert_eq!(list[1].context_window, None);

    // A provider that fails to list is a gateway error with a code.
    h.generator
        .model_lists
        .lock()
        .unwrap()
        .insert(Provider::OpenAi, None);
    let r = get(Provider::OpenAi).await.unwrap();
    assert_eq!(r.status(), 502);
    let e: ApiError = r.json().await.unwrap();
    assert_eq!(e.code, error_code::SERVER_ERROR);
}

#[tokio::test(flavor = "multi_thread")]
async fn windows_come_from_the_provider_and_are_cached_until_settings_change() {
    let (h, t) = signed_in().await;
    add_claude_key(&h, &t).await;
    patch(&h, &t, json!({"default_provider": "claude"})).await;
    let before = h.generator.list_calls.load(Ordering::SeqCst);
    let m = h.state.model_for(Route::Write, None).await.unwrap();
    assert_eq!(
        m.window,
        ModelWindow {
            input: 500_000,
            output: 64_000
        }
    );
    let small = h.state.model_for(Route::Small, None).await.unwrap();
    assert_eq!(small.window.input, 200_000);
    assert_eq!(
        h.generator.list_calls.load(Ordering::SeqCst),
        before + 1,
        "listed once"
    );

    // Settings changed: listed again.
    patch(&h, &t, json!({"reasoning_effort": "high"})).await;
    h.state.model_for(Route::Write, None).await.unwrap();
    assert_eq!(h.generator.list_calls.load(Ordering::SeqCst), before + 2);

    // A model the provider doesn't list runs on the fallback window.
    patch(
        &h,
        &t,
        json!({"claude_models": {"chat": "claude-opus-5-5", "write": "claude-new-9", "small": "claude-haiku-5-5"}}),
    )
    .await;
    let m = h.state.model_for(Route::Write, None).await.unwrap();
    assert_eq!(m.window, MODEL_WINDOW_FALLBACK);

    // OpenAI and ChatGPT windows come from margin_ai's table.
    let openai = h
        .state
        .model_for(Route::Chat, Some(Provider::OpenAi))
        .await
        .unwrap();
    assert_eq!(
        openai.window, MODEL_WINDOW_FALLBACK,
        "test-model isn't in the table"
    );
    patch(&h, &t, json!({"openai_models": {"chat": "gpt-6.1-sol", "write": "test-model", "small": "test-model"}})).await;
    let openai = h
        .state
        .model_for(Route::Chat, Some(Provider::OpenAi))
        .await
        .unwrap();
    assert_eq!(
        openai.window,
        ModelWindow {
            input: 1_050_000,
            output: 128_000
        }
    );
}

// ---------------------------------------------------------------------------
// Chats, titles, lessons, traces
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_keeps_the_provider_it_was_created_on() {
    let (h, t) = signed_in().await;
    let first = chat(&h, &t, json!({})).await.chat;
    assert_eq!(first.provider, Provider::OpenAi, "Settings' default");
    add_claude_key(&h, &t).await;
    patch(&h, &t, json!({"default_provider": "claude"})).await;
    let second = chat(&h, &t, json!({})).await.chat;
    assert_eq!(second.provider, Provider::Claude);
    let third = chat(&h, &t, json!({"provider": "chatgpt"})).await.chat;
    assert_eq!(third.provider, Provider::ChatGpt, "asked for");
    // Changing the default moves no chat.
    let again: ChatDetail = h
        .get(&t, &format!("/api/chats/{}", first.id))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(again.chat.provider, Provider::OpenAi);
    let listed: Vec<Chat> = h
        .get(&t, "/api/chats")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let of = |id: &str| listed.iter().find(|c| c.id == id).unwrap().provider;
    assert_eq!(of(&second.id), Provider::Claude);

    // A ChatGPT chat without a sign-in fails its turn with `auth_failed`.
    let r = h
        .post(&t, &format!("/api/chats/{}/messages", third.id))
        .json(&json!({"text": "hello"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let d = wait_chat(&h, &t, &third.id, |d| {
        matches!(d.chat.state, ChatState::Failed { .. })
    })
    .await;
    let error = d
        .items
        .iter()
        .find_map(|i| match &i.kind {
            margin_shared::ChatItemKind::Error { code, .. } => Some(code.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(error.as_deref(), Some(error_code::AUTH_FAILED));
}

#[tokio::test(flavor = "multi_thread")]
async fn titles_run_on_the_default_providers_small_slot_and_are_traced_with_it() {
    let (h, t) = signed_in().await;
    add_claude_key(&h, &t).await;
    patch(&h, &t, json!({"default_provider": "claude"})).await;
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Waves add up."])));
    h.title_reply("Adding waves");
    // The chat itself runs on OpenAI.
    let created = chat(
        &h,
        &t,
        json!({"provider": "openai", "message": {"text": "How do waves add?"}}),
    )
    .await;
    let d = wait_chat(&h, &t, &created.chat.id, |d| {
        d.chat.title_by == TitleSource::Model
    })
    .await;
    assert_eq!(d.chat.title, "Adding waves");
    assert_eq!(
        h.generator.agent.requests()[0].model.as_deref(),
        Some("test-model")
    );
    let calls = h.generator.text_calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        (calls[0].provider, calls[0].model.as_str()),
        (Provider::Claude, "claude-haiku-5-5")
    );
    assert!(calls[0].opts.max_output_tokens.is_some());

    let turn_id = d.items[0].turn_id.clone();
    let rows = eventually(|| async {
        let rows = traces(&h, &t, &turn_id).await;
        (rows
            .iter()
            .filter(|r| r.kind == TraceKind::ModelCall)
            .count()
            == 2)
            .then_some(rows)
    })
    .await;
    let step = rows
        .iter()
        .find(|r| r.kind == TraceKind::ModelCall && r.route == Some(Route::Chat))
        .unwrap();
    assert_eq!(step.provider.as_deref(), Some("openai"));
    let title = rows
        .iter()
        .find(|r| r.kind == TraceKind::ModelCall && r.route == Some(Route::Small))
        .unwrap();
    assert_eq!(title.provider.as_deref(), Some("claude"));
    assert_eq!(title.model.as_deref(), Some("claude-haiku-5-5"));
    assert!(
        title
            .prompt_version
            .as_deref()
            .is_some_and(|v| v.starts_with("title-"))
    );
    let u = title.usage.expect("usage");
    // Haiku 5.5: $0.10 in, $0.50 out per million.
    let expected = (u.input as f64 * 0.10 + u.output as f64 * 0.50) / 1e6;
    let cost = title.cost_usd.expect("priced");
    assert!((cost - expected).abs() < 1e-12, "{cost} vs {expected}");
}

#[tokio::test(flavor = "multi_thread")]
async fn lessons_follow_the_default_provider_on_its_writing_slot() {
    let (h, t) = signed_in().await;
    // Proposed on OpenAI, accepted once Claude is the default.
    let req = ask_request("Fourier series", 1, false);
    let p = h.propose(&t, &req).await;
    add_claude_key(&h, &t).await;
    patch(&h, &t, json!({"default_provider": "claude"})).await;
    let r = h
        .post(&t, &format!("/api/proposals/{}/accept", p.id))
        .json(&json!({"outline": p.outline.unwrap()}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let lesson_id = eventually(|| async {
        let ctx = h
            .generator
            .lesson_requests
            .lock()
            .unwrap()
            .first()
            .cloned()?;
        let id = h
            .state
            .store
            .list::<margin_shared::Lesson>(margin_service::db::LESSON)
            .await
            .ok()?
            .into_iter()
            .find(|l| l.status == margin_shared::LessonStatus::Ready)?
            .id;
        Some((ctx, id))
    })
    .await;
    let (ctx, lesson_id) = lesson_id;
    let choice = h.generator.choices.lock().unwrap().last().cloned().unwrap();
    assert_eq!(choice.config.provider, Provider::Claude);
    assert_eq!(choice.model, "claude-opus-5-5");
    assert_eq!(
        choice.window,
        ModelWindow {
            input: 500_000,
            output: 64_000
        }
    );
    let opts = h
        .generator
        .call_options
        .lock()
        .unwrap()
        .last()
        .cloned()
        .unwrap();
    assert_eq!(opts.route, Route::Write);
    let prompt = margin_ai::prompts::lesson(&ctx);
    assert_eq!(opts.cache_prefix_len, Some(prompt.constant.len()));
    assert_eq!(opts.cache_key.as_deref(), Some("lesson:lesson-v2"));
    assert_eq!(opts.prompt_version.as_deref(), Some("lesson-v2"));

    let rows = eventually(|| async {
        let rows = traces(&h, &t, &lesson_id).await;
        rows.iter()
            .any(|r| r.kind == TraceKind::ModelCall)
            .then_some(rows)
    })
    .await;
    let call = rows
        .iter()
        .find(|r| r.kind == TraceKind::ModelCall)
        .unwrap();
    assert_eq!(call.provider.as_deref(), Some("claude"));
    assert_eq!(call.model.as_deref(), Some("claude-opus-5-5"));
    assert!(call.cost_usd.is_some(), "Opus is priced");
}

// ---------------------------------------------------------------------------
// Sign in with ChatGPT
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn chatgpt_signs_in_on_the_loopback_and_refreshes_with_rotation() {
    let (auth, fake) = start_fake_auth().await;
    let (h, t) = signed_in().await;
    use_fake_auth(&h, &auth);
    assert_eq!(status(&h, &t).await, ChatGptStatus::SignedOut);
    let mut sock = h.socket(&t).await;

    // The first sign-in registers Margin.
    let (start, q) = start_sign_in(&h, &t).await;
    assert!(
        start
            .authorize_url
            .starts_with(&format!("{auth}/api/accounts/authorize?"))
    );
    assert_eq!(q["client_id"], DYNAMIC_CLIENT_ID);
    assert_eq!(q["agent_name_hint"], "Margin");
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["scope"], CHATGPT_SCOPE);
    assert_eq!(q["resource"], "https://api.openai.com/v1");
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["state"], start.state);
    assert!(q["ext_agent_host_id"].starts_with("urn:uuid:"));
    assert!(
        q["redirect_uri"].starts_with("http://127.0.0.1:")
            && q["redirect_uri"].ends_with("/auth/callback")
    );
    assert!(!q.contains_key("login_hint"));
    assert_eq!(
        status(&h, &t).await,
        ChatGptStatus::Waiting {
            authorize_url: start.authorize_url.clone()
        }
    );

    let (code, page) = browser(&h, &start.authorize_url).await;
    assert_eq!(code, 200, "{page}");
    assert!(page.contains(OWNER_EMAIL), "{page}");
    let ChatGptStatus::SignedIn {
        account,
        last_error: None,
    } = status(&h, &t).await
    else {
        panic!("not signed in");
    };
    assert_eq!(account.email, OWNER_EMAIL);
    assert!(account.expires_at.is_some());
    sock.until(|e| matches!(e, Event::SettingsChanged { settings } if settings.chatgpt.is_some()))
        .await;
    let body = h
        .get(&t, routes::SETTINGS)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!body.contains("at-1") && !body.contains("rt-1"), "{body}");
    let s: Settings = serde_json::from_str(&body).unwrap();
    assert!(s.has_credential(Provider::ChatGpt));
    let stored = stored_tokens(&h);
    assert_eq!(stored.client_id.as_deref(), Some(ISSUED_CLIENT_ID));
    assert_eq!(stored.sub.as_deref(), Some("user-123"));
    assert!(
        stored
            .scopes
            .iter()
            .any(|s| s == "chatgpt.tokens.use.direct")
    );
    // The code exchange sent the PKCE verifier (checked by the fake) and the resource.
    let form = fake.token_forms.lock().unwrap()[0].clone();
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["resource"], "https://api.openai.com/v1");

    // A fresh token is used as it is; the ChatGPT config is a bearer on the plan's API.
    let config = h.state.provider_config(Provider::ChatGpt).await.unwrap();
    assert_eq!(
        config.credential,
        margin_ai::Credential::Bearer("at-1".into())
    );
    assert_eq!(config.base_url, "https://api.openai.com/v1");
    assert_eq!(fake.token_forms.lock().unwrap().len(), 1);

    // Less than five minutes left: refreshed first, and the refresh token rotates.
    *fake.earliest_in.lock().unwrap() = Some(3600);
    expire_in(&h, 60);
    assert_eq!(bearer(&h).await.unwrap(), "at-2");
    let form = fake.token_forms.lock().unwrap()[1].clone();
    assert_eq!(
        (form["grant_type"].as_str(), form["refresh_token"].as_str()),
        ("refresh_token", "rt-1")
    );
    assert_eq!(form["client_id"], ISSUED_CLIENT_ID);
    assert_eq!(stored_tokens(&h).refresh_token.as_deref(), Some("rt-2"));

    // Not before `earliest_refresh_at`, even when it is about to run out…
    expire_in(&h, 60);
    assert_eq!(bearer(&h).await.unwrap(), "at-2");
    assert_eq!(fake.token_forms.lock().unwrap().len(), 2);
    // Nor when the API refused the token (a forced refresh): it fails without signing
    // out.
    let (code, _) = providers::chatgpt_access_token(&h.state, Some("at-2"))
        .await
        .map_err(|e| (e.code.to_string(), e.message))
        .unwrap_err();
    assert_eq!(code, error_code::AUTH_FAILED);
    assert_eq!(fake.token_forms.lock().unwrap().len(), 2);
    assert!(stored_tokens(&h).is_signed_in());
    // A refused token that is no longer the stored one: the stored one, no refresh.
    let newer = providers::chatgpt_access_token(&h.state, Some("at-1"))
        .await
        .unwrap();
    assert_eq!(newer, "at-2");
    assert_eq!(fake.token_forms.lock().unwrap().len(), 2);
    // Once `earliest_refresh_at` has passed, a refused token is refreshed.
    *fake.earliest_in.lock().unwrap() = None;
    refreshable_now(&h);
    let forced = providers::chatgpt_access_token(&h.state, Some("at-2"))
        .await
        .unwrap();
    assert_eq!(forced, "at-3");
    assert_eq!(fake.token_forms.lock().unwrap()[2]["refresh_token"], "rt-2");

    // A refresh the server refuses signs ChatGPT out.
    fake.refuse_refresh.store(true, Ordering::SeqCst);
    expire_in(&h, 60);
    let (code, _) = bearer(&h).await.unwrap_err();
    assert_eq!(code, error_code::AUTH_FAILED);
    assert_eq!(status(&h, &t).await, ChatGptStatus::SignedOut);
    sock.until(|e| matches!(e, Event::SettingsChanged { settings } if settings.chatgpt.is_none()))
        .await;
    let kept = stored_tokens(&h);
    assert!(!kept.is_signed_in());
    assert_eq!(
        kept.client_id.as_deref(),
        Some(ISSUED_CLIENT_ID),
        "the registration stays"
    );

    // The next sign-in reuses the registration and names the account.
    fake.refuse_refresh.store(false, Ordering::SeqCst);
    let (start, q) = start_sign_in(&h, &t).await;
    assert_eq!(q["client_id"], ISSUED_CLIENT_ID);
    assert_eq!(q["login_hint"], OWNER_EMAIL);
    assert!(!q.contains_key("agent_name_hint"));
    assert_eq!(
        q["ext_agent_host_id"], kept.ext_agent_host_id,
        "generated once"
    );
    let (code, _) = browser(&h, &start.authorize_url).await;
    assert_eq!(code, 200);

    // Signing in again while signed in: the URL goes to devices and the clipboard, so
    // it carries no id_token (`login_hint` only).
    let id_token = kept_id_token(&h);
    let (start, q) = start_sign_in(&h, &t).await;
    assert!(!q.contains_key("id_token_hint"));
    assert!(!start.authorize_url.contains(&id_token));
    assert_eq!(q["login_hint"], OWNER_EMAIL);
    // That attempt fails: still signed in, with why.
    let denied = format!(
        "{}?error=access_denied&error_description=The+owner+declined.&state={}",
        q["redirect_uri"], start.state
    );
    assert_eq!(h.http.get(&denied).send().await.unwrap().status(), 400);
    let ChatGptStatus::SignedIn {
        account,
        last_error,
    } = status(&h, &t).await
    else {
        panic!("still signed in");
    };
    assert_eq!(account.email, OWNER_EMAIL);
    assert_eq!(last_error.as_deref(), Some("The owner declined."));
    assert!(bearer(&h).await.is_ok());

    let current = stored_tokens(&h).refresh_token.unwrap();

    // Sign-out revokes the refresh token and forgets the tokens.
    let r = h
        .delete(&t, routes::PROVIDERS_CHATGPT)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    assert_eq!(fake.revoked.lock().unwrap().as_slice(), [current]);
    assert_eq!(status(&h, &t).await, ChatGptStatus::SignedOut);
    assert!(settings(&h, &t).await.chatgpt.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chatgpt_sign_in_that_fails_says_why() {
    let (auth, fake) = start_fake_auth().await;
    let (h, t) = signed_in().await;
    use_fake_auth(&h, &auth);

    // A callback for another sign-in (an old tab) is refused and the sign-in waits on.
    let (start, q) = start_sign_in(&h, &t).await;
    let stale = format!("{}?code=x&state=not-this-one", q["redirect_uri"]);
    let r = h.http.get(&stale).send().await.unwrap();
    assert_eq!(r.status(), 400);
    assert!(matches!(
        status(&h, &t).await,
        ChatGptStatus::Waiting { .. }
    ));
    // The owner declined in the browser.
    let denied = format!(
        "{}?error=access_denied&error_description=The+owner+declined.&state={}",
        q["redirect_uri"], start.state
    );
    let r = h.http.get(&denied).send().await.unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(
        status(&h, &t).await,
        ChatGptStatus::Failed {
            message: "The owner declined.".into()
        }
    );

    // The plan permission wasn't granted.
    *fake.scope.lock().unwrap() = "openid profile email offline_access".into();
    let (start, _) = start_sign_in(&h, &t).await;
    let (code, page) = browser(&h, &start.authorize_url).await;
    assert_eq!(code, 400);
    assert!(page.contains("chatgpt.tokens.use.direct"), "{page}");
    let ChatGptStatus::Failed { message } = status(&h, &t).await else {
        panic!()
    };
    assert!(message.contains("chatgpt.tokens.use.direct"), "{message}");
    assert!(settings(&h, &t).await.chatgpt.is_none());

    // An id_token for another sign-in (wrong nonce) is refused.
    *fake.scope.lock().unwrap() = CHATGPT_SCOPE.into();
    fake.wrong_nonce.store(true, Ordering::SeqCst);
    let (start, _) = start_sign_in(&h, &t).await;
    let (code, _) = browser(&h, &start.authorize_url).await;
    assert_eq!(code, 400);
    let ChatGptStatus::Failed { message } = status(&h, &t).await else {
        panic!()
    };
    assert!(message.contains("nonce"), "{message}");

    // Starting again replaces a waiting sign-in; this one works.
    fake.wrong_nonce.store(false, Ordering::SeqCst);
    let (old, _) = start_sign_in(&h, &t).await;
    let (start, _) = start_sign_in(&h, &t).await;
    assert_ne!(old.state, start.state);
    let (code, _) = browser(&h, &start.authorize_url).await;
    assert_eq!(code, 200);
    assert!(matches!(
        status(&h, &t).await,
        ChatGptStatus::SignedIn { .. }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chatgpt_chat_refreshes_a_refused_token_once_and_its_title_has_no_price() {
    let (auth, fake) = start_fake_auth().await;
    let (h, t) = signed_in().await;
    use_fake_auth(&h, &auth);
    sign_in_chatgpt(&h, &t).await;
    patch(&h, &t, json!({"default_provider": "chatgpt"})).await;

    // ChatGPT says the token is no good: the service refreshes it and calls again.
    h.generator.agent.push(Scripted::Fail(AiError::Http {
        status: 401,
        message: "invalid user".into(),
        retry_after: None,
        code: Some("subscription_sharing_invalid_user".into()),
    }));
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Plan answer."])));
    h.title_reply("Plan title");
    let created = chat(&h, &t, json!({"message": {"text": "Hello plan"}})).await;
    assert_eq!(created.chat.provider, Provider::ChatGpt);
    let d = wait_chat(&h, &t, &created.chat.id, |d| {
        d.chat.title_by == TitleSource::Model
    })
    .await;
    assert_eq!(d.chat.state, ChatState::Idle);
    assert_eq!(
        h.generator.agent.requests().len(),
        2,
        "the refused call and the retry"
    );
    assert_eq!(
        fake.token_forms.lock().unwrap().len(),
        2,
        "the sign-in and one refresh"
    );
    assert_eq!(stored_tokens(&h).access_token.as_deref(), Some("at-2"));

    // The title: ChatGPT's small slot, no output cap (the plan route takes none), and
    // traced without a dollar cost.
    let call = h.generator.text_calls.lock().unwrap()[0].clone();
    assert_eq!(
        (call.provider, call.model.as_str()),
        (Provider::ChatGpt, "gpt-6.1-sol")
    );
    assert_eq!(call.opts.max_output_tokens, None);
    let turn_id = d.items[0].turn_id.clone();
    let rows = eventually(|| async {
        let rows = traces(&h, &t, &turn_id).await;
        rows.iter()
            .any(|r| r.kind == TraceKind::ModelCall && r.route == Some(Route::Small))
            .then_some(rows)
    })
    .await;
    for r in rows.iter().filter(|r| r.kind == TraceKind::ModelCall) {
        assert_eq!(r.provider.as_deref(), Some("chatgpt"), "{r:#?}");
        assert_eq!(r.cost_usd, None, "plan-billed: {r:#?}");
    }
    // The refresh and the call again are inside one step: it is traced once, as it
    // ended.
    let steps: Vec<&TraceRow> = rows
        .iter()
        .filter(|r| r.kind == TraceKind::ModelCall && r.route == Some(Route::Chat))
        .collect();
    assert_eq!(steps.len(), 1);
    assert!(steps[0].ok);
}

#[tokio::test(flavor = "multi_thread")]
async fn account_limits_end_a_turn_with_the_fixed_d7_texts_not_the_apis_words() {
    let (h, t) = signed_in().await;
    let http = |status: u16, code: &str| {
        Scripted::Fail(AiError::Http {
            status,
            message: "raw API words".into(),
            retry_after: None,
            code: Some(code.into()),
        })
    };
    let streamed = |code: &str| {
        Scripted::Events(vec![margin_ai::ModelEvent::Failed {
            code: Some(code.into()),
            message: "raw API words".into(),
        }])
    };
    let plan_limit = "Margin's share of your ChatGPT plan is used up. Raise it in ChatGPT → \
                      Settings → Usage, or start a new chat on another provider.";
    let cases = [
        (
            http(429, "enforced_spend_limit_reached"),
            error_code::SPEND_LIMIT_REACHED,
            "Your Claude organization has reached its spend limit.",
        ),
        (
            http(429, "subscription_sharing_usage_limit_exceeded"),
            error_code::PLAN_LIMIT_REACHED,
            plan_limit,
        ),
        (
            streamed("subscription_sharing_usage_unavailable"),
            error_code::PLAN_LIMIT_REACHED,
            plan_limit,
        ),
        (
            http(403, "subscription_sharing_user_not_eligible"),
            error_code::PLAN_INELIGIBLE,
            "Your ChatGPT plan can't be used by apps like Margin.",
        ),
    ];
    for (reply, want_code, want_text) in cases {
        h.generator.agent.push(reply);
        let created = chat(&h, &t, json!({"message": {"text": "Hello"}})).await;
        let d = wait_chat(&h, &t, &created.chat.id, |d| {
            matches!(d.chat.state, ChatState::Failed { .. })
        })
        .await;
        let error = d
            .items
            .iter()
            .find_map(|i| match &i.kind {
                ChatItemKind::Error { message, code, .. } => Some((message, code)),
                _ => None,
            })
            .expect("an Error item");
        assert_eq!(error.0, want_text);
        assert_eq!(error.1.as_deref(), Some(want_code));
        assert!(!error.0.contains("raw API words"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_refreshes_of_one_refused_token_refresh_once() {
    let (auth, fake) = start_fake_auth().await;
    let (h, t) = signed_in().await;
    use_fake_auth(&h, &auth);
    sign_in_chatgpt(&h, &t).await;
    assert_eq!(stored_tokens(&h).access_token.as_deref(), Some("at-1"));

    // Two calls had `at-1` refused at once. The first refresh is held at the token
    // endpoint while the second asks; the second waits for it and takes its token
    // instead of refreshing again (which would rotate the refresh token twice).
    let gate = fake.gate.lock().await;
    let state = h.state.clone();
    let first =
        tokio::spawn(async move { providers::chatgpt_access_token(&state, Some("at-1")).await });
    token_requests(&fake, 2).await;
    let state = h.state.clone();
    let second =
        tokio::spawn(async move { providers::chatgpt_access_token(&state, Some("at-1")).await });
    drop(gate);
    let first = first.await.unwrap().map_err(|e| e.message).unwrap();
    let second = second.await.unwrap().map_err(|e| e.message).unwrap();
    assert_eq!((first.as_str(), second.as_str()), ("at-2", "at-2"));
    assert_eq!(
        fake.token_forms.lock().unwrap().len(),
        2,
        "the sign-in and one refresh"
    );
    assert!(stored_tokens(&h).is_signed_in());
    assert!(matches!(
        status(&h, &t).await,
        ChatGptStatus::SignedIn { .. }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_code_exchange_in_flight_does_not_undo_a_sign_out_or_a_newer_sign_in() {
    let (auth, fake) = start_fake_auth().await;
    let (h, t) = signed_in().await;
    use_fake_auth(&h, &auth);

    // The browser came back and the service is exchanging the code (held at the fake)
    // when the owner signs out: the exchange's tokens are not stored.
    let (start, _) = start_sign_in(&h, &t).await;
    let gate = fake.gate.lock().await;
    let page = browser_in_background(h.http.clone(), start.authorize_url.clone());
    token_requests(&fake, 1).await;
    let r = h
        .delete(&t, routes::PROVIDERS_CHATGPT)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    drop(gate);
    let (code, body) = page.await.unwrap();
    assert_eq!(code, 400, "{body}");
    assert!(body.contains("cancelled"), "{body}");
    assert_eq!(status(&h, &t).await, ChatGptStatus::SignedOut);
    assert!(!stored_tokens(&h).is_signed_in());
    assert!(settings(&h, &t).await.chatgpt.is_none());

    // The same while a newer sign-in started: the older one stores nothing and the
    // newer one is still waiting, then completes.
    let (old, _) = start_sign_in(&h, &t).await;
    let gate = fake.gate.lock().await;
    let page = browser_in_background(h.http.clone(), old.authorize_url.clone());
    token_requests(&fake, 2).await;
    let (newer, _) = start_sign_in(&h, &t).await;
    drop(gate);
    let (code, body) = page.await.unwrap();
    assert_eq!(code, 400, "{body}");
    assert!(!stored_tokens(&h).is_signed_in());
    assert_eq!(
        status(&h, &t).await,
        ChatGptStatus::Waiting {
            authorize_url: newer.authorize_url.clone()
        }
    );
    let (code, body) = browser(&h, &newer.authorize_url).await;
    assert_eq!(code, 200, "{body}");
    assert!(matches!(
        status(&h, &t).await,
        ChatGptStatus::SignedIn {
            last_error: None,
            ..
        }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn sign_out_cancels_a_sign_in_that_is_waiting() {
    let (auth, fake) = start_fake_auth().await;
    let (h, t) = signed_in().await;
    use_fake_auth(&h, &auth);
    let (start, q) = start_sign_in(&h, &t).await;
    assert!(matches!(
        status(&h, &t).await,
        ChatGptStatus::Waiting { .. }
    ));

    // Cancel in Settings: back to signed out, the loopback wait is over.
    let r = h
        .delete(&t, routes::PROVIDERS_CHATGPT)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    assert_eq!(status(&h, &t).await, ChatGptStatus::SignedOut);
    assert!(fake.revoked.lock().unwrap().is_empty(), "nothing to revoke");
    let late = format!("{}?code=x&state={}", q["redirect_uri"], start.state);
    eventually(|| async { h.http.get(&late).send().await.is_err().then_some(()) }).await;
    assert_eq!(status(&h, &t).await, ChatGptStatus::SignedOut);
    assert!(settings(&h, &t).await.chatgpt.is_none());
    // No code was exchanged.
    assert!(fake.token_forms.lock().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// A ChatGPT chat end to end: sign-in, the real client, web search
// ---------------------------------------------------------------------------

/// A fake Responses API for the ChatGPT tests: one scripted SSE body per request; every
/// request's `Authorization` and body are kept.
#[derive(Default)]
struct FakeResponses {
    replies: Mutex<std::collections::VecDeque<String>>,
    requests: Mutex<Vec<(Option<String>, Value)>>,
}

async fn fake_responses(
    State(f): State<Arc<FakeResponses>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    f.requests.lock().unwrap().push((auth, body));
    match f.replies.lock().unwrap().pop_front() {
        Some(sse) => ([(header::CONTENT_TYPE, "text/event-stream")], sse).into_response(),
        None => (StatusCode::INTERNAL_SERVER_ERROR, "no reply scripted").into_response(),
    }
}

async fn start_fake_responses() -> (String, Arc<FakeResponses>) {
    let fake = Arc::new(FakeResponses::default());
    let app = Router::new()
        .route("/v1/responses", post(fake_responses))
        .with_state(fake.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, fake)
}

fn sse_event(v: Value) -> String {
    format!("event: {}\ndata: {}\n\n", v["type"].as_str().unwrap(), v)
}

/// A searched answer in the Responses shapes: the `web_search_call` with its sources,
/// then a message with a `url_citation`.
fn web_search_answer() -> String {
    let searched = json!({"id": "ws_1", "type": "web_search_call", "status": "completed",
    "action": {"type": "search", "query": "rust 1.99 release date",
        "sources": [
            {"type": "url", "url": "https://blog.rust-lang.org/1.99"},
            {"type": "url", "url": "https://releases.rs", "title": "Rust releases"}
        ]}});
    let message = json!({"id": "msg_2", "type": "message", "role": "assistant", "status": "completed",
        "content": [{"type": "output_text", "text": "Rust 1.99 shipped in May.",
            "annotations": [{"type": "url_citation", "url": "https://blog.rust-lang.org/1.99",
                "title": "Rust 1.99", "start_index": 0, "end_index": 9}]}]});
    [
        sse_event(
            json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress", "output": []}}),
        ),
        sse_event(json!({"type": "response.output_item.added", "output_index": 0,
            "item": {"id": "ws_1", "type": "web_search_call", "status": "in_progress"}})),
        sse_event(
            json!({"type": "response.web_search_call.searching", "output_index": 0, "item_id": "ws_1"}),
        ),
        sse_event(json!({"type": "response.output_item.done", "output_index": 0, "item": searched})),
        sse_event(json!({"type": "response.output_item.added", "output_index": 1,
            "item": {"id": "msg_2", "type": "message", "role": "assistant", "status": "in_progress", "content": []}})),
        sse_event(json!({"type": "response.output_text.delta", "item_id": "msg_2", "output_index": 1,
            "content_index": 0, "delta": "Rust 1.99 shipped in May."})),
        sse_event(json!({"type": "response.output_item.done", "output_index": 1, "item": message})),
        sse_event(json!({"type": "response.completed", "response": {
            "id": "resp_1", "status": "completed", "model": "gpt-6.1-sol-2026-09-30", "output": [],
            "usage": {"input_tokens": 900, "input_tokens_details": {"cached_tokens": 0},
                "output_tokens": 40, "output_tokens_details": {"reasoning_tokens": 0}}}})),
    ]
    .concat()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chatgpt_chat_signs_in_and_searches_the_web_through_the_real_client() {
    let (auth, _auth_fake) = start_fake_auth().await;
    let (api, fake) = start_fake_responses().await;
    let (h, t) = signed_in().await;
    h.state.providers.set_endpoints(Endpoints {
        chatgpt_auth: auth.clone(),
        chatgpt_api: api,
        chatgpt_callback_port: 0,
        ..Endpoints::default()
    });
    sign_in_chatgpt(&h, &t).await;
    h.generator
        .agent_over_http
        .store(true, std::sync::atomic::Ordering::SeqCst);
    fake.replies.lock().unwrap().push_back(web_search_answer());
    let created = chat(
        &h,
        &t,
        json!({"provider": "chatgpt", "message": {"text": "When did Rust 1.99 ship?"}}),
    )
    .await;
    assert_eq!(created.chat.provider, Provider::ChatGpt);
    let d = wait_chat(&h, &t, &created.chat.id, |d| {
        d.chat.state == ChatState::Idle && d.items.len() > 1
    })
    .await;

    // The call went to the ChatGPT endpoint with the signed-in token, on the plan's
    // rules: one tool call per step, no output cap, the hosted web search offered.
    let (bearer, body) = fake.requests.lock().unwrap()[0].clone();
    assert_eq!(bearer.as_deref(), Some("Bearer at-1"));
    assert_eq!(body["model"], RouteModels::chatgpt_default().chat);
    assert_eq!(body["parallel_tool_calls"], false);
    assert!(body.get("max_output_tokens").is_none(), "{body}");
    assert!(
        body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["type"] == "web_search"),
        "{body}"
    );

    // The search's rows and card, then the cited answer.
    let call = d
        .items
        .iter()
        .find(|i| matches!(&i.kind, margin_shared::ChatItemKind::ToolCall { name, .. } if name == "web_search"))
        .expect("a web_search row");
    let margin_shared::ChatItemKind::ToolCall { label, .. } = &call.kind else {
        unreachable!()
    };
    assert!(label.contains("rust 1.99 release date"), "{label}");
    let card = d
        .items
        .iter()
        .find_map(|i| match &i.kind {
            margin_shared::ChatItemKind::Card {
                card: margin_shared::Card::WebResults(c),
                ..
            } => Some(c.clone()),
            _ => None,
        })
        .expect("a web results card");
    assert_eq!(card.results.len(), 2);
    let margin_shared::ChatItemKind::AssistantText { text, citations } =
        &d.items.last().unwrap().kind
    else {
        panic!("{:?}", d.items.last())
    };
    assert_eq!(text, "Rust 1.99 shipped in May.");
    assert_eq!(citations[0].url, "https://blog.rust-lang.org/1.99");

    // Traced on ChatGPT, with its tokens and the search counted, and no dollar price.
    let turn_id = d.items[0].turn_id.clone();
    let step = eventually(|| async {
        traces(&h, &t, &turn_id)
            .await
            .into_iter()
            .find(|r| r.kind == TraceKind::ModelCall && r.route == Some(Route::Chat))
    })
    .await;
    assert_eq!(step.provider.as_deref(), Some("chatgpt"));
    assert_eq!(step.model.as_deref(), Some("gpt-6.1-sol-2026-09-30"));
    assert_eq!(step.cost_usd, None, "plan-billed");
    let usage = step.usage.unwrap();
    assert_eq!((usage.input, usage.web_searches), (900, 1));
}
