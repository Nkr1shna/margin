//! The provider routes and state (`docs/agent-providers-brief.md` D9, D10, D13): the
//! models each provider offers, with their windows, the window cache `model_for`
//! reads, and "Sign in with ChatGPT".
//!
//! ChatGPT's sign-in only redirects to a loopback URL (`http://127.0.0.1:<port>/auth/callback`,
//! only the port may vary), and that URL reaches the device whose browser signs in, not
//! the service (a headless VM behind a tunnel). So the service runs the OAuth flow's
//! secret half and the person's device carries the code back (`docs/agent-siwc-spike.md`,
//! "Signing in when the service is remote"):
//!
//! - `GET routes::providers_models(provider)` → `Vec<ModelInfo>`; 409 `not_configured`
//!   when the provider has no credential.
//! - `POST routes::PROVIDERS_CHATGPT_START` (`ChatGptStartRequest`) → `ChatGptStart`: a
//!   fresh authorization URL (PKCE S256, `state`, `nonce`) whose redirect is the app's
//!   loopback port. The verifier, state and nonce stay here, for one completion within
//!   `SIGN_IN_WAIT`.
//! - `POST routes::PROVIDERS_CHATGPT_COMPLETE` (`ChatGptComplete`) → `ChatGptStatus`:
//!   the callback's address (code and state), forwarded by the desktop app's loopback
//!   listener or pasted by the person; checked against the waiting sign-in's state,
//!   used once, the code exchanged here with the verifier, the tokens stored here.
//! - `GET routes::PROVIDERS_CHATGPT` → `ChatGptStatus`.
//! - `DELETE routes::PROVIDERS_CHATGPT` → 204: signs out (revokes the refresh token at
//!   the auth server's `revocation_endpoint`, best effort, then forgets the tokens).
//!
//! Credentials stay on the service: the Anthropic key and the ChatGPT tokens are in the
//! configured secret store (`secrets::ANTHROPIC_API_KEY`, `secrets::CHATGPT_TOKENS`),
//! never in an event, a log line, a trace row or a response. The authorization code
//! and the PKCE verifier are never logged either.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use margin_ai::{
    AiError, MODEL_WINDOW_FALLBACK, ModelInfo, ModelWindow, ProviderConfig, ProviderKind,
};
use margin_shared::sync::Locked;
pub use margin_shared::{CHATGPT_CALLBACK_PATH, CHATGPT_CALLBACK_PORT};
use margin_shared::{
    CLAUDE_LISTED_MODELS, ChatGptAccount, ChatGptComplete, ChatGptStart, ChatGptStartRequest,
    ChatGptStatus, Provider, error_code,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{AppError, AppResult};
use crate::principal::RequireAdmin;
use crate::secrets::CHATGPT_TOKENS;
use crate::state::{App, AppState, random_token};

/// Where ChatGPT's OAuth server is.
pub const CHATGPT_AUTH_URL: &str = "https://auth.openai.com";
/// The Responses API a ChatGPT plan token calls (and the token's `resource`).
pub const CHATGPT_API_URL: &str = "https://api.openai.com/v1";
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
/// How long a started sign-in may be completed (its state and verifier are forgotten
/// after).
pub const SIGN_IN_WAIT: Duration = Duration::from_secs(10 * 60);
/// The longest callback address `complete` reads.
const CALLBACK_MAX_LEN: usize = 8 * 1024;
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
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            anthropic: margin_ai::ANTHROPIC_BASE_URL.into(),
            chatgpt_api: CHATGPT_API_URL.into(),
            chatgpt_auth: CHATGPT_AUTH_URL.into(),
        }
    }
}

/// The providers' runtime state, in `App`.
pub struct Providers {
    endpoints: Mutex<Endpoints>,
    /// The ChatGPT sign-in in progress, or how the last one failed.
    flow: Mutex<Flow>,
    /// How long a started sign-in may be completed (`SIGN_IN_WAIT`; tests shorten it).
    sign_in_wait: Mutex<Duration>,
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
    /// Started: waiting for the callback's address.
    Waiting(Box<Pending>),
    /// A completion took this sign-in (it is used once) and is exchanging its code.
    Exchanging {
        authorize_url: String,
        state: String,
    },
    Failed(String),
}

