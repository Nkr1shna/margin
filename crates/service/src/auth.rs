//! Sign-in ("the app waits on the service"), device tokens and the `/api` guard.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration as StdDuration, Instant};

use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum::{Extension, Json};
use chrono::{DateTime, Duration, Utc};
use margin_shared::{
    Device, Event, Id, LoginState, Me, PollLoginRequest, StartLogin, StartLoginRequest,
};
use serde::Deserialize;

use crate::db;
use crate::error::{ApiJson, AppError, AppResult};
use crate::secrets::GITHUB_CLIENT_SECRET;
use crate::state::{AppState, DeviceRow, new_id, random_token, sha256_hex};

pub const LOGIN_TTL_MINUTES: i64 = 10;
const LAST_SEEN_INTERVAL_SECONDS: i64 = 60;

// ---------------------------------------------------------------------------
// Pending logins (in memory: they live for ten minutes)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Login {
    pub device_name: String,
    pub poll_secret_hash: String,
    pub expires_at: DateTime<Utc>,
    pub phase: LoginPhase,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LoginPhase {
    Pending,
    /// The callback is talking to GitHub.
    Exchanging,
    Approved {
        github_login: String,
    },
    Denied {
        reason: String,
    },
    /// The token was handed to the app.
    Collected,
}

/// Drops logins past their expiry.
pub fn purge_expired_logins(state: &AppState) {
    let now = Utc::now();
    state
        .logins
        .lock()
        .unwrap()
        .retain(|_, l| l.expires_at > now);
}

// ---------------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------------

/// Who a limit counts: the client's IP (sign-in, before there is a device) or the
/// signed-in device (`docs/agent-contract.md` §7).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Client {
    Ip(IpAddr),
    Device(String),
}

/// (window start, requests in it) per (route, client).
type Windows = HashMap<(&'static str, Client), (Instant, u32)>;

const WINDOW: StdDuration = StdDuration::from_secs(60);

/// Fixed one-minute windows per (route, client).
#[derive(Default)]
pub struct RateLimiter {
    windows: Mutex<Windows>,
}

impl RateLimiter {
    /// Counts one request; `false` when it is over `per_minute` in this window.
    pub fn check(&self, route: &'static str, ip: IpAddr, per_minute: u32) -> bool {
        self.admit(route, Client::Ip(ip), per_minute).is_ok()
    }

    /// Counts one request; over the limit, `Err` holds how long until the window ends.
    pub fn admit(
        &self,
        route: &'static str,
        client: Client,
        per_minute: u32,
    ) -> Result<(), StdDuration> {
        let now = Instant::now();
        let mut map = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry((route, client)).or_insert((now, 0));
        if now.duration_since(entry.0) >= WINDOW {
            *entry = (now, 0);
        }
        entry.1 = entry.1.saturating_add(1);
        if entry.1 <= per_minute {
            Ok(())
        } else {
            Err(WINDOW.saturating_sub(now.duration_since(entry.0)))
        }
    }

    pub fn purge(&self) {
        let now = Instant::now();
        self.windows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, (start, _)| now.duration_since(*start) < WINDOW);
    }
}

/// 429 `rate_limited` with `Retry-After` (whole seconds, at least 1) until the window ends.
pub fn rate_limited(retry_after: StdDuration) -> Response {
    let secs = retry_after.as_secs() + u64::from(retry_after.subsec_nanos() > 0);
    let mut res = AppError::rate_limited().into_response();
    res.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(secs.max(1)));
    res
}

/// What a route's limit counts by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum By {
    Ip,
    Device,
}

/// One route's limit (`server::router` puts it on the route through `limit`).
#[derive(Debug, Clone, Copy)]
pub struct Limit {
    pub route: &'static str,
    pub per_minute: u32,
    pub by: By,
    /// Folder files (`?workspace_id=`) don't count: a device sends a folder's files one
    /// after another on every sync, as many as the folder has (`docs/agent-contract.md` §7).
    pub skip_folder_files: bool,
}

