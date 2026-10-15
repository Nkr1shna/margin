//! Groups, memberships and each person's expanded read set (L1-groups,
//! `docs/l1-brief.md` D1, D5).
//!
//! A group is a path (`margin_shared::is_group_path`); the tree is the paths. A person's
//! expansion is `person/<id>` ∪ `company` ∪ the ancestors of each direct membership,
//! cached in `App.expansions` and cleared whole after any person, group, membership or
//! agent write.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use margin_shared::{Group, GroupPatch, Id, NewGroup, OrgRole, ROOT_GROUP, person_id};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::db;
use crate::error::{ApiJson, AppResult};
use crate::people::PersonRow;
use crate::principal::{CurrentPrincipal, RequireAdmin};
use crate::state::AppState;

/// A stored group, keyed by its path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupRow {
    pub path: String,
    pub name: String,
    /// `margin_shared::SOURCE_MANUAL` for groups made through the API.
    pub source: String,
    pub created_at: DateTime<Utc>,
}

/// A direct membership.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MembershipRow {
    pub id: Id,
    pub person_id: Id,
    pub group_path: String,
    pub source: String,
}

/// What a person may read, expanded (D1).
#[derive(Debug, Clone, PartialEq)]
pub struct Expansion {
    pub person_id: Id,
    pub org_role: OrgRole,
    /// `person/<id>`, `company`, every direct group and its ancestors: sorted, unique.
    pub paths: Vec<String>,
    /// Own id and every direct and implied group (L2 evaluates `deny` against it).
    pub deny_ids: Vec<String>,
}

/// The expansion cache: one org, small tables, so any write clears it whole.
pub struct Expansions {
    map: Mutex<HashMap<Id, Arc<Expansion>>>,
    /// Bumped by every `invalidate_all`; event sockets watch it to re-expand.
    generation: watch::Sender<u64>,
}

impl Default for Expansions {
    fn default() -> Self {
        Self::new()
    }
}

impl Expansions {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            generation: watch::channel(0).0,
        }
    }

    pub fn get(&self, person_id: &str) -> Option<Arc<Expansion>> {
        self.map.lock().unwrap().get(person_id).cloned()
    }

    pub fn insert(&self, expansion: Arc<Expansion>) {
        self.map
            .lock()
            .unwrap()
            .insert(expansion.person_id.clone(), expansion);
    }

    /// Forgets every expansion and bumps the generation. Call after the write lock is
    /// released.
    pub fn invalidate_all(&self) {
        self.map.lock().unwrap().clear();
        self.generation.send_modify(|g| *g += 1);
    }

    pub fn generation(&self) -> u64 {
        *self.generation.borrow()
    }

    /// A receiver that changes with every `invalidate_all`.
    pub fn watch(&self) -> watch::Receiver<u64> {
        self.generation.subscribe()
    }
}

/// `person_id`'s expansion (cached). First commit: `[company, person/<id>]` with the
/// person's stored role (the bootstrap owner's `Owner` when there is no person row yet,
/// before `people-v1`); L1-groups adds the memberships and the cache.
pub async fn expansion(state: &AppState, person_id_: &str) -> AppResult<Arc<Expansion>> {
    let row: Option<PersonRow> = state.store.get(db::PERSON, person_id_).await?;
    let org_role = row.map(|r| r.person.org_role).unwrap_or(OrgRole::Owner);
    let mut paths = vec![ROOT_GROUP.to_string(), person_id(person_id_)];
    paths.sort();
    Ok(Arc::new(Expansion {
        person_id: person_id_.to_string(),
        org_role,
        deny_ids: paths.clone(),
        paths,
    }))
}

/// `GET /api/groups`: every group with its direct member count, sorted by path.
pub async fn list_groups(
    State(_state): State<AppState>,
    _p: CurrentPrincipal,
) -> AppResult<Json<Vec<Group>>> {
    unimplemented!("L1, groups")
}

/// `POST /api/groups` (admins).
pub async fn create_group(
    State(_state): State<AppState>,
    _admin: RequireAdmin,
    ApiJson(_new): ApiJson<NewGroup>,
) -> AppResult<Json<Group>> {
    unimplemented!("L1, groups")
}

/// `PATCH /api/groups/{*path}` (admins): rename.
pub async fn patch_group(
    State(_state): State<AppState>,
    _admin: RequireAdmin,
    Path(_path): Path<String>,
    ApiJson(_patch): ApiJson<GroupPatch>,
) -> AppResult<Json<Group>> {
    unimplemented!("L1, groups")
}

/// `DELETE /api/groups/{*path}` (admins): only an empty group, never `company`.
pub async fn delete_group(
    State(_state): State<AppState>,
    _admin: RequireAdmin,
    Path(_path): Path<String>,
) -> AppResult<StatusCode> {
    unimplemented!("L1, groups")
}
