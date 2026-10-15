//! Agents a person owns, their ceilings and their `mga_` tokens (L1-groups,
//! `docs/l1-brief.md` D7).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use margin_shared::{Agent, AgentKind, AgentPatch, AgentToken, Id, NewAgent, PrincipalId};
use serde::{Deserialize, Serialize};

use crate::error::{ApiJson, AppResult};
use crate::groups::Expansion;
use crate::principal::{CurrentPrincipal, Principal};
use crate::state::AppState;

/// A stored agent: the wire type (whose `token` holds the prefix, dates and expiry)
/// plus the token's hex SHA-256 (`agent_token` index). The token itself is never kept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRow {
    #[serde(flatten)]
    pub agent: Agent,
    #[serde(default)]
    pub token_hash: Option<String>,
}

/// The principal of an `mga_` token: `kind: Agent`, the owner's person, `org_role:
/// Member`, `paths` = owner's ∩ ceiling (D1). `None` for an unknown, revoked or expired
/// token, a disabled agent or a disabled owner. First commit: always `None`.
pub async fn principal_for_token(
    _state: &AppState,
    _token: &str,
) -> AppResult<Option<Arc<Principal>>> {
    Ok(None)
}

/// The ceiling to store (D7): `asked` ∪ the ancestors of each asked group, which must
/// lie inside `owner.paths` (400 `outside_owner` naming the first that doesn't), never
/// empty. `None` asks for the kind's default: `Margin` the owner's whole expansion,
/// `External` and `Voice` the expansion minus `person/<owner>` (owner, Q5).
pub fn normalise_ceiling(
    _owner: &Expansion,
    _kind: AgentKind,
    _asked: Option<Vec<PrincipalId>>,
) -> AppResult<Vec<PrincipalId>> {
    unimplemented!("L1, groups")
}

/// `GET /api/agents`: the caller's agents; an admin's call lists everyone's.
pub async fn list_agents(
    State(_state): State<AppState>,
    _p: CurrentPrincipal,
) -> AppResult<Json<Vec<Agent>>> {
    unimplemented!("L1, groups")
}

/// `POST /api/agents`: an agent of the caller's.
pub async fn create_agent(
    State(_state): State<AppState>,
    _p: CurrentPrincipal,
    ApiJson(_new): ApiJson<NewAgent>,
) -> AppResult<Json<Agent>> {
    unimplemented!("L1, groups")
}

/// `PATCH /api/agents/{id}`: its owner; an admin may only set `disabled`.
pub async fn patch_agent(
    State(_state): State<AppState>,
    _p: CurrentPrincipal,
    Path(_id): Path<Id>,
    ApiJson(_patch): ApiJson<AgentPatch>,
) -> AppResult<Json<Agent>> {
    unimplemented!("L1, groups")
}

/// `DELETE /api/agents/{id}`: its owner; the token dies with it.
pub async fn delete_agent(
    State(_state): State<AppState>,
    _p: CurrentPrincipal,
    Path(_id): Path<Id>,
) -> AppResult<StatusCode> {
    unimplemented!("L1, groups")
}

/// `POST /api/agents/{id}/token`: mints `mga_` + `state::random_token()`, replacing any
/// previous token, and returns it once.
pub async fn mint_token(
    State(_state): State<AppState>,
    _p: CurrentPrincipal,
    Path(_id): Path<Id>,
) -> AppResult<Json<AgentToken>> {
    unimplemented!("L1, groups")
}

/// `DELETE /api/agents/{id}/token`: the next request on it is 401.
pub async fn revoke_token(
    State(_state): State<AppState>,
    _p: CurrentPrincipal,
    Path(_id): Path<Id>,
) -> AppResult<StatusCode> {
    unimplemented!("L1, groups")
}
