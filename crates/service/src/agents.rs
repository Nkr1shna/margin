//! Agents a person owns, their ceilings and their `mga_` tokens (L1-groups,
//! `docs/l1-brief.md` D7).
//!
//! An agent acts for its owner within its ceiling: a set of principal ids and group
//! paths, stored ancestor-closed and inside the owner's expansion when it is written.
//! A request on its token reads `owner's expansion ∩ ceiling`, computed per request,
//! so what the owner loses later the agent loses too without rewriting the ceiling.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{Duration, Utc};
use margin_shared::context::is_agent_name;
use margin_shared::{
    AGENT_TOKEN_PREFIX, Agent, AgentKind, AgentPatch, AgentToken, AgentTokenInfo, Event, Id,
    NewAgent, OrgRole, Person, PersonSettings, PersonState, PrincipalId, access_code, agent_id,
    ancestors, handle_for_login, is_group_path, person_id,
};
use serde::{Deserialize, Serialize};

use crate::db;
use crate::error::{ApiJson, AppError, AppResult};
use crate::groups::{self, Expansion};
use crate::people::PersonRow;
use crate::principal::{BOOTSTRAP_PERSON_ID, CurrentPrincipal, Principal, PrincipalKind};
use crate::state::{AppState, Audience, new_id, random_token, sha256_hex};

/// How often `last_used_at` is written, at most (as devices' `last_seen_at`).
const TOUCH_EVERY: Duration = Duration::seconds(60);
/// Characters of a token kept to tell tokens apart ("mga_ab12").
const TOKEN_PREFIX_LEN: usize = 8;

/// A stored agent: the wire type (whose `token` holds the prefix, dates and expiry)
/// plus the token's hex SHA-256 (`agent_token` index). The token itself is never kept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRow {
    #[serde(flatten)]
    pub agent: Agent,
    #[serde(default)]
    pub token_hash: Option<String>,
}

// ---------------------------------------------------------------------------
// The principal of a token
// ---------------------------------------------------------------------------

/// The principal of an `mga_` token: `kind: Agent`, the owner's person, `org_role:
/// Member`, `paths` = owner's ∩ ceiling (D1). `None` for an unknown, revoked or expired
/// token, a disabled agent or a disabled owner.
pub async fn principal_for_token(
    state: &AppState,
    token: &str,
) -> AppResult<Option<Arc<Principal>>> {
    if !token.starts_with(AGENT_TOKEN_PREFIX) {
        return Ok(None);
    }
    let hash = sha256_hex(token);
    let rows: Vec<AgentRow> = state
        .store
        .list_where(db::AGENT, "token_hash", &hash)
        .await?;
    let Some(row) = rows.into_iter().next() else {
        return Ok(None);
    };
    let agent = row.agent;
    let now = Utc::now();
    let Some(info) = &agent.token else {
        return Ok(None);
    };
    if agent.disabled || info.expires_at.is_some_and(|at| at <= now) {
        return Ok(None);
    }
    let Some(owner) = owner_person(state, &agent.owner_id).await? else {
        return Ok(None);
    };
    if owner.state != PersonState::Active {
        return Ok(None);
    }
    let expansion = groups::expansion(state, &agent.owner_id).await?;
    if info
        .last_used_at
        .is_none_or(|at| now.signed_duration_since(at) >= TOUCH_EVERY)
    {
        touch(state, &agent.id, &hash).await?;
    }
    Ok(Some(Arc::new(agent_principal(&agent, &expansion, owner))))
}

/// The principal an agent acts as, from its owner's expansion.
pub fn agent_principal(agent: &Agent, owner: &Expansion, person: Person) -> Principal {
    let paths = intersect(&owner.paths, &agent.ceiling);
    let mut deny_ids: BTreeSet<String> = owner.deny_ids.iter().cloned().collect();
    deny_ids.insert(agent_id(&agent.id));
    Principal {
        kind: PrincipalKind::Agent {
            owner_id: agent.owner_id.clone(),
            agent_id: agent.id.clone(),
        },
        person_id: agent.owner_id.clone(),
        id: agent_id(&agent.id),
        paths,
        deny_ids: deny_ids.into_iter().collect(),
        org_role: OrgRole::Member,
        device: None,
        person,
    }
}