/// One started sign-in's secrets. They never leave the service.
struct Pending {
    authorize_url: String,
    state: String,
    nonce: String,
    verifier: String,
    redirect_uri: String,
    /// The registered client id, for a sign-in after the first.
    client_id: Option<String>,
    started: Instant,
}

impl Flow {
    /// The state of the sign-in in progress (waiting or exchanging), if any.
    fn state(&self) -> Option<&str> {
        match self {
            Flow::Waiting(p) => Some(&p.state),
            Flow::Exchanging { state, .. } => Some(state),
            Flow::Idle | Flow::Failed(_) => None,
        }
    }
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
            sign_in_wait: Mutex::new(SIGN_IN_WAIT),
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
        self.endpoints.locked().clone()
    }

    pub fn set_endpoints(&self, endpoints: Endpoints) {
        *self.endpoints.locked() = endpoints;
        self.forget_models();
    }

    /// How long a started ChatGPT sign-in may be completed (tests shorten it).
    pub fn set_sign_in_wait(&self, wait: Duration) {
        *self.sign_in_wait.locked() = wait;
    }

    /// The flow, with a waiting sign-in that ran out of time turned `Failed` (its
    /// secrets dropped).
    fn flow(&self) -> std::sync::MutexGuard<'_, Flow> {
        let wait = *self.sign_in_wait.locked();
        let mut flow = self.flow.locked();
        if matches!(&*flow, Flow::Waiting(p) if p.started.elapsed() >= wait) {
            tracing::info!("a ChatGPT sign-in wasn't finished in time");
            *flow = Flow::Failed(format!(
                "The sign-in wasn't finished within {} minutes. Try again.",
                wait.as_secs().div_ceil(60)
            ));
        }
        flow
    }

    /// Drops every cached model list: the next `model_for` fetches again. Called
    /// whenever the settings or a credential change (`SettingsChanged`).
    pub fn forget_models(&self) {
        self.models.locked().clear();
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
        self.models.locked().insert(
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
        if self.warned.locked().insert(key) {
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
        let cache = state.providers.models.locked();
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
/// `REFRESH_MARGIN` is left (and `earliest_refresh_at` has passed). `refused` is the
/// access token the API just refused (a forced refresh): under the refresh lock, a
/// stored token other than it is already newer and is returned as it is; else it is
/// refreshed, but never before `earliest_refresh_at`. A refresh the server refuses for
/// good signs ChatGPT out (`SettingsChanged`) and is `auth_failed`; one that fails for a
/// while (network, 5xx) keeps the tokens and uses the old access token while it lasts.
pub async fn chatgpt_access_token(state: &App, refused: Option<&str>) -> AppResult<String> {
    let _one = state.providers.refresh.lock().await;
    let tokens = load_tokens(state).await?;
    let (Some(access), Some(refresh)) = (tokens.access_token.clone(), tokens.refresh_token.clone())
    else {
        return Err(signed_out_error());
    };
    let now = Utc::now();
    let too_early = tokens.earliest_refresh_at.is_some_and(|at| now < at);
    match refused {
        // Another call refreshed (or the owner signed in again) meanwhile.
        Some(refused) if refused != access => return Ok(access),
        Some(_) if too_early => {
            tracing::warn!("ChatGPT refused the access token before it may be refreshed");
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                error_code::AUTH_FAILED,
                "ChatGPT refused the sign-in's access token, and it can't be renewed yet. Try again in a few minutes.",
            ));
        }
        Some(_) => {}
        None => {
            let fresh = tokens
                .expires_at
                .is_some_and(|at| at - now > REFRESH_MARGIN);
            if fresh || too_early {
                return Ok(access);
            }
        }
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

/// PKCE: a random verifier and its S256 challenge.
fn pkce() -> (String, String) {
    use base64::Engine;
    use sha2::Digest;
    let verifier = random_token();
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// `POST /api/providers/chatgpt/start`, with an optional `ChatGptStartRequest` (an
/// empty body asks for `CHATGPT_CALLBACK_PORT`). The ChatGPT sign-in is the
/// organisation's (one set of provider credentials, plan §3): admins only (403
/// `admin_required`). A sign-in already waiting is replaced.
pub async fn chatgpt_start(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    body: Bytes,
) -> AppResult<Json<ChatGptStart>> {
    let request: ChatGptStartRequest = if body.iter().all(u8::is_ascii_whitespace) {
        ChatGptStartRequest::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| AppError::bad_request(format!("The request can't be read: {e}")))?
    };
    let port = request.callback_port.unwrap_or(CHATGPT_CALLBACK_PORT);
    if port == 0 {
        return Err(AppError::bad_request(
            "callback_port must be the port the app listens on.",
        ));
    }
    let endpoints = state.providers.endpoints();
    let mut tokens = load_tokens(&state).await?;
    if tokens.ext_agent_host_id.is_empty() {
        // The service's own host id: the tokens live and refresh here, whichever
        // device's browser signs in.
        tokens.ext_agent_host_id = format!("urn:uuid:{}", uuid::Uuid::new_v4());
        save_tokens(&state, &tokens).await?;
    }
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
        } else if let Some(email) = &tokens.email {
            // No `id_token_hint`: this URL goes to devices and the clipboard.
            q.append_pair("login_hint", email);
        }
    }
    let authorize_url = url.to_string();

    *state.providers.flow.locked() = Flow::Waiting(Box::new(Pending {
        authorize_url: authorize_url.clone(),
        state: flow_state.clone(),
        nonce,
        verifier,
        redirect_uri: redirect_uri.clone(),
        client_id: tokens.client_id.clone(),
        started: Instant::now(),
    }));
    tracing::info!(%port, "ChatGPT sign-in started; waiting for the callback from the person's device");
    Ok(Json(ChatGptStart {
        authorize_url,
        state: flow_state,
        redirect_uri,
    }))
}

/// Sets the flow's outcome, unless a sign-out or another sign-in replaced it meanwhile.
fn set_flow(app: &App, flow_state: &str, next: Flow) {
    let mut flow = app.providers.flow.locked();
    if flow.state() == Some(flow_state) {
        *flow = next;
    }
}

fn set_failed(app: &App, flow_state: &str, message: &str) {
    // A pasted `error_description` lands here verbatim: one line, bounded, in the log.
    let logged: String = message
        .chars()
        .take(200)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    tracing::warn!("ChatGPT sign-in failed: {logged}");
    set_flow(app, flow_state, Flow::Failed(message.to_string()));
}

/// The query of a callback address: `http://127.0.0.1:<port>/auth/callback?…`, or the
/// query alone (with or without its `?`). Never logged: it holds the code.
fn callback_query(text: &str) -> AppResult<HashMap<String, String>> {
    let text = text.trim();
    if text.is_empty() || text.len() > CALLBACK_MAX_LEN {
        return Err(not_a_callback());
    }
    let url = if text.contains("://") {
        let url = reqwest::Url::parse(text).map_err(|_| not_a_callback())?;
        let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
        if url.scheme() != "http" || !loopback || url.path() != CHATGPT_CALLBACK_PATH {
            return Err(not_a_callback());
        }
        url
    } else {
        let query = text.trim_start_matches('?');
        reqwest::Url::parse(&format!("http://127.0.0.1{CHATGPT_CALLBACK_PATH}?{query}"))
            .map_err(|_| not_a_callback())?
    };
    let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
    if !query.contains_key("state") {
        return Err(not_a_callback());
    }
    Ok(query)
}

fn not_a_callback() -> AppError {
    AppError::new(
        StatusCode::BAD_REQUEST,
        "invalid_callback",
        format!(
            "That isn't the address the sign-in came back to. Copy the whole address from the browser's address bar: it starts with http://127.0.0.1:{CHATGPT_CALLBACK_PORT}{CHATGPT_CALLBACK_PATH}?code="
        ),
    )
}

/// `POST /api/providers/chatgpt/complete`: the callback's address from the person's
/// device. The waiting sign-in whose `state` it carries is taken (used once), its code
/// exchanged here with the verifier only the service holds, the `id_token` checked and
/// the tokens stored here. Admins only; rate limited (`auth::CHATGPT_COMPLETE_LIMIT`).
///
/// 400 `invalid_callback` (not a callback address) and 400 `invalid_state` (another
/// sign-in's) leave the sign-in waiting; 409 `no_sign_in` when none is waiting (never
/// started, finished, cancelled or out of time); 400 `sign_in_failed` when ChatGPT
/// declined or the exchange failed, which ends the sign-in (`GET` then says why).
pub async fn chatgpt_complete(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    Json(body): Json<ChatGptComplete>,
) -> AppResult<Json<ChatGptStatus>> {
    let query = callback_query(&body.callback_url)?;
    let pending = {
        let mut flow = state.providers.flow();
        let taken = matches!(&*flow, Flow::Waiting(p) if Some(&p.state) == query.get("state"));
        if taken {
            let Flow::Waiting(pending) = std::mem::replace(&mut *flow, Flow::Idle) else {
                unreachable!("checked above")
            };
            *flow = Flow::Exchanging {
                authorize_url: pending.authorize_url.clone(),
                state: pending.state.clone(),
            };
            pending
        } else {
            return Err(match &*flow {
                Flow::Waiting(_) => {
                    tracing::warn!("a ChatGPT callback for another sign-in was refused");
                    AppError::new(
                        StatusCode::BAD_REQUEST,
                        "invalid_state",
                        "That address is from another sign-in attempt. Use the address from the latest one, or start again.",
                    )
                }
                Flow::Exchanging { .. } => {
                    AppError::conflict("no_sign_in", "This sign-in is already being finished.")
                }
                Flow::Failed(message) => AppError::conflict(
                    "no_sign_in",
                    format!("No ChatGPT sign-in is waiting. {message}"),
                ),
                Flow::Idle => AppError::conflict(
                    "no_sign_in",
                    "No ChatGPT sign-in is waiting. Start again with Continue with ChatGPT.",
                ),
            });
        }
    };
    let outcome = match (query.get("error"), query.get("code")) {
        (Some(error), _) => Err(query
            .get("error_description")
            .cloned()
            .unwrap_or_else(|| format!("ChatGPT answered {error}."))),
        (None, Some(code)) => {
            complete(
                &state,
                &pending,
                code,
                query.get("client_id").cloned(),
                query.get("scope").cloned(),
            )
            .await
        }
        (None, None) => Err("The sign-in came back without a code.".to_string()),
    };
    match outcome {
        Ok(account) => {
            set_flow(&state, &pending.state, Flow::Idle);
            state.providers.forget_models();
            crate::settings::emit_settings(&state).await;
            tracing::info!("signed in with ChatGPT");
            Ok(Json(ChatGptStatus::SignedIn {
                account,
                last_error: None,
            }))
        }
        Err(message) => {
            set_failed(&state, &pending.state, &message);
            Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "sign_in_failed",
                message,
            ))
        }
    }
}

