//! The provider routes and state (`docs/agent-providers-brief.md` D9, D10, D13): the
//! models each provider offers, with their windows, the window cache `model_for`
//! reads, and "Sign in with ChatGPT", which runs on the service Mac (its OAuth callback
//! is a loopback URL there).
//!
//! - `GET routes::providers_models(provider)` → `Vec<ModelInfo>`; 409 `not_configured`
//!   when the provider has no credential.
//! - `POST routes::PROVIDERS_CHATGPT_START` → `ChatGptStart`: a fresh authorization URL
//!   (PKCE S256, `state`, `nonce`) and a listener on `127.0.0.1:<port>/auth/callback`
//!   for one callback or `SIGN_IN_WAIT`.
//! - `GET routes::PROVIDERS_CHATGPT` → `ChatGptStatus`.
//! - `DELETE routes::PROVIDERS_CHATGPT` → 204: signs out (revokes the refresh token at
//!   the auth server's `revocation_endpoint`, best effort, then forgets the tokens).
//!
//! Credentials stay on the service: the Anthropic key and the ChatGPT tokens are in the
//! configured secret store (`secrets::ANTHROPIC_API_KEY`, `secrets::CHATGPT_TOKENS`),
//! never in an event, a log line, a trace row or a response.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use chrono::{DateTime, Utc};
use margin_ai::{
    AiError, MODEL_WINDOW_FALLBACK, ModelInfo, ModelWindow, ProviderConfig, ProviderKind,
};
use margin_shared::{
    CLAUDE_LISTED_MODELS, ChatGptAccount, ChatGptStart, ChatGptStatus, Provider, error_code,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Notify;
use tokio::task::AbortHandle;

use crate::error::{AppError, AppResult};
use crate::secrets::CHATGPT_TOKENS;
use crate::state::{App, AppState, random_token};

/// Where ChatGPT's OAuth server is.
pub const CHATGPT_AUTH_URL: &str = "https://auth.openai.com";
/// The Responses API a ChatGPT plan token calls (and the token's `resource`).
pub const CHATGPT_API_URL: &str = "https://api.openai.com/v1";
/// The loopback port of the OAuth callback. It never changes: the registered redirect
/// URI's host, port and path must stay the same across sign-ins.
pub const CHATGPT_CALLBACK_PORT: u16 = 1455;
pub const CHATGPT_CALLBACK_PATH: &str = "/auth/callback";
/// What a ChatGPT sign-in asks for. `chatgpt.tokens.use.direct` is what lets the token
/// call the Responses API on the owner's plan; a sign-in without it fails.
pub const CHATGPT_SCOPE: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const DIRECT_SCOPE: &str = "chatgpt.tokens.use.direct";
/// `client_id` of the first sign-in: the server registers Margin and returns the
/// client id it issued in the callback.
pub const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
/// `agent_name_hint` of the first sign-in.
pub const AGENT_NAME: &str = "Margin";
/// How long the callback listener waits for the owner's browser.
pub const SIGN_IN_WAIT: Duration = Duration::from_secs(10 * 60);
/// An access token is refreshed before a call when less than this is left of it.
pub const REFRESH_MARGIN: chrono::Duration = chrono::Duration::minutes(5);
/// How long a refresh token lasts (it rotates on every refresh).
pub const REFRESH_TOKEN_LIFETIME: chrono::Duration = chrono::Duration::days(30);
/// A model list that couldn't be fetched is tried again after this.
const MODELS_RETRY: Duration = Duration::from_secs(10 * 60);
/// How long a list or token request may take.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Where the providers are. The defaults are the real services; tests point them at
/// fakes (`Providers::set_endpoints`). The OpenAI base URL is Settings'.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    /// The Messages API base ("https://api.anthropic.com").
    pub anthropic: String,
    /// The Responses API a ChatGPT token calls.
    pub chatgpt_api: String,
    /// The OAuth server ChatGPT signs in with (also the `id_token` issuer).
    pub chatgpt_auth: String,
    /// The loopback callback's port (`0` picks a free one: tests only).
    pub chatgpt_callback_port: u16,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            anthropic: margin_ai::ANTHROPIC_BASE_URL.into(),
            chatgpt_api: CHATGPT_API_URL.into(),
            chatgpt_auth: CHATGPT_AUTH_URL.into(),
            chatgpt_callback_port: CHATGPT_CALLBACK_PORT,
        }
    }
}

/// The providers' runtime state, in `App`.
pub struct Providers {
    endpoints: Mutex<Endpoints>,
    /// The ChatGPT sign-in in progress, or how the last one failed.
    flow: Mutex<Flow>,
    /// Token refreshes, one at a time.
    refresh: tokio::sync::Mutex<()>,
    /// Each provider's model list, with the windows `model_for` uses (D13).
    models: Mutex<HashMap<Provider, CachedModels>>,
    /// Models whose window is unknown, already logged once.
    warned: Mutex<HashSet<String>>,
    http: reqwest::Client,
}