/// `a ∩ b`, sorted (both are ancestor-closed, so a plain set intersection).
fn intersect(a: &[String], b: &[String]) -> Vec<String> {
    let b: BTreeSet<&String> = b.iter().collect();
    let out: BTreeSet<String> = a.iter().filter(|p| b.contains(p)).cloned().collect();
    out.into_iter().collect()
}

/// The owner's person row; the bootstrap owner before `people-v1` has none.
async fn owner_person(state: &AppState, owner_id: &str) -> AppResult<Option<Person>> {
    if let Some(row) = state.store.get::<PersonRow>(db::PERSON, owner_id).await? {
        return Ok(Some(row.person));
    }
    if owner_id != BOOTSTRAP_PERSON_ID {
        return Ok(None);
    }
    let login = state.config.allowed_github_login.clone();
    Ok(Some(Person {
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
    }))
}

/// Writes `last_used_at` (the token may have been rotated meanwhile: then nothing).
async fn touch(state: &AppState, id: &str, hash: &str) -> AppResult<()> {
    let _w = state.write_lock.lock().await;
    if let Some(mut row) = state.store.get::<AgentRow>(db::AGENT, id).await?
        && row.token_hash.as_deref() == Some(hash)
        && let Some(info) = &mut row.agent.token
    {
        info.last_used_at = Some(Utc::now());
        state.store.put(db::AGENT, id, &row).await?;
    }
    Ok(())
}

