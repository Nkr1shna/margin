//! Sign-in ("the app waits on the service"), device tokens and the `/api` guard.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration as StdDuration, Instant};

use axum::Json;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use chrono::{DateTime, Duration, Utc};
use margin_shared::{
    Device, Event, Id, LoginState, Me, PollLoginRequest, StartLogin, StartLoginRequest,
};
use serde::Deserialize;

use crate::db;
use crate::error::{ApiJson, AppError, AppResult};
use crate::principal::CurrentPrincipal;
use crate::secrets::GITHUB_CLIENT_SECRET;
use crate::state::{AppState, DeviceRow, new_id, random_token, sha256_hex};

pub const LOGIN_TTL_MINUTES: i64 = 10;
const START_LIMIT_PER_MINUTE: u32 = 10;
const POLL_LIMIT_PER_MINUTE: u32 = 90;
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

/// (window start, requests in it) per (route, client IP).
type Windows = HashMap<(&'static str, IpAddr), (Instant, u32)>;

/// Fixed one-minute windows per (route, client IP).
#[derive(Default)]
pub struct RateLimiter {
    windows: Mutex<Windows>,
}

impl RateLimiter {
    pub fn check(&self, route: &'static str, ip: IpAddr, per_minute: u32) -> bool {
        let now = Instant::now();
        let mut map = self.windows.lock().unwrap();
        let entry = map.entry((route, ip)).or_insert((now, 0));
        if now.duration_since(entry.0) >= StdDuration::from_secs(60) {
            *entry = (now, 0);
        }
        entry.1 += 1;
        entry.1 <= per_minute
    }

    pub fn purge(&self) {
        let now = Instant::now();
        self.windows
            .lock()
            .unwrap()
            .retain(|_, (start, _)| now.duration_since(*start) < StdDuration::from_secs(60));
    }
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

pub async fn start(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<StartLoginRequest>,
) -> AppResult<Json<StartLogin>> {
    if !state
        .limiter
        .check("start", client_ip(peer, &headers), START_LIMIT_PER_MINUTE)
    {
        return Err(AppError::rate_limited());
    }
    let device_name = req.device_name.trim();
    if device_name.is_empty() {
        return Err(AppError::bad_request("Give this device a name."));
    }
    let device_name: String = device_name.chars().take(100).collect();
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

pub async fn poll(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<PollLoginRequest>,
) -> AppResult<Json<LoginState>> {
    if !state
        .limiter
        .check("poll", client_ip(peer, &headers), POLL_LIMIT_PER_MINUTE)
    {
        return Err(AppError::rate_limited());
    }
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
        person_id: None,
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
    };
    let mut res = (status, Html(page(title, &body))).into_response();
    res.headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    res.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        "default-src 'none'; style-src 'unsafe-inline'"
            .parse()
            .unwrap(),
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
            LoginPhase::Approved { .. } | LoginPhase::Collected => return Outcome::SignedIn,
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
<style>
  :root {{ color-scheme: light dark; --bg: #ffffff; --fg: #0d0d0d; --muted: #5d5d5d; --line: #e5e5e5; }}
  @media (prefers-color-scheme: dark) {{ :root {{ --bg: #0d0d0d; --fg: #f3f3f3; --muted: #a3a3a3; --line: #2a2a2a; }} }}
  body {{ margin: 0; min-height: 100vh; display: grid; place-items: center; background: var(--bg); color: var(--fg);
         font: 16px/1.5 -apple-system, BlinkMacSystemFont, "Helvetica Neue", Arial, sans-serif; }}
  main {{ max-width: 28rem; margin: 1rem; padding: 2rem; border: 1px solid var(--line); border-radius: 12px; }}
  h1 {{ font-size: 1.25rem; margin: 0 0 .5rem; }}
  p {{ margin: 0; color: var(--muted); }}
</style>
</head>
<body><main><h1>{title}</h1><p>{message}</p></main></body>
</html>
"#,
        title = escape_html(title),
        message = escape_html(message),
    )
}

// ---------------------------------------------------------------------------
// Bearer guard for /api
// ---------------------------------------------------------------------------

/// The calling device, as `principal::CurrentDevice` extracts it from the principal.
pub use crate::principal::CurrentDevice;

/// The device a device token belongs to (touching `last_seen_at` at most every
/// `LAST_SEEN_INTERVAL_SECONDS`); `principal::require_principal` builds on it.
pub async fn device_for_token(state: &AppState, token: &str) -> AppResult<Option<DeviceRow>> {
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
        person_id: row.person_id.clone(),
    }
}

/// `GET /api/me`. First commit: the device only; L1-auth adds `person` (and an agent
/// token's `agent`).
pub async fn me(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
) -> AppResult<Json<Me>> {
    let device = p
        .device
        .as_ref()
        .ok_or_else(crate::principal::device_required)?;
    Ok(Json(Me {
        github_login: device.github_login.clone(),
        device: Some(to_device(
            device,
            &device.id,
            state.devices.is_online(&device.id),
        )),
        person: None,
        agent: None,
    }))
}

pub async fn list_devices(
    State(state): State<AppState>,
    CurrentDevice(me): CurrentDevice,
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
    fn pages_escape_text() {
        assert!(page("t", "<script>").contains("&lt;script&gt;"));
    }
}