/// The authorization code → tokens, the `id_token` checked, the tokens stored.
async fn complete(
    app: &App,
    pending: &Pending,
    code: &str,
    issued_client_id: Option<String>,
    callback_scope: Option<String>,
) -> Result<ChatGptAccount, String> {
    let endpoints = app.providers.endpoints();
    // A reauthorization may omit the client id; one that names another is refused.
    if let (Some(issued), Some(known)) = (&issued_client_id, &pending.client_id)
        && issued != known
    {
        return Err("ChatGPT answered for another registration of Margin.".into());
    }
    let client_id = issued_client_id
        .or_else(|| pending.client_id.clone())
        .ok_or("ChatGPT didn't say which client id it registered Margin as.")?;
    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", client_id.as_str()),
        ("code", code),
        ("code_verifier", pending.verifier.as_str()),
        ("redirect_uri", pending.redirect_uri.as_str()),
        ("resource", CHATGPT_API_URL),
    ];
    let resp = app
        .providers
        .http
        .post(token_url(&endpoints))
        .form(&form)
        .send()
        .await
        .map_err(|e| {
            format!(
                "Couldn't reach ChatGPT's sign-in server ({}).",
                e.without_url()
            )
        })?;
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
        &pending.nonce,
        &id_token,
    )
    .await?;

    // Under the refresh lock and only while this sign-in is still the one in progress:
    // a sign-out or a newer sign-in that came meanwhile wins.
    let _one = app.providers.refresh.lock().await;
    let current = app.providers.flow.locked().state() == Some(pending.state.as_str());
    if !current {
        return Err("The sign-in was cancelled or replaced by a newer one.".into());
    }
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

