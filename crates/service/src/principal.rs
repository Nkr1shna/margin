//! Who is calling: every `/api` request resolves a `Principal` before any handler runs
//! (L1-auth, `docs/l1-brief.md` D1).
//!
//! `require_principal` is the one guard on `/api` (`server::router`): a Bearer token
//! starting with `mga_` is an agent's (`agents::principal_for_token`), anything else a
//! device's. Every refusal is 401 `unauthorized`, never saying which half failed. The
//! routes agents may not call carry `refuse_agents` inside it (403 `agent_not_allowed`).
//! Handlers read the caller with the extractors here: `CurrentPrincipal`,
//! `CurrentDevice` (403 `device_required` for an agent), `RequireAdmin` (403
//! `admin_required`), `RequireOwner`.

use std::sync::Arc;

use axum::extract::{FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use margin_shared::{
    AGENT_TOKEN_PREFIX, Id, OrgRole, Person, PersonSettings, PersonState, PrincipalId, access_code,
    handle_for_login, person_id,
};

use crate::error::{AppError, AppResult};
use crate::people::PersonRow;
use crate::state::{AppState, DeviceRow};
use crate::{agents, auth, db, groups};

/// The person id the first commit's stub principal uses for a device signed in before
/// `people-v1` created the owner's row (L1-auth's migration makes it unreachable).
pub const BOOTSTRAP_PERSON_ID: &str = "bootstrap";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrincipalKind {
    Person,
    Agent { owner_id: Id, agent_id: Id },
}

/// The caller of an `/api` request.
#[derive(Debug, Clone)]
pub struct Principal {
    pub kind: PrincipalKind,
    /// The person, or the agent's owner.
    pub person_id: Id,
    /// `person/<id>` or `agent/<id>`.
    pub id: PrincipalId,
    /// The expanded read set (D5); for an agent the owner's ∩ its ceiling.
    pub paths: Vec<String>,
    /// Own id and every direct and implied group (L2 evaluates `deny` against it).
    pub deny_ids: Vec<String>,
    /// `Member` for every agent.
    pub org_role: OrgRole,
    /// Set for a device token.
    pub device: Option<DeviceRow>,
    /// The person row as read (handle, settings); an agent's owner.
    pub person: Person,
}

impl Principal {
    pub fn is_admin(&self) -> bool {
        self.org_role.is_admin()
    }

    pub fn is_agent(&self) -> bool {
        matches!(self.kind, PrincipalKind::Agent { .. })
    }
}

/// The caller, inserted by `require_principal`.
#[derive(Debug, Clone)]
pub struct CurrentPrincipal(pub Arc<Principal>);

/// The calling device (403 `device_required` for an agent token).
#[derive(Debug, Clone)]
pub struct CurrentDevice(pub DeviceRow);

/// The caller, who must be an organisation admin (or the owner).
#[derive(Debug, Clone)]
pub struct RequireAdmin(pub Arc<Principal>);

/// The caller, who must be the organisation owner.
#[derive(Debug, Clone)]
pub struct RequireOwner(pub Arc<Principal>);

impl<S: Send + Sync> FromRequestParts<S> for CurrentPrincipal {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, AppError> {
        parts
            .extensions
            .get::<CurrentPrincipal>()
            .cloned()
            .ok_or_else(AppError::unauthorized)
    }
}

impl<S: Send + Sync> FromRequestParts<S> for CurrentDevice {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, AppError> {
        let CurrentPrincipal(p) = CurrentPrincipal::from_request_parts(parts, state).await?;
        p.device
            .clone()
            .map(CurrentDevice)
            .ok_or_else(device_required)
    }
}

impl<S: Send + Sync> FromRequestParts<S> for RequireAdmin {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, AppError> {
        let CurrentPrincipal(p) = CurrentPrincipal::from_request_parts(parts, state).await?;
        if p.is_admin() {
            Ok(RequireAdmin(p))
        } else {
            Err(admin_required())
        }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for RequireOwner {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, AppError> {
        let CurrentPrincipal(p) = CurrentPrincipal::from_request_parts(parts, state).await?;
        if p.org_role == OrgRole::Owner {
            Ok(RequireOwner(p))
        } else {
            Err(AppError::new(
                StatusCode::FORBIDDEN,
                access_code::ADMIN_REQUIRED,
                "Only the organisation owner can do this.",
            ))
        }
    }
}

pub fn admin_required() -> AppError {
    AppError::new(
        StatusCode::FORBIDDEN,
        access_code::ADMIN_REQUIRED,
        "Only an organisation admin can do this.",
    )
}

pub fn device_required() -> AppError {
    AppError::new(
        StatusCode::FORBIDDEN,
        access_code::DEVICE_REQUIRED,
        "Sign in with a device for this; an agent token can't do it.",
    )
}

pub fn agent_not_allowed() -> AppError {
    AppError::new(
        StatusCode::FORBIDDEN,
        access_code::AGENT_NOT_ALLOWED,
        "Agent tokens can read context; sign in with a device for this.",
    )
}

/// The `/api` guard: resolves the Bearer token to a `Principal` and inserts
/// `CurrentPrincipal`.
pub async fn require_principal(
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
    let principal = if token.starts_with(AGENT_TOKEN_PREFIX) {
        agents::principal_for_token(&state, &token).await
    } else {
        principal_for_device_token(&state, &token).await
    };
    match principal {
        Ok(Some(p)) => {
            req.extensions_mut().insert(CurrentPrincipal(p));
            next.run(req).await
        }
        Ok(None) => AppError::unauthorized().into_response(),
        Err(e) => e.into_response(),
    }
}

/// Inside `require_principal` on every route agents may not call (all but `/api/me`,
/// `/api/projects`, `/api/context/*`): 403 `agent_not_allowed` for an agent token.
pub async fn refuse_agents(req: Request, next: Next) -> Response {
    match req.extensions().get::<CurrentPrincipal>() {
        Some(CurrentPrincipal(p)) if p.is_agent() => agent_not_allowed().into_response(),
        _ => next.run(req).await,
    }
}

/// A device token's principal. `None` (401) for an unknown token, or a device whose
/// person is missing or disabled.
///
/// A device without `person_id` exists only before `people-v1` (which runs before the
/// service serves): it is the bootstrap owner, synthesised from
/// `config.allowed_github_login`. Once the migration has run such a device is refused.
pub async fn principal_for_device_token(
    state: &AppState,
    token: &str,
) -> AppResult<Option<Arc<Principal>>> {
    let Some(device) = auth::device_for_token(state, token).await? else {
        return Ok(None);
    };
    let person = match &device.person_id {
        Some(id) => match state.store.get::<PersonRow>(db::PERSON, id).await? {
            Some(row) if row.person.state == PersonState::Active => row.person,
            _ => return Ok(None),
        },
        None if !crate::people::migration_done(state).await? => bootstrap_person(state),
        None => return Ok(None),
    };
    let expansion = groups::expansion(state, &person.id).await?;
    Ok(Some(Arc::new(Principal {
        kind: PrincipalKind::Person,
        person_id: person.id.clone(),
        id: person_id(&person.id),
        paths: expansion.paths.clone(),
        deny_ids: expansion.deny_ids.clone(),
        org_role: expansion.org_role,
        device: Some(device),
        person,
    })))
}

/// The owner as `config.allowed_github_login` names them, before `people-v1`.
pub fn bootstrap_person(state: &AppState) -> Person {
    let login = state.config.allowed_github_login.clone();
    Person {
        id: BOOTSTRAP_PERSON_ID.into(),
        handle: handle_for_login(&login),
        name: login.clone(),
        github_login: login,
        github_id: None,
        avatar_url: None,
        org_role: OrgRole::Owner,
        state: PersonState::Active,
        groups: Vec::new(),
        settings: PersonSettings::default(),
        created_at: Utc::now(),
    }
}