/// `POST /auth/start`, per IP.
pub const START_LIMIT: Limit = Limit {
    route: "start",
    per_minute: 10,
    by: By::Ip,
    skip_folder_files: false,
};
/// `POST /auth/poll`, per IP.
pub const POLL_LIMIT: Limit = Limit {
    route: "poll",
    per_minute: 90,
    by: By::Ip,
    skip_folder_files: false,
};
/// `POST /api/chats/{id}/messages`, per device.
pub const MESSAGES_LIMIT: Limit = Limit {
    route: "messages",
    per_minute: 30,
    by: By::Device,
    skip_folder_files: false,
};
/// `POST /api/context/search`, per device.
pub const CONTEXT_SEARCH_LIMIT: Limit = Limit {
    route: "context-search",
    per_minute: 120,
    by: By::Device,
    skip_folder_files: false,
};
/// `POST /api/context/ingest`, per device (`margin-mcp ingest-dir` waits out the
/// `Retry-After`, so 200 files take under four minutes).
pub const CONTEXT_INGEST_LIMIT: Limit = Limit {
    route: "context-ingest",
    per_minute: 60,
    by: By::Device,
    skip_folder_files: false,
};
/// `POST /api/sources` and `POST /api/source-uploads` together, per device; a folder's
/// files aren't counted.
pub const SOURCES_LIMIT: Limit = Limit {
    route: "sources",
    per_minute: 60,
    by: By::Device,
    skip_folder_files: true,
};

/// Middleware: counts the request against `limit` and answers 429 `rate_limited` with
/// `Retry-After` over it. A device limit sits inside `require_device` (it reads
/// `CurrentDevice`).
pub async fn limit(
    State((state, limit)): State<(AppState, Limit)>,
    req: Request,
    next: Next,
) -> Response {
    if limit.skip_folder_files && is_folder_file(req.uri().query()) {
        return next.run(req).await;
    }
    let client = match limit.by {
        By::Ip => {
            let Some(ConnectInfo(peer)) = req.extensions().get::<ConnectInfo<SocketAddr>>() else {
                return AppError::internal("no peer address on the request").into_response();
            };
            Client::Ip(client_ip(*peer, req.headers()))
        }
        By::Device => match req.extensions().get::<CurrentDevice>() {
            Some(CurrentDevice(device)) => Client::Device(device.id.clone()),
            None => return AppError::unauthorized().into_response(),
        },
    };
    match state.limiter.admit(limit.route, client, limit.per_minute) {
        Ok(()) => next.run(req).await,
        Err(retry_after) => {
            tracing::info!(route = limit.route, "rate limited");
            rate_limited(retry_after)
        }
    }
}

/// Whether a query string names a folder (`workspace_id`): a folder's file.
fn is_folder_file(query: Option<&str>) -> bool {
    query.is_some_and(|q| {
        q.split('&').any(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            k == "workspace_id" && !v.is_empty()
        })
    })
}