/// `GET /api/providers/chatgpt`. A sign-in that failed while valid tokens are kept is
/// `SignedIn` with the failure as `last_error`. Admins only: while a sign-in waits, the
/// answer carries the authorize URL that completes it.
pub async fn chatgpt_status(
    State(state): State<AppState>,
    _admin: RequireAdmin,
) -> AppResult<Json<ChatGptStatus>> {
    let (waiting, failed) = match &*state.providers.flow() {
        Flow::Waiting(p) => (Some(p.authorize_url.clone()), None),
        Flow::Exchanging { authorize_url, .. } => (Some(authorize_url.clone()), None),
        Flow::Failed(message) => (None, Some(message.clone())),
        Flow::Idle => (None, None),
    };
    if let Some(authorize_url) = waiting {
        return Ok(Json(ChatGptStatus::Waiting { authorize_url }));
    }
    Ok(Json(match (chatgpt_account(&state).await?, failed) {
        (Some(account), last_error) => ChatGptStatus::SignedIn {
            account,
            last_error,
        },
        (None, Some(message)) => ChatGptStatus::Failed { message },
        (None, None) => ChatGptStatus::SignedOut,
    }))
}

/// `DELETE /api/providers/chatgpt`: revokes the refresh token (best effort) and
/// forgets the tokens; a sign-in in progress is cancelled (its state and verifier
/// dropped, so its callback can no longer complete it). Admins only.
pub async fn chatgpt_sign_out(
    State(state): State<AppState>,
    _admin: RequireAdmin,
) -> AppResult<StatusCode> {
    *state.providers.flow.locked() = Flow::Idle;
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
    fn callback_addresses_are_read_whole_or_as_their_query() {
        let q = callback_query("http://127.0.0.1:1455/auth/callback?code=c%2B1&state=s").unwrap();
        assert_eq!((q["code"].as_str(), q["state"].as_str()), ("c+1", "s"));
        let q =
            callback_query(" http://localhost:54321/auth/callback?state=s&error=access_denied\n")
                .unwrap();
        assert_eq!(q["error"], "access_denied");
        assert_eq!(callback_query("?code=c&state=s").unwrap()["code"], "c");
        assert_eq!(callback_query("code=c&state=s").unwrap()["state"], "s");
        for bad in [
            "",
            "code=c",
            "https://127.0.0.1:1455/auth/callback?code=c&state=s",
            "http://example.com/auth/callback?code=c&state=s",
            "http://127.0.0.1:1455/callback?code=c&state=s",
            &format!("code=c&state={}", "s".repeat(CALLBACK_MAX_LEN)),
        ] {
            let e = callback_query(bad).unwrap_err();
            assert_eq!(e.code, "invalid_callback", "{bad}");
        }
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