enum Flow {
    Idle,
    Waiting {
        authorize_url: String,
        state: String,
        task: AbortHandle,
    },
    Failed(String),
}

struct CachedModels {
    models: Arc<Vec<ModelInfo>>,
    fetched: Instant,
    ok: bool,
}

impl Default for Providers {
    fn default() -> Self {
        Self::new()
    }
}

impl Providers {
    pub fn new() -> Self {
        Self {
            endpoints: Mutex::new(Endpoints::default()),
            flow: Mutex::new(Flow::Idle),
            refresh: tokio::sync::Mutex::new(()),
            models: Mutex::new(HashMap::new()),
            warned: Mutex::new(HashSet::new()),
            http: reqwest::Client::builder()
                .timeout(HTTP_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!("margin-service/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default(),
        }
    }

    pub fn endpoints(&self) -> Endpoints {
        self.endpoints.lock().unwrap().clone()
    }

    pub fn set_endpoints(&self, endpoints: Endpoints) {
        *self.endpoints.lock().unwrap() = endpoints;
        self.forget_models();
    }

    /// Drops every cached model list: the next `model_for` fetches again. Called
    /// whenever the settings or a credential change (`SettingsChanged`).
    pub fn forget_models(&self) {
        self.models.lock().unwrap().clear();
    }

    /// The model list cached for `provider`, when one was fetched.
    pub fn cached_models(&self, provider: Provider) -> Option<Arc<Vec<ModelInfo>>> {
        self.models
            .lock()
            .unwrap()
            .get(&provider)
            .filter(|c| c.ok)
            .map(|c| c.models.clone())
    }

    fn remember_models(&self, provider: Provider, models: Arc<Vec<ModelInfo>>, ok: bool) {
        self.models.lock().unwrap().insert(
            provider,
            CachedModels {
                models,
                fetched: Instant::now(),
                ok,
            },
        );
    }

    /// Logs a model whose window isn't known, once per model.
    fn warn_window(&self, provider: Provider, model: &str) {
        let key = format!("{provider}/{model}");
        if self.warned.lock().unwrap().insert(key) {
            tracing::warn!(
                %provider, %model,
                "the model's context window isn't known; using {} input / {} output tokens",
                MODEL_WINDOW_FALLBACK.input, MODEL_WINDOW_FALLBACK.output
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Model lists and windows (D13)
// ---------------------------------------------------------------------------

/// The window of `model` on `config`'s provider: Claude's from its model list (fetched
/// once and cached until the settings change), OpenAI's and ChatGPT's from
/// `margin_ai::windows` (their `/models` carry none). `MODEL_WINDOW_FALLBACK`, logged
/// once, when neither knows it.
pub async fn window(state: &App, config: &ProviderConfig, model: &str) -> ModelWindow {
    let known = match config.provider {
        ProviderKind::OpenAi | ProviderKind::ChatGpt => margin_ai::windows::openai_window(model),
        ProviderKind::Claude => {
            let list = cached_list(state, config).await;
            list.iter().find(|m| m.id == model).and_then(|m| {
                match (m.context_window, m.max_output_tokens) {
                    (Some(input), Some(output)) => Some(ModelWindow { input, output }),
                    (Some(input), None) => Some(ModelWindow {
                        input,
                        output: MODEL_WINDOW_FALLBACK.output.min(input),
                    }),
                    _ => None,
                }
            })
        }
    };
    known.unwrap_or_else(|| {
        state.providers.warn_window(config.provider, model);
        MODEL_WINDOW_FALLBACK
    })
}

/// `config`'s provider's model list from the cache, fetched when missing (or when the
/// last fetch failed more than `MODELS_RETRY` ago). Empty when it can't be fetched.
async fn cached_list(state: &App, config: &ProviderConfig) -> Arc<Vec<ModelInfo>> {
    {
        let cache = state.providers.models.lock().unwrap();
        if let Some(c) = cache.get(&config.provider)
            && (c.ok || c.fetched.elapsed() < MODELS_RETRY)
        {
            return c.models.clone();
        }
    }
    match fetch_models(state, config).await {
        Ok(list) => list,
        Err(e) => {
            tracing::warn!(
                provider = %config.provider,
                "couldn't list the provider's models: {e}"
            );
            let empty = Arc::new(Vec::new());
            state
                .providers
                .remember_models(config.provider, empty.clone(), false);
            empty
        }
    }
}

/// Lists `config`'s models through its client and caches the list. OpenAI and ChatGPT
/// ids the client gave no window get `margin_ai::windows`' when it knows them; Claude's
/// defaults are listed first.
async fn fetch_models(
    state: &App,
    config: &ProviderConfig,
) -> Result<Arc<Vec<ModelInfo>>, AiError> {
    let client = state.generator.agent_model(config);
    let mut list = client.list_models().await?;
    if config.provider != ProviderKind::Claude {
        for m in &mut list {
            if m.context_window.is_none()
                && let Some(w) = margin_ai::windows::openai_window(&m.id)
            {
                m.context_window = Some(w.input);
                m.max_output_tokens = m.max_output_tokens.or(Some(w.output));
            }
        }
    }
    list.sort_by_key(|m| {
        CLAUDE_LISTED_MODELS
            .iter()
            .position(|id| *id == m.id)
            .unwrap_or(CLAUDE_LISTED_MODELS.len())
    });
    let list = Arc::new(list);
    state
        .providers
        .remember_models(config.provider, list.clone(), true);
    Ok(list)
}

/// Whether a pipeline route override `id` may apply to `provider`'s slot: it is in the
/// provider's cached model list, or, before one is cached, it looks like the provider's
/// (Claude ids start with `claude`, OpenAI's and ChatGPT's don't). An override never
/// crosses providers.
pub fn override_applies(state: &App, provider: Provider, id: &str) -> bool {
    match state.providers.cached_models(provider) {
        Some(list) if !list.is_empty() => list.iter().any(|m| m.id == id),
        _ => (provider == Provider::Claude) == id.starts_with("claude"),
    }
}

/// `GET /api/providers/{provider}/models`.
pub async fn models(
    State(state): State<AppState>,
    Path(provider): Path<Provider>,
) -> AppResult<Json<Vec<ModelInfo>>> {
    let config = match state.provider_config(provider).await {
        Ok(c) => c,
        Err(e) if matches!(e.code, error_code::NO_API_KEY | error_code::AUTH_FAILED) => {
            return Err(AppError::conflict(
                "not_configured",
                format!(
                    "{} isn't set up yet: {}",
                    provider.label(),
                    e.message.trim_end_matches('.')
                ) + ".",
            ));
        }
        Err(e) => return Err(e),
    };
    match fetch_models(&state, &config).await {
        Ok(list) => Ok(Json(list.as_ref().clone())),
        Err(e) => Err(model_list_error(provider, &e)),
    }
}

fn model_list_error(provider: Provider, e: &AiError) -> AppError {
    let code = match e {
        AiError::Http {
            status: 401..=403, ..
        } => error_code::AUTH_FAILED,
        AiError::Http { status: 429, .. } => error_code::RATE_LIMITED,
        AiError::Network(_) => error_code::NETWORK,
        _ => error_code::SERVER_ERROR,
    };
    AppError::new(
        StatusCode::BAD_GATEWAY,
        code,
        format!("{} didn't list its models: {e}", provider.label()),
    )
}

// ---------------------------------------------------------------------------
// ChatGPT tokens
// ---------------------------------------------------------------------------

/// What `secrets::CHATGPT_TOKENS` holds (JSON). The client id and the host id outlive a
/// sign-out (the next sign-in reuses the registration); the tokens don't. Its `Debug`
/// prints no token.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatGptTokens {
    /// The client id the auth server issued at the first sign-in.
    #[serde(default)]
    pub client_id: Option<String>,
    /// `urn:uuid:<v4>`, generated once.
    #[serde(default)]
    pub ext_agent_host_id: String,
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
    /// When the access token runs out.
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    /// Not refreshed before this (the token response's `earliest_refresh_at`).
    #[serde(default)]
    pub earliest_refresh_at: Option<DateTime<Utc>>,
    /// When the refresh token runs out.
    #[serde(default)]
    pub refresh_expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub sub: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub signed_in_at: Option<DateTime<Utc>>,
}

impl std::fmt::Debug for ChatGptTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatGptTokens")
            .field("signed_in", &self.is_signed_in())
            .field("expires_at", &self.expires_at)
            .field("scopes", &self.scopes)
            .finish_non_exhaustive()
    }
}

impl ChatGptTokens {
    pub fn is_signed_in(&self) -> bool {
        self.refresh_token.is_some() && self.access_token.is_some()
    }

    /// The account as the app sees it, when signed in.
    pub fn account(&self) -> Option<ChatGptAccount> {
        self.is_signed_in().then(|| ChatGptAccount {
            email: self.email.clone().unwrap_or_default(),
            signed_in_at: self.signed_in_at.unwrap_or_else(Utc::now),
            expires_at: self.refresh_expires_at,
        })
    }

    /// Forgets the tokens, keeping the registration (client id, host id) and the
    /// account's email (the next sign-in's `login_hint`).
    fn signed_out(&self) -> Self {
        Self {
            client_id: self.client_id.clone(),
            ext_agent_host_id: self.ext_agent_host_id.clone(),
            email: self.email.clone(),
            ..Default::default()
        }
    }
}

pub async fn load_tokens(state: &App) -> anyhow::Result<ChatGptTokens> {
    Ok(match state.secret_get(CHATGPT_TOKENS).await? {
        Some(json) => serde_json::from_str(&json).unwrap_or_else(|_| {
            tracing::warn!("the stored ChatGPT sign-in can't be read; signed out");
            ChatGptTokens::default()
        }),
        None => ChatGptTokens::default(),
    })
}

async fn save_tokens(state: &App, tokens: &ChatGptTokens) -> anyhow::Result<()> {
    state
        .secret_set(CHATGPT_TOKENS, serde_json::to_string(tokens)?)
        .await
}

/// The ChatGPT account Margin is signed in with, if any.
pub async fn chatgpt_account(state: &App) -> anyhow::Result<Option<ChatGptAccount>> {
    Ok(load_tokens(state).await?.account())
}

fn signed_out_error() -> AppError {
    AppError::new(
        StatusCode::BAD_REQUEST,
        error_code::AUTH_FAILED,
        "Sign in with ChatGPT in Settings → AI first.",
    )
}

/// A ChatGPT access token to call with: the stored one, refreshed first when less than
/// `REFRESH_MARGIN` is left (and `earliest_refresh_at` has passed), or always when
/// `force` (the API refused the token). A refresh the server refuses for good signs
/// ChatGPT out (`SettingsChanged`) and is `auth_failed`; one that fails for a while
/// (network, 5xx) keeps the tokens and uses the old access token while it lasts.
pub async fn chatgpt_access_token(state: &App, force: bool) -> AppResult<String> {
    let _one = state.providers.refresh.lock().await;
    let tokens = load_tokens(state).await?;
    let (Some(access), Some(refresh)) = (tokens.access_token.clone(), tokens.refresh_token.clone())
    else {
        return Err(signed_out_error());
    };
    let now = Utc::now();
    let fresh = tokens
        .expires_at
        .is_some_and(|at| at - now > REFRESH_MARGIN);
    let too_early = tokens.earliest_refresh_at.is_some_and(|at| now < at);
    if !force && (fresh || too_early) {
        return Ok(access);
    }
    let endpoints = state.providers.endpoints();
    let client_id = tokens.client_id.clone().unwrap_or_default();
    let form = [
        ("grant_type", "refresh_token"),
        ("client_id", client_id.as_str()),
        ("refresh_token", refresh.as_str()),
        ("resource", CHATGPT_API_URL),
    ];
    let sent = state
        .providers
        .http
        .post(token_url(&endpoints))
        .form(&form)
        .send()
        .await;
    let resp = match sent {
        Ok(r) => r,
        Err(e) => return refresh_unavailable(&tokens, &access, &format!("{e}")),
    };
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if status.is_success() {
        let parsed: TokenResponse = serde_json::from_value(body)
            .map_err(|e| AppError::internal(format!("the token refresh answer: {e}")))?;
        let mut next = tokens.clone();
        apply_tokens(&mut next, &parsed, now);
        save_tokens(state, &next).await?;
        tracing::info!("the ChatGPT access token was refreshed");
        return Ok(next.access_token.unwrap_or(access));
    }
    let error = body["error"].as_str().unwrap_or_default().to_string();
    let terminal = matches!(status.as_u16(), 400 | 401)
        && (matches!(error.as_str(), "invalid_grant" | "token_expired")
            || error.starts_with("refresh_token"));
    if !terminal {
        return refresh_unavailable(&tokens, &access, &format!("HTTP {status} {error}"));
    }
    tracing::warn!(%error, "ChatGPT refused the refresh token; signed out");
    save_tokens(state, &tokens.signed_out()).await?;
    state.providers.forget_models();
    crate::settings::emit_settings(state).await;
    Err(AppError::new(
        StatusCode::BAD_REQUEST,
        error_code::AUTH_FAILED,
        "The ChatGPT sign-in ran out. Sign in with ChatGPT again in Settings → AI.",
    ))
}

/// A refresh that failed for now: the old access token while it lasts.
fn refresh_unavailable(tokens: &ChatGptTokens, access: &str, why: &str) -> AppResult<String> {
    tracing::warn!("couldn't refresh the ChatGPT token: {why}");
    if tokens.expires_at.is_some_and(|at| at > Utc::now()) {
        return Ok(access.to_string());
    }
    Err(AppError::new(
        StatusCode::BAD_GATEWAY,
        error_code::NETWORK,
        "Couldn't reach ChatGPT's sign-in server to renew the sign-in. Try again in a moment.",
    ))
}

/// A token endpoint's answer (authorization code or refresh).
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    earliest_refresh_at: Option<Value>,
    #[serde(default)]
    scope: Option<String>,
}

/// `earliest_refresh_at` as unix seconds or an RFC 3339 time.
fn parse_time(v: &Value) -> Option<DateTime<Utc>> {
    match v {
        Value::Number(n) => DateTime::from_timestamp(n.as_i64()?, 0),
        Value::String(s) => s.parse::<DateTime<Utc>>().ok().or_else(|| {
            s.parse::<i64>()
                .ok()
                .and_then(|n| DateTime::from_timestamp(n, 0))
        }),
        _ => None,
    }
}

/// Stores a token response's tokens (a refresh token rotates when a new one is sent).
fn apply_tokens(tokens: &mut ChatGptTokens, r: &TokenResponse, now: DateTime<Utc>) {
    tokens.access_token = Some(r.access_token.clone());
    if let Some(refresh) = &r.refresh_token {
        tokens.refresh_token = Some(refresh.clone());
        tokens.refresh_expires_at = Some(now + REFRESH_TOKEN_LIFETIME);
    }
    if let Some(id) = &r.id_token {
        tokens.id_token = Some(id.clone());
    }
    tokens.expires_at = Some(now + chrono::Duration::seconds(r.expires_in.unwrap_or(3600)));
    tokens.earliest_refresh_at = r.earliest_refresh_at.as_ref().and_then(parse_time);
    if let Some(scope) = &r.scope {
        tokens.scopes = scope.split_whitespace().map(str::to_string).collect();
    }
}

fn token_url(e: &Endpoints) -> String {
    format!(
        "{}/api/accounts/oauth/token",
        e.chatgpt_auth.trim_end_matches('/')
    )
}

fn authorize_url(e: &Endpoints) -> String {
    format!(
        "{}/api/accounts/authorize",
        e.chatgpt_auth.trim_end_matches('/')
    )
}

fn revoke_url(e: &Endpoints) -> String {
    format!(
        "{}/api/accounts/oauth/revoke",
        e.chatgpt_auth.trim_end_matches('/')
    )
}

fn jwks_url(e: &Endpoints) -> String {
    format!(
        "{}/.well-known/jwks.json",
        e.chatgpt_auth.trim_end_matches('/')
    )
}

// ---------------------------------------------------------------------------
// Sign in with ChatGPT
// ---------------------------------------------------------------------------

/// One sign-in's secrets, held by its callback listener.
struct Callback {
    app: AppState,
    state: String,
    nonce: String,
    verifier: String,
    redirect_uri: String,
    /// The registered client id, for a sign-in after the first.
    client_id: Option<String>,
    done: Notify,
}

/// PKCE: a random verifier and its S256 challenge.
fn pkce() -> (String, String) {
    use base64::Engine;
    use sha2::Digest;
    let verifier = random_token();
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// `POST /api/providers/chatgpt/start`.
pub async fn chatgpt_start(State(state): State<AppState>) -> AppResult<Json<ChatGptStart>> {
    let endpoints = state.providers.endpoints();
    let mut tokens = load_tokens(&state).await?;
    if tokens.ext_agent_host_id.is_empty() {
        tokens.ext_agent_host_id = format!("urn:uuid:{}", uuid::Uuid::new_v4());
        save_tokens(&state, &tokens).await?;
    }
    // A sign-in already waiting is replaced (its listener closes, freeing the port).
    if let Flow::Waiting { task, .. } =
        std::mem::replace(&mut *state.providers.flow.lock().unwrap(), Flow::Idle)
    {
        task.abort();
    }
    let listener = bind_callback(endpoints.chatgpt_callback_port).await?;
    let port = listener.local_addr().map_err(AppError::internal)?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}{CHATGPT_CALLBACK_PATH}");
    let (verifier, challenge) = pkce();
    let (flow_state, nonce) = (random_token(), random_token());

    let mut url = reqwest::Url::parse(&authorize_url(&endpoints))
        .map_err(|e| AppError::internal(format!("the ChatGPT auth URL: {e}")))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair(
                "client_id",
                tokens.client_id.as_deref().unwrap_or(DYNAMIC_CLIENT_ID),
            )
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("scope", CHATGPT_SCOPE)
            .append_pair("resource", CHATGPT_API_URL)
            .append_pair("state", &flow_state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("ext_agent_host_id", &tokens.ext_agent_host_id);
        if tokens.client_id.is_none() {
            q.append_pair("agent_name_hint", AGENT_NAME);
        } else {
            if let Some(hint) = &tokens.id_token {
                q.append_pair("id_token_hint", hint);
            }
            if let Some(email) = &tokens.email {
                q.append_pair("login_hint", email);
            }
        }
    }
    let authorize_url = url.to_string();

    let cb = Arc::new(Callback {
        app: state.clone(),
        state: flow_state.clone(),
        nonce,
        verifier,
        redirect_uri,
        client_id: tokens.client_id.clone(),
        done: Notify::new(),
    });
    let task = tokio::spawn(serve_callback(listener, cb));
    *state.providers.flow.lock().unwrap() = Flow::Waiting {
        authorize_url: authorize_url.clone(),
        state: flow_state.clone(),
        task: task.abort_handle(),
    };
    tracing::info!(%port, "ChatGPT sign-in started; waiting for the browser on the loopback callback");
    Ok(Json(ChatGptStart {
        authorize_url,
        state: flow_state,
    }))
}

async fn bind_callback(port: u16) -> AppResult<tokio::net::TcpListener> {
    // A listener just aborted may take a moment to let go of the port.
    let mut last = None;
    for _ in 0..20 {
        match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => return Ok(l),
            Err(e) => last = Some(e),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let e = last.map(|e| e.to_string()).unwrap_or_default();
    Err(AppError::conflict(
        "port_in_use",
        format!(
            "Margin can't listen for the ChatGPT sign-in on 127.0.0.1:{port} ({e}). Close what uses that port on the service Mac and try again."
        ),
    ))
}

/// Serves the loopback callback until one callback of this sign-in arrives or
/// `SIGN_IN_WAIT` passes.
async fn serve_callback(listener: tokio::net::TcpListener, cb: Arc<Callback>) {
    let router = axum::Router::new()
        .route(CHATGPT_CALLBACK_PATH, axum::routing::get(callback))
        .with_state(cb.clone());
    let done = cb.clone();
    let served = axum::serve(listener, router)
        .with_graceful_shutdown(async move { done.done.notified().await });
    if tokio::time::timeout(SIGN_IN_WAIT, served).await.is_err() {
        set_failed(
            &cb.app,
            &cb.state,
            "The sign-in wasn't finished within 10 minutes. Try again.",
        );
    }
}

/// Sets the flow's outcome, unless another sign-in replaced it meanwhile.
fn set_flow(app: &App, flow_state: &str, next: Flow) {
    let mut flow = app.providers.flow.lock().unwrap();
    if matches!(&*flow, Flow::Waiting { state, .. } if state == flow_state) {
        *flow = next;
    }
}

fn set_failed(app: &App, flow_state: &str, message: &str) {
    tracing::warn!("ChatGPT sign-in failed: {message}");
    set_flow(app, flow_state, Flow::Failed(message.to_string()));
}

/// `GET http://127.0.0.1:<port>/auth/callback?code&state[&client_id][&scope]` from the
/// owner's browser.
async fn callback(
    State(cb): State<Arc<Callback>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if q.get("state").map(String::as_str) != Some(cb.state.as_str()) {
        // Not this sign-in (an old tab): keep waiting for the right one.
        return page(
            StatusCode::BAD_REQUEST,
            "This sign-in link is out of date",
            "Start again from Margin's Settings → AI → ChatGPT.",
        );
    }
    let outcome = match (q.get("error"), q.get("code")) {
        (Some(error), _) => Err(q
            .get("error_description")
            .cloned()
            .unwrap_or_else(|| format!("ChatGPT answered {error}."))),
        (None, Some(code)) => {
            complete(
                &cb,
                code,
                q.get("client_id").cloned(),
                q.get("scope").cloned(),
            )
            .await
        }
        (None, None) => Err("The sign-in came back without a code.".to_string()),
    };
    let response = match outcome {
        Ok(account) => {
            set_flow(&cb.app, &cb.state, Flow::Idle);
            cb.app.providers.forget_models();
            crate::settings::emit_settings(&cb.app).await;
            tracing::info!("signed in with ChatGPT");
            page(
                StatusCode::OK,
                "Margin is signed in with ChatGPT",
                &format!(
                    "Signed in as {}. You can close this tab.",
                    html_escape(&account.email)
                ),
            )
        }
        Err(message) => {
            set_failed(&cb.app, &cb.state, &message);
            page(
                StatusCode::BAD_REQUEST,
                "The ChatGPT sign-in didn't work",
                &format!(
                    "{} Try again from Margin's Settings.",
                    html_escape(&message)
                ),
            )
        }
    };
    cb.done.notify_one();
    response
}

fn page(status: StatusCode, title: &str, text: &str) -> Response {
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>{title}</title>\
         <body style=\"font-family:-apple-system,system-ui,sans-serif;margin:3em\">\
         <h1 style=\"font-size:1.4em\">{title}</h1><p>{text}</p></body>"
    );
    (status, [(header::CONNECTION, "close")], Html(body)).into_response()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The authorization code → tokens, the `id_token` checked, the tokens stored.
async fn complete(
    cb: &Callback,
    code: &str,
    issued_client_id: Option<String>,
    callback_scope: Option<String>,
) -> Result<ChatGptAccount, String> {
    let app = &cb.app;
    let endpoints = app.providers.endpoints();
    let client_id = issued_client_id
        .or_else(|| cb.client_id.clone())
        .ok_or("ChatGPT didn't say which client id it registered Margin as.")?;
    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", client_id.as_str()),
        ("code", code),
        ("code_verifier", cb.verifier.as_str()),
        ("redirect_uri", cb.redirect_uri.as_str()),
        ("resource", CHATGPT_API_URL),
    ];
    let resp = app
        .providers
        .http
        .post(token_url(&endpoints))
        .form(&form)
        .send()
        .await
        .map_err(|e| format!("Couldn't reach ChatGPT's sign-in server ({e})."))?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let why = body["error_description"]
            .as_str()
            .or(body["error"].as_str())
            .unwrap_or("no reason given");
        return Err(format!(
            "ChatGPT refused the sign-in (HTTP {status}: {why})."
        ));
    }
    let parsed: TokenResponse = serde_json::from_value(body)
        .map_err(|e| format!("ChatGPT's token answer can't be read ({e})."))?;
    let scope = parsed.scope.clone().or(callback_scope).unwrap_or_default();
    if !scope.split_whitespace().any(|s| s == DIRECT_SCOPE) {
        return Err(
            "ChatGPT didn't let Margin use your plan (the chatgpt.tokens.use.direct permission wasn't granted)."
                .into(),
        );
    }
    if parsed.refresh_token.is_none() {
        return Err("ChatGPT didn't send a refresh token.".into());
    }
    let id_token = parsed
        .id_token
        .clone()
        .ok_or("ChatGPT didn't send an id_token.")?;
    let claims = validate_id_token(
        &app.providers.http,
        &endpoints,
        &client_id,
        &cb.nonce,
        &id_token,
    )
    .await?;

    let now = Utc::now();
    let mut tokens = load_tokens(app)
        .await
        .map_err(|e| format!("The sign-in couldn't be stored ({e:#})."))?;
    tokens.client_id = Some(client_id);
    apply_tokens(&mut tokens, &parsed, now);
    tokens.scopes = scope.split_whitespace().map(str::to_string).collect();
    tokens.sub = Some(claims.sub);
    tokens.email = claims.email;
    tokens.signed_in_at = Some(now);
    save_tokens(app, &tokens)
        .await
        .map_err(|e| format!("The sign-in couldn't be stored ({e:#})."))?;
    tokens
        .account()
        .ok_or_else(|| "The sign-in couldn't be stored.".to_string())
}

#[derive(Debug, Deserialize)]
struct IdClaims {
    sub: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    nonce: Option<String>,
}

/// Checks an `id_token`: RS256, signed by a key in the auth server's JWKS, issued by
/// the auth server, for `client_id`, unexpired, with this sign-in's nonce.
async fn validate_id_token(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    client_id: &str,
    nonce: &str,
    id_token: &str,
) -> Result<IdClaims, String> {
    use jsonwebtoken::jwk::JwkSet;
    use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
    let bad = |what: &str| format!("ChatGPT's id_token isn't valid ({what}).");
    let header = decode_header(id_token).map_err(|e| bad(&e.to_string()))?;
    if header.alg != Algorithm::RS256 {
        return Err(bad("not signed with RS256"));
    }
    let jwks: JwkSet = http
        .get(jwks_url(endpoints))
        .send()
        .await
        .map_err(|e| format!("Couldn't fetch ChatGPT's signing keys ({e})."))?
        .json()
        .await
        .map_err(|e| format!("ChatGPT's signing keys can't be read ({e})."))?;
    let jwk = match &header.kid {
        Some(kid) => jwks.find(kid),
        None if jwks.keys.len() == 1 => jwks.keys.first(),
        None => None,
    }
    .ok_or_else(|| bad("signed with an unknown key"))?;
    let key = DecodingKey::from_jwk(jwk).map_err(|e| bad(&e.to_string()))?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[endpoints.chatgpt_auth.trim_end_matches('/')]);
    validation.set_audience(&[client_id]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    let data = decode::<IdClaims>(id_token, &key, &validation).map_err(|e| bad(&e.to_string()))?;
    if data.claims.nonce.as_deref() != Some(nonce) {
        return Err(bad("the nonce doesn't match"));
    }
    Ok(data.claims)
}

/// `GET /api/providers/chatgpt`.
pub async fn chatgpt_status(State(state): State<AppState>) -> AppResult<Json<ChatGptStatus>> {
    let pending = match &*state.providers.flow.lock().unwrap() {
        Flow::Waiting { authorize_url, .. } => Some(ChatGptStatus::Waiting {
            authorize_url: authorize_url.clone(),
        }),
        Flow::Failed(message) => Some(ChatGptStatus::Failed {
            message: message.clone(),
        }),
        Flow::Idle => None,
    };
    if let Some(status) = pending {
        return Ok(Json(status));
    }
    Ok(Json(match chatgpt_account(&state).await? {
        Some(account) => ChatGptStatus::SignedIn { account },
        None => ChatGptStatus::SignedOut,
    }))
}

/// `DELETE /api/providers/chatgpt`: revokes the refresh token (best effort) and
/// forgets the tokens; a sign-in in progress is cancelled.
pub async fn chatgpt_sign_out(State(state): State<AppState>) -> AppResult<StatusCode> {
    if let Flow::Waiting { task, .. } =
        std::mem::replace(&mut *state.providers.flow.lock().unwrap(), Flow::Idle)
    {
        task.abort();
    }
    *state.providers.flow.lock().unwrap() = Flow::Idle;
    let _one = state.providers.refresh.lock().await;
    let tokens = load_tokens(&state).await?;
    if let Some(refresh) = &tokens.refresh_token {
        let endpoints = state.providers.endpoints();
        let client_id = tokens.client_id.clone().unwrap_or_default();
        let form = [
            ("token", refresh.as_str()),
            ("token_type_hint", "refresh_token"),
            ("client_id", client_id.as_str()),
        ];
        match state
            .providers
            .http
            .post(revoke_url(&endpoints))
            .form(&form)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => tracing::warn!("ChatGPT didn't revoke the sign-in: HTTP {}", r.status()),
            Err(e) => tracing::warn!("couldn't reach ChatGPT to revoke the sign-in: {e}"),
        }
    }
    save_tokens(&state, &tokens.signed_out()).await?;
    state.providers.forget_models();
    crate::settings::emit_settings(&state).await;
    tracing::info!("signed out of ChatGPT");
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_never_print_and_sign_out_keeps_the_registration() {
        let t = ChatGptTokens {
            client_id: Some("client-1".into()),
            ext_agent_host_id: "urn:uuid:x".into(),
            access_token: Some("secret-access".into()),
            refresh_token: Some("secret-refresh".into()),
            id_token: Some("secret-id".into()),
            email: Some("a@b.c".into()),
            ..Default::default()
        };
        let printed = format!("{t:?}");
        assert!(!printed.contains("secret"), "{printed}");
        assert!(t.is_signed_in());
        assert_eq!(t.account().unwrap().email, "a@b.c");
        let out = t.signed_out();
        assert!(!out.is_signed_in() && out.account().is_none());
        assert_eq!(out.client_id.as_deref(), Some("client-1"));
        assert_eq!(out.ext_agent_host_id, "urn:uuid:x");
    }