/// The client's IP. Behind cloudflared every connection comes from loopback, so for
/// loopback peers the `CF-Connecting-IP` header Cloudflare sets is used instead.
pub fn client_ip(peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
    if peer.ip().is_loopback()
        && let Some(ip) = headers
            .get("cf-connecting-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<IpAddr>().ok())
    {
        return ip;
    }
    peer.ip()
}

// ---------------------------------------------------------------------------
// /auth/start, /auth/poll
// ---------------------------------------------------------------------------

/// The name a device signs in with as stored: `text::for_model`, whitespace runs
/// (newlines included) as one space, trimmed, at most 100 characters.
pub fn device_display_name(raw: &str) -> String {
    let normal = margin_shared::text::for_model(raw);
    let one_line = normal.split_whitespace().collect::<Vec<_>>().join(" ");
    one_line.chars().take(100).collect()
}

/// `POST /auth/start` (limited per IP by `START_LIMIT`).
pub async fn start(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<StartLoginRequest>,
) -> AppResult<Json<StartLogin>> {
    // The name reaches the model (the device tools list it): normalised like any text
    // from outside the owner's messages (`text::for_model`), on one line.
    let device_name = device_display_name(&req.device_name);
    if device_name.is_empty() {
        return Err(AppError::bad_request("Give this device a name."));
    }
    purge_expired_logins(&state);

    // The login id doubles as the OAuth `state`; a v4 UUID carries 122 random bits.
    let login_id = new_id();
    let poll_secret = random_token();
    let expires_at = Utc::now() + Duration::minutes(LOGIN_TTL_MINUTES);
    let config = &state.config;
    let redirect_uri = format!("{}/auth/github/callback", config.public_url);
    let authorize_url = reqwest::Url::parse_with_params(
        &format!("{}/login/oauth/authorize", config.github_oauth_url),
        &[
            ("client_id", config.github_client_id.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("state", login_id.as_str()),
            ("scope", "read:user"),
        ],
    )
    .map_err(|e| AppError::internal(format!("bad github_oauth_url: {e}")))?
    .to_string();

    state.logins.lock().unwrap().insert(
        login_id.clone(),
        Login {
            device_name,
            poll_secret_hash: sha256_hex(&poll_secret),
            expires_at,
            phase: LoginPhase::Pending,
        },
    );
    tracing::info!("sign-in started");
    Ok(Json(StartLogin {
        login_id,
        poll_secret,
        authorize_url,
        expires_at,
    }))
}

/// `POST /auth/poll` (limited per IP by `POLL_LIMIT`).
pub async fn poll(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<PollLoginRequest>,
) -> AppResult<Json<LoginState>> {
    let now = Utc::now();
    let collected = {
        let mut logins = state.logins.lock().unwrap();
        let Some(login) = logins.get_mut(&req.login_id) else {
            return Ok(Json(LoginState::Expired));
        };
        if login.poll_secret_hash != sha256_hex(&req.poll_secret) {
            return Err(AppError::not_found("sign-in"));
        }
        if login.expires_at <= now {
            logins.remove(&req.login_id);
            return Ok(Json(LoginState::Expired));
        }
        match &login.phase {
            LoginPhase::Pending | LoginPhase::Exchanging => return Ok(Json(LoginState::Pending)),
            LoginPhase::Denied { reason } => {
                return Ok(Json(LoginState::Denied {
                    reason: reason.clone(),
                }));
            }
            LoginPhase::Collected => return Ok(Json(LoginState::Expired)),
            LoginPhase::Approved { github_login } => {
                let out = (github_login.clone(), login.device_name.clone());
                login.phase = LoginPhase::Collected;
                out
            }
        }
    };

    // Mint the device token now, so it is handed out exactly once.
    let (github_login, device_name) = collected;
    let token = random_token();
    let device = DeviceRow {
        id: new_id(),
        name: device_name,
        token_hash: sha256_hex(&token),
        github_login: github_login.clone(),
        created_at: now,
        last_seen_at: Some(now),
    };
    {
        let _w = state.write_lock.lock().await;
        state.store.put(db::DEVICE, &device.id, &device).await?;
    }
    tracing::info!(device = %device.id, "device signed in");
    Ok(Json(LoginState::Approved {
        token,
        github_login,
    }))
}

// ---------------------------------------------------------------------------
// /auth/github/callback
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

enum Outcome {
    SignedIn,
    Denied(String),
    Expired,
    InProgress,
    /// The sign-in already completed; this is a replay.
    Used,
}

/// The callback page's stylesheet: the only thing its CSP lets in (by hash).
const PAGE_STYLE: &str = r#"
  :root { color-scheme: light dark; --bg: #ffffff; --fg: #0d0d0d; --muted: #5d5d5d; --line: #e5e5e5; }
  @media (prefers-color-scheme: dark) { :root { --bg: #0d0d0d; --fg: #f3f3f3; --muted: #a3a3a3; --line: #2a2a2a; } }
  body { margin: 0; min-height: 100vh; display: grid; place-items: center; background: var(--bg); color: var(--fg);
         font: 16px/1.5 -apple-system, BlinkMacSystemFont, "Helvetica Neue", Arial, sans-serif; }
  main { max-width: 28rem; margin: 1rem; padding: 2rem; border: 1px solid var(--line); border-radius: 12px; }
  h1 { font-size: 1.25rem; margin: 0 0 .5rem; }
  p { margin: 0; color: var(--muted); }
"#;

/// `Content-Security-Policy` of the callback page (the only HTML the service serves):
/// nothing loads, runs, frames it or is submitted from it; its one stylesheet is
/// allowed by its hash.
pub fn page_csp() -> String {
    use base64::Engine as _;
    use sha2::Digest as _;
    let hash = base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(PAGE_STYLE));
    format!(
        "default-src 'none'; style-src 'sha256-{hash}'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    )
}

pub async fn github_callback(
    State(state): State<AppState>,
    Query(q): Query<CallbackQuery>,
) -> Response {
    let outcome = callback_outcome(&state, q).await;
    let (status, title, body) = match outcome {
        Outcome::SignedIn => (
            StatusCode::OK,
            "Signed in",
            "Signed in to Margin. You can return to the app.".to_string(),
        ),
        Outcome::Denied(reason) => (StatusCode::FORBIDDEN, "Sign-in denied", reason),
        Outcome::Expired => (
            StatusCode::GONE,
            "Sign-in expired",
            "This sign-in link has expired or was already used. Start again from the app."
                .to_string(),
        ),
        Outcome::InProgress => (
            StatusCode::CONFLICT,
            "Signing in",
            "This sign-in is already being completed. Return to the app.".to_string(),
        ),
        Outcome::Used => (
            StatusCode::GONE,
            "Sign-in already used",
            "This sign-in link was already used. Return to the app, or start again from it."
                .to_string(),
        ),
    };
    let mut res = (status, Html(page(title, &body))).into_response();
    let headers = res.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(csp) = HeaderValue::from_str(&page_csp()) {
        headers.insert(header::CONTENT_SECURITY_POLICY, csp);
    }
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    res
}

async fn callback_outcome(state: &AppState, q: CallbackQuery) -> Outcome {
    let Some(login_id) = q.state else {
        return Outcome::Expired;
    };
    {
        let mut logins = state.logins.lock().unwrap();
        let Some(login) = logins.get_mut(&login_id) else {
            return Outcome::Expired;
        };
        if login.expires_at <= Utc::now() {
            logins.remove(&login_id);
            return Outcome::Expired;
        }
        match &login.phase {
            LoginPhase::Pending => {}
            LoginPhase::Exchanging => return Outcome::InProgress,
            // The `state` is single-use: a replayed callback (with this or any other
            // `code`) is refused and never reaches GitHub or changes the login.
            LoginPhase::Approved { .. } | LoginPhase::Collected => return Outcome::Used,
            LoginPhase::Denied { reason } => return Outcome::Denied(reason.clone()),
        }
        login.phase = LoginPhase::Exchanging;
    }

    let result = match (q.error, q.code) {
        (Some(_), _) | (None, None) => {
            Err("GitHub sign-in was cancelled. Start again from the app.".to_string())
        }
        (None, Some(code)) => github_login_for_code(state, &code).await,
    };
    let phase = match result {
        Ok(login) if login.eq_ignore_ascii_case(&state.config.allowed_github_login) => {
            tracing::info!("GitHub sign-in approved");
            LoginPhase::Approved {
                github_login: login,
            }
        }
        Ok(login) => {
            tracing::warn!("GitHub sign-in denied for a login that isn't allowed");
            LoginPhase::Denied {
                reason: format!("The GitHub account @{login} isn't allowed to use this Margin."),
            }
        }
        Err(reason) => LoginPhase::Denied { reason },
    };
    let outcome = match &phase {
        LoginPhase::Approved { .. } => Outcome::SignedIn,
        LoginPhase::Denied { reason } => Outcome::Denied(reason.clone()),
        _ => Outcome::Expired,
    };
    if let Some(login) = state.logins.lock().unwrap().get_mut(&login_id) {
        login.phase = phase;
    }
    outcome
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Deserialize)]
struct GithubUser {
    login: String,
}

/// Exchanges the OAuth code and asks GitHub who signed in. Errors are user-facing text.
async fn github_login_for_code(state: &AppState, code: &str) -> Result<String, String> {
    let config = &state.config;
    let secret = match state.secret_get(GITHUB_CLIENT_SECRET).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            return Err(
                "The service has no GitHub client secret. Run `margin-service setup`.".into(),
            );
        }
        Err(e) => {
            tracing::error!("couldn't read the GitHub client secret: {e:#}");
            return Err(
                "The service couldn't read its GitHub client secret from its secret store.".into(),
            );
        }
    };
    let redirect_uri = format!("{}/auth/github/callback", config.public_url);
    let resp = state
        .http
        .post(format!(
            "{}/login/oauth/access_token",
            config.github_oauth_url
        ))
        .header(header::ACCEPT, "application/json")
        .form(&[
            ("client_id", config.github_client_id.as_str()),
            ("client_secret", secret.as_str()),
            ("code", code),
            ("redirect_uri", redirect_uri.as_str()),
        ])
        .send()
        .await
        .map_err(|e| {
            tracing::warn!("GitHub token exchange failed: {}", e.without_url());
            "The service couldn't reach GitHub. Try again.".to_string()
        })?;
    if !resp.status().is_success() {
        tracing::warn!("GitHub token exchange returned {}", resp.status());
        return Err(format!(
            "GitHub rejected the sign-in ({}). Try again.",
            resp.status()
        ));
    }
    let token: TokenResponse = resp
        .json()
        .await
        .map_err(|_| "GitHub's answer couldn't be read. Try again.".to_string())?;
    let Some(access_token) = token.access_token else {
        let why = token
            .error_description
            .or(token.error)
            .unwrap_or_else(|| "no access token".into());
        tracing::warn!("GitHub token exchange refused: {why}");
        return Err(format!(
            "GitHub didn't complete the sign-in: {why}. Start again from the app."
        ));
    };
    let resp = state
        .http
        .get(format!("{}/user", config.github_api_url))
        .header(header::AUTHORIZATION, format!("Bearer {access_token}"))
        .header(header::ACCEPT, "application/vnd.github+json")
        .header(header::USER_AGENT, "margin-service")
        .send()
        .await
        .map_err(|e| {
            tracing::warn!("GitHub /user failed: {}", e.without_url());
            "The service couldn't reach GitHub. Try again.".to_string()
        })?;
    if !resp.status().is_success() {
        tracing::warn!("GitHub /user returned {}", resp.status());
        return Err(format!(
            "GitHub didn't say who signed in ({}). Try again.",
            resp.status()
        ));
    }
    let user: GithubUser = resp
        .json()
        .await
        .map_err(|_| "GitHub's answer couldn't be read. Try again.".to_string())?;
    Ok(user.login)
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn page(title: &str, message: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} · Margin</title>
<style>{style}</style>
</head>
<body><main><h1>{title}</h1><p>{message}</p></main></body>
</html>
"#,
        title = escape_html(title),
        message = escape_html(message),
        style = PAGE_STYLE,
    )
}

// ---------------------------------------------------------------------------
// Bearer guard for /api
// ---------------------------------------------------------------------------

/// The device making an `/api` request.
#[derive(Clone)]
pub struct CurrentDevice(pub DeviceRow);

pub async fn require_device(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .map(|t| t.trim().to_string());
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        return AppError::unauthorized().into_response();
    };
    let device = match device_for_token(&state, &token).await {
        Ok(Some(d)) => d,
        Ok(None) => return AppError::unauthorized().into_response(),
        Err(e) => return e.into_response(),
    };
    req.extensions_mut().insert(CurrentDevice(device));
    next.run(req).await
}