/// The agent a principal is, if it is one (`/api/me` on an agent token).
pub async fn agent_of(state: &AppState, p: &Principal) -> AppResult<Option<Agent>> {
    match &p.kind {
        PrincipalKind::Agent { agent_id, .. } => Ok(state
            .store
            .get::<AgentRow>(db::AGENT, agent_id)
            .await?
            .map(|r| r.agent)),
        PrincipalKind::Person => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Ceilings
// ---------------------------------------------------------------------------

/// The ceiling to store (D7): `asked` ∪ the ancestors of each asked group, which must
/// lie inside `owner.paths` (400 `outside_owner` naming the first that doesn't), never
/// empty. `None` asks for the kind's default: `Margin` the owner's whole expansion,
/// `External` and `Voice` the expansion minus `person/<owner>` (owner, Q5).
pub fn normalise_ceiling(
    owner: &Expansion,
    kind: AgentKind,
    asked: Option<Vec<PrincipalId>>,
) -> AppResult<Vec<PrincipalId>> {
    let own = person_id(&owner.person_id);
    let ceiling: BTreeSet<String> = match asked {
        None => owner
            .paths
            .iter()
            .filter(|p| kind == AgentKind::Margin || **p != own)
            .cloned()
            .collect(),
        Some(asked) => {
            let mut out = BTreeSet::new();
            for entry in asked {
                let entry = entry.trim().to_string();
                if !owner.paths.contains(&entry) {
                    return Err(AppError::new(
                        StatusCode::BAD_REQUEST,
                        access_code::OUTSIDE_OWNER,
                        format!(
                            "{entry} is outside what you can read; an agent can only have access you have."
                        ),
                    ));
                }
                if is_group_path(&entry) {
                    // The owner's paths are ancestor-closed, so these are inside too.
                    out.extend(ancestors(&entry));
                } else {
                    out.insert(entry);
                }
            }
            out
        }
    };
    if ceiling.is_empty() {
        return Err(AppError::bad_request(
            "Give the agent access to at least one thing: a group or your private notes.",
        ));
    }
    Ok(ceiling.into_iter().collect())
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

fn not_owner() -> AppError {
    AppError::new(
        StatusCode::FORBIDDEN,
        access_code::NOT_OWNER,
        "Only the agent's owner can do this; an admin can only disable it.",
    )
}

fn check_name(name: &str) -> AppResult<String> {
    let name = name.trim();
    if !is_agent_name(name) {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "bad_name",
            "An agent's name is 1 to 40 lowercase letters, digits, - and _ (like claude-code).",
        ));
    }
    Ok(name.to_string())
}

/// 409 `exists` when another agent of `owner_id` (not `except`) has `name`.
async fn check_unique(
    state: &AppState,
    owner_id: &str,
    name: &str,
    except: Option<&str>,
) -> AppResult<()> {
    let mine: Vec<AgentRow> = state
        .store
        .list_where(db::AGENT, "owner_id", owner_id)
        .await?;
    if mine
        .iter()
        .any(|r| r.agent.name == name && Some(r.agent.id.as_str()) != except)
    {
        return Err(AppError::conflict(
            "exists",
            format!("You already have an agent called {name}."),
        ));
    }
    Ok(())
}

/// The agent `id` as the caller may see it: their own, or anyone's for an admin (404
/// otherwise, never saying it exists).
async fn visible(state: &AppState, p: &Principal, id: &str) -> AppResult<AgentRow> {
    match state.store.get::<AgentRow>(db::AGENT, id).await? {
        Some(row) if row.agent.owner_id == p.person_id || p.is_admin() => Ok(row),
        _ => Err(AppError::not_found("agent")),
    }
}

/// The agent `id`, which must be the caller's (an admin seeing someone else's: 403).
async fn owned(state: &AppState, p: &Principal, id: &str) -> AppResult<AgentRow> {
    let row = visible(state, p, id).await?;
    if row.agent.owner_id != p.person_id {
        return Err(not_owner());
    }
    Ok(row)
}

/// After an agent write, with the lock released.
fn agent_written(state: &AppState, agent: &Agent) {
    state.expansions.invalidate_all();
    state.emit_to(
        Audience::PersonOrAdmins(agent.owner_id.clone()),
        Event::AgentChanged {
            agent: agent.clone(),
        },
    );
}

fn sorted(mut agents: Vec<Agent>) -> Vec<Agent> {
    agents.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    agents
}

/// `GET /api/agents`: the caller's agents; an admin's call lists everyone's.
pub async fn list_agents(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
) -> AppResult<Json<Vec<Agent>>> {
    let rows: Vec<AgentRow> = if p.is_admin() {
        state.store.list(db::AGENT).await?
    } else {
        state
            .store
            .list_where(db::AGENT, "owner_id", &p.person_id)
            .await?
    };
    Ok(Json(sorted(rows.into_iter().map(|r| r.agent).collect())))
}

/// `POST /api/agents`: an agent of the caller's.
pub async fn create_agent(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    ApiJson(new): ApiJson<NewAgent>,
) -> AppResult<Json<Agent>> {
    let name = check_name(&new.name)?;
    let owner = groups::expansion(&state, &p.person_id).await?;
    let ceiling = normalise_ceiling(&owner, new.kind, new.ceiling)?;
    let agent = {
        let _w = state.write_lock.lock().await;
        check_unique(&state, &p.person_id, &name, None).await?;
        let agent = Agent {
            id: new_id(),
            owner_id: p.person_id.clone(),
            name,
            kind: new.kind,
            ceiling,
            disabled: false,
            token: None,
            created_at: Utc::now(),
        };
        let row = AgentRow {
            agent: agent.clone(),
            token_hash: None,
        };
        state.store.put(db::AGENT, &agent.id, &row).await?;
        agent
    };
    agent_written(&state, &agent);
    Ok(Json(agent))
}

/// `PATCH /api/agents/{id}`: its owner; an admin may only set `disabled`.
pub async fn patch_agent(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    Path(id): Path<Id>,
    ApiJson(patch): ApiJson<AgentPatch>,
) -> AppResult<Json<Agent>> {
    let row = visible(&state, &p, &id).await?;
    let mine = row.agent.owner_id == p.person_id;
    if !mine && (patch.name.is_some() || patch.ceiling.is_some()) {
        return Err(not_owner());
    }
    let name = patch.name.as_deref().map(check_name).transpose()?;
    let ceiling = match patch.ceiling {
        Some(asked) => {
            let owner = groups::expansion(&state, &row.agent.owner_id).await?;
            Some(normalise_ceiling(&owner, row.agent.kind, Some(asked))?)
        }
        None => None,
    };
    let agent = {
        let _w = state.write_lock.lock().await;
        let mut row = state
            .store
            .get::<AgentRow>(db::AGENT, &id)
            .await?
            .ok_or_else(|| AppError::not_found("agent"))?;
        if let Some(name) = name {
            check_unique(&state, &row.agent.owner_id, &name, Some(&id)).await?;
            row.agent.name = name;
        }
        if let Some(ceiling) = ceiling {
            row.agent.ceiling = ceiling;
        }
        if let Some(disabled) = patch.disabled {
            row.agent.disabled = disabled;
        }
        state.store.put(db::AGENT, &id, &row).await?;
        row.agent
    };
    agent_written(&state, &agent);
    Ok(Json(agent))
}

/// `DELETE /api/agents/{id}`: its owner; the token dies with it.
pub async fn delete_agent(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    Path(id): Path<Id>,
) -> AppResult<StatusCode> {
    let owner_id = {
        let _w = state.write_lock.lock().await;
        let row = owned(&state, &p, &id).await?;
        state.store.delete(db::AGENT, &id).await?;
        row.agent.owner_id
    };
    state.expansions.invalidate_all();
    state.emit_to(
        Audience::PersonOrAdmins(owner_id),
        Event::AgentDeleted { id },
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/agents/{id}/token`: mints `mga_` + `state::random_token()`, replacing any
/// previous token, and returns it once.
pub async fn mint_token(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    Path(id): Path<Id>,
) -> AppResult<Json<AgentToken>> {
    let token = format!("{AGENT_TOKEN_PREFIX}{}", random_token());
    let agent = {
        let _w = state.write_lock.lock().await;
        let mut row = owned(&state, &p, &id).await?;
        row.agent.token = Some(AgentTokenInfo {
            prefix: token.chars().take(TOKEN_PREFIX_LEN).collect(),
            created_at: Utc::now(),
            last_used_at: None,
            expires_at: None,
        });
        row.token_hash = Some(sha256_hex(&token));
        state.store.put(db::AGENT, &id, &row).await?;
        row.agent
    };
    agent_written(&state, &agent);
    Ok(Json(AgentToken { token, agent }))
}

/// `DELETE /api/agents/{id}/token`: the next request on it is 401.
pub async fn revoke_token(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    Path(id): Path<Id>,
) -> AppResult<StatusCode> {
    let agent = {
        let _w = state.write_lock.lock().await;
        let mut row = owned(&state, &p, &id).await?;
        row.agent.token = None;
        row.token_hash = None;
        state.store.put(db::AGENT, &id, &row).await?;
        row.agent
    };
    agent_written(&state, &agent);
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::groups::expand;

    fn owner() -> Expansion {
        expand("o", OrgRole::Member, &["company/eng/web".into()])
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn default_ceilings_by_kind() {
        let o = owner();
        assert_eq!(
            normalise_ceiling(&o, AgentKind::Margin, None).unwrap(),
            s(&["company", "company/eng", "company/eng/web", "person/o"])
        );
        for kind in [AgentKind::External, AgentKind::Voice] {
            assert_eq!(
                normalise_ceiling(&o, kind, None).unwrap(),
                s(&["company", "company/eng", "company/eng/web"])
            );
        }
    }

    #[test]
    fn asked_ceilings_are_closed_and_inside_the_owner() {
        let o = owner();
        assert_eq!(
            normalise_ceiling(&o, AgentKind::External, Some(s(&["company/eng"]))).unwrap(),
            s(&["company", "company/eng"])
        );
        assert_eq!(
            normalise_ceiling(&o, AgentKind::External, Some(s(&["person/o"]))).unwrap(),
            s(&["person/o"])
        );
        let e =
            normalise_ceiling(&o, AgentKind::External, Some(s(&["company/sales"]))).unwrap_err();
        assert_eq!(e.code, access_code::OUTSIDE_OWNER);
        assert!(e.message.contains("company/sales"));
        let e = normalise_ceiling(&o, AgentKind::Margin, Some(s(&["person/someone"]))).unwrap_err();
        assert_eq!(e.code, access_code::OUTSIDE_OWNER);
        assert!(normalise_ceiling(&o, AgentKind::Margin, Some(vec![])).is_err());
    }

    /// Over random trees and random asks: the stored ceiling is ancestor-closed and a
    /// subset of the owner's expansion, or the ask is refused for naming something
    /// outside it.
    #[test]
    fn ceilings_hold_over_random_trees() {
        use rand::RngExt;
        let mut rng = rand::rng();
        for _ in 0..500 {
            let mut groups: Vec<String> = Vec::new();
            for _ in 0..rng.random_range(0..4) {
                let depth = rng.random_range(1..4);
                let mut path = "company".to_string();
                for _ in 0..depth {
                    path.push_str(&format!("/g{}", rng.random_range(0..3)));
                }
                groups.push(path);
            }
            let o = expand("o", OrgRole::Member, &groups);
            let mut asked = Vec::new();
            for _ in 0..rng.random_range(0..4) {
                let depth = rng.random_range(0..4);
                let mut path = "company".to_string();
                for _ in 0..depth {
                    path.push_str(&format!("/g{}", rng.random_range(0..3)));
                }
                asked.push(if rng.random_bool(0.2) {
                    "person/o".to_string()
                } else {
                    path
                });
            }
            match normalise_ceiling(&o, AgentKind::External, Some(asked.clone())) {
                Ok(c) => {
                    assert!(!c.is_empty());
                    for entry in &c {
                        assert!(o.paths.contains(entry), "{entry} ∉ {:?}", o.paths);
                        for a in ancestors(entry) {
                            assert!(c.contains(&a), "{a} missing from {c:?}");
                        }
                    }
                    for a in &asked {
                        assert!(c.contains(a));
                    }
                }
                Err(e) => assert!(
                    e.code == access_code::OUTSIDE_OWNER
                        && asked.iter().any(|a| !o.paths.contains(a))
                        || asked.is_empty()
                ),
            }
        }
    }

    #[test]
    fn an_agent_reads_its_owners_paths_within_its_ceiling() {
        let o = owner();
        let person = Person {
            id: "o".into(),
            github_login: "o".into(),
            github_id: None,
            handle: "oo".into(),
            name: "O".into(),
            avatar_url: None,
            org_role: OrgRole::Admin,
            state: PersonState::Active,
            groups: vec![],
            settings: PersonSettings::default(),
            created_at: Utc::now(),
        };
        let agent = Agent {
            id: "a1".into(),
            owner_id: "o".into(),
            name: "bot".into(),
            kind: AgentKind::External,
            // `company/sales` was the owner's once: the request-time ∩ drops it.
            ceiling: s(&["company", "company/eng", "company/sales"]),
            disabled: false,
            token: None,
            created_at: Utc::now(),
        };
        let p = agent_principal(&agent, &o, person);
        assert_eq!(p.paths, s(&["company", "company/eng"]));
        assert_eq!(p.id, "agent/a1");
        assert_eq!(p.person_id, "o");
        assert_eq!(p.org_role, OrgRole::Member);
        assert!(p.is_agent() && !p.is_admin() && p.device.is_none());
        assert!(p.deny_ids.contains(&"agent/a1".to_string()));
    }
}