    #[test]
    fn token_responses_rotate_and_read_both_time_shapes() {
        let now = Utc::now();
        let mut t = ChatGptTokens {
            refresh_token: Some("r1".into()),
            ..Default::default()
        };
        let r: TokenResponse = serde_json::from_value(serde_json::json!({
            "access_token": "a2", "expires_in": 3600, "earliest_refresh_at": 2_000_000_000,
            "scope": "openid chatgpt.tokens.use.direct"
        }))
        .unwrap();
        apply_tokens(&mut t, &r, now);
        assert_eq!(
            t.refresh_token.as_deref(),
            Some("r1"),
            "kept without a new one"
        );
        assert_eq!(t.earliest_refresh_at.unwrap().timestamp(), 2_000_000_000);
        assert_eq!(t.scopes.len(), 2);
        let r: TokenResponse = serde_json::from_value(serde_json::json!({
            "access_token": "a3", "refresh_token": "r2",
            "earliest_refresh_at": "2030-01-01T00:00:00Z"
        }))
        .unwrap();
        apply_tokens(&mut t, &r, now);
        assert_eq!(t.refresh_token.as_deref(), Some("r2"), "rotated");
        assert_eq!(t.refresh_expires_at, Some(now + REFRESH_TOKEN_LIFETIME));
        assert_eq!(
            t.earliest_refresh_at.unwrap().to_rfc3339(),
            "2030-01-01T00:00:00+00:00"
        );
    }

    #[test]
    fn pkce_challenge_is_s256_of_the_verifier() {
        use base64::Engine;
        use sha2::Digest;
        let (v, c) = pkce();
        let expect = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(sha2::Sha256::digest(v.as_bytes()));
        assert_eq!(c, expect);
        assert!(v.len() >= 43);
    }
}