async fn device_for_token(state: &AppState, token: &str) -> AppResult<Option<DeviceRow>> {
    let hash = sha256_hex(token);
    let found: Vec<DeviceRow> = state
        .store
        .list_where(db::DEVICE, "token_hash", &hash)
        .await?;
    let Some(mut device) = found.into_iter().next() else {
        return Ok(None);
    };
    let now = Utc::now();
    let stale = device
        .last_seen_at
        .is_none_or(|t| (now - t).num_seconds() >= LAST_SEEN_INTERVAL_SECONDS);
    if stale {
        let _w = state.write_lock.lock().await;
        // Re-read under the lock so a concurrent revoke isn't undone.
        if let Some(mut current) = state.store.get::<DeviceRow>(db::DEVICE, &device.id).await? {
            current.last_seen_at = Some(now);
            state.store.put(db::DEVICE, &current.id, &current).await?;
            device = current;
        } else {
            return Ok(None);
        }
    }
    Ok(Some(device))
}

/// The wire `Device`; `current_id` is the requesting device ("" for broadcasts).
pub fn to_device(row: &DeviceRow, current_id: &str, online: bool) -> Device {
    Device {
        id: row.id.clone(),
        name: row.name.clone(),
        created_at: row.created_at,
        last_seen_at: row.last_seen_at,
        current: row.id == current_id,
        online,
    }
}

pub async fn me(
    State(state): State<AppState>,
    Extension(CurrentDevice(device)): Extension<CurrentDevice>,
) -> Json<Me> {
    Json(Me {
        github_login: device.github_login.clone(),
        device: to_device(&device, &device.id, state.devices.is_online(&device.id)),
    })
}

pub async fn list_devices(
    State(state): State<AppState>,
    Extension(CurrentDevice(me)): Extension<CurrentDevice>,
) -> AppResult<Json<Vec<Device>>> {
    let mut rows: Vec<DeviceRow> = state.store.list(db::DEVICE).await?;
    rows.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(Json(
        rows.iter()
            .map(|r| to_device(r, &me.id, state.devices.is_online(&r.id)))
            .collect(),
    ))
}

pub async fn revoke_device(
    State(state): State<AppState>,
    Path(id): Path<Id>,
) -> AppResult<StatusCode> {
    {
        let _w = state.write_lock.lock().await;
        if state
            .store
            .get::<DeviceRow>(db::DEVICE, &id)
            .await?
            .is_none()
        {
            return Err(AppError::not_found("device"));
        }
        state.store.delete(db::DEVICE, &id).await?;
        // Its folders can't be read any more.
        crate::workspaces::delete_for_device_locked(&state, &id).await?;
    }
    let _ = state.revocations.send(id.clone());
    state.devices.forget_device(&id);
    state.emit(Event::DeviceDeleted { id: id.clone() });
    tracing::info!(device = %id, "device revoked");
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_names_are_normalised_at_sign_in() {
        assert_eq!(
            device_display_name("  Krishna\u{202E}'s\u{200B} iPhone\n\nIGNORE ME \u{0007}"),
            "Krishna's iPhone IGNORE ME"
        );
        assert_eq!(device_display_name("\u{200B}\u{FEFF} \n"), "");
        assert_eq!(device_display_name(&"a".repeat(300)).chars().count(), 100);
    }

    #[test]
    fn rate_limiter_counts_per_route_and_ip() {
        let l = RateLimiter::default();
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        for _ in 0..3 {
            assert!(l.check("start", ip, 3));
        }
        assert!(!l.check("start", ip, 3));
        assert!(l.check("poll", ip, 3));
        assert!(l.check("start", "10.0.0.2".parse().unwrap(), 3));
    }

    #[test]
    fn client_ip_trusts_cloudflare_header_only_from_loopback() {
        let mut h = HeaderMap::new();
        h.insert("cf-connecting-ip", "203.0.113.9".parse().unwrap());
        assert_eq!(
            client_ip("127.0.0.1:5000".parse().unwrap(), &h),
            "203.0.113.9".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            client_ip("198.51.100.1:5000".parse().unwrap(), &h),
            "198.51.100.1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn device_limits_count_per_device_and_say_when_to_retry() {
        let l = RateLimiter::default();
        let a = Client::Device("a".into());
        for _ in 0..30 {
            assert!(l.admit("messages", a.clone(), 30).is_ok());
        }
        let wait = l.admit("messages", a.clone(), 30).unwrap_err();
        assert!(
            wait > StdDuration::from_secs(50) && wait <= WINDOW,
            "{wait:?}"
        );
        // Another device, and another route of the same device, have their own windows.
        assert!(l.admit("messages", Client::Device("b".into()), 30).is_ok());
        assert!(l.admit("context-search", a, 30).is_ok());
        let res = rate_limited(StdDuration::from_millis(1_500));
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(res.headers()[header::RETRY_AFTER], "2");
        assert_eq!(
            rate_limited(StdDuration::ZERO).headers()[header::RETRY_AFTER],
            "1"
        );
    }

    #[test]
    fn folder_files_are_told_apart_by_their_query() {
        assert!(is_folder_file(Some("workspace_id=w1&name=a.md")));
        assert!(is_folder_file(Some("name=a.md&workspace_id=w1")));
        assert!(!is_folder_file(Some("name=a.md&workspace_id=")));
        assert!(!is_folder_file(Some("name=a.md&topic_id=t")));
        assert!(!is_folder_file(Some("xworkspace_id=w1")));
        assert!(!is_folder_file(None));
    }

    #[test]
    fn pages_escape_text() {
        assert!(page("t", "<script>").contains("&lt;script&gt;"));
    }

    #[test]
    fn the_page_csp_allows_only_its_own_stylesheet() {
        use base64::Engine as _;
        use sha2::Digest as _;
        let html = page("t", "m");
        let start = html.find("<style>").unwrap() + "<style>".len();
        let end = html.find("</style>").unwrap();
        let hash = base64::engine::general_purpose::STANDARD
            .encode(sha2::Sha256::digest(&html[start..end]));
        let csp = page_csp();
        assert!(csp.starts_with("default-src 'none';"), "{csp}");
        assert!(csp.contains(&format!("style-src 'sha256-{hash}'")), "{csp}");
        assert!(!csp.contains("unsafe-inline"));
        assert!(csp.contains("frame-ancestors 'none'"));
        assert!(!html.contains("<script"));
    }
}
