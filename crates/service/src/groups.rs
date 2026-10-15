//! Groups, memberships and each person's expanded read set (L1-groups,
//! `docs/l1-brief.md` D1, D5).
//!
//! A group is a path (`margin_shared::is_group_path`); the tree is the paths. A person's
//! expansion is `person/<id>` ∪ `company` ∪ the ancestors of each direct membership,
//! cached in `App.expansions` and cleared whole after any person, group, membership or
//! agent write.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use margin_shared::context::AuditKind;
use margin_shared::{
    Event, Group, GroupPatch, Id, NewGroup, OrgRole, ROOT_GROUP, SOURCE_MANUAL, access_code,
    ancestors, is_group_path, person_id,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::audit;
use crate::db::{self, Store};
use crate::error::{ApiJson, AppError, AppResult};
use crate::people::PersonRow;
use crate::principal::{BOOTSTRAP_PERSON_ID, CurrentPrincipal, Principal, RequireAdmin};
use crate::state::{AppState, Audience};

/// Longest group name.
pub const GROUP_NAME_MAX: usize = 80;
/// What the root group is called when it is first stored.
pub const ROOT_GROUP_NAME: &str = "Company";

/// A stored group, keyed by its path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupRow {
    pub path: String,
    pub name: String,
    /// `margin_shared::SOURCE_MANUAL` for groups made through the API.
    pub source: String,
    pub created_at: DateTime<Utc>,
}

impl GroupRow {
    fn to_group(&self, members: u32) -> Group {
        Group {
            path: self.path.clone(),
            name: self.name.clone(),
            source: self.source.clone(),
            members,
            created_at: self.created_at,
        }
    }
}

/// A direct membership.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MembershipRow {
    pub id: Id,
    pub person_id: Id,
    pub group_path: String,
    pub source: String,
}

impl MembershipRow {
    /// One row per person and group: the key makes writing it twice harmless.
    pub fn key(person_id: &str, group_path: &str) -> String {
        format!("{person_id}|{group_path}")
    }
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

    /// Caches `expansion` unless an `invalidate_all` happened since `generation` was
    /// read (it was computed from rows that may have changed meanwhile).
    pub fn insert_if_current(&self, generation: u64, expansion: Arc<Expansion>) {
        let mut map = self.map.lock().unwrap();
        if *self.generation.borrow() == generation {
            map.insert(expansion.person_id.clone(), expansion);
        }
    }

    /// Forgets every expansion and bumps the generation. Call after the write lock is
    /// released.
    pub fn invalidate_all(&self) {
        // Bumped under the map's lock, so `insert_if_current` sees clear-and-bump as one.
        let mut map = self.map.lock().unwrap();
        map.clear();
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

// ---------------------------------------------------------------------------
// Expansion
// ---------------------------------------------------------------------------

/// `person_id`'s expansion (cached): `person/<id>` ∪ `company` ∪ the ancestors of each
/// direct membership, with the person's role. A person id without a row is a `Member`
/// with those two paths, except the bootstrap owner before `people-v1` (`Owner`).
pub async fn expansion(state: &AppState, person_id_: &str) -> AppResult<Arc<Expansion>> {
    if let Some(e) = state.expansions.get(person_id_) {
        return Ok(e);
    }
    let generation = state.expansions.generation();
    let e = Arc::new(compute_expansion(&state.store, person_id_).await?);
    state.expansions.insert_if_current(generation, e.clone());
    Ok(e)
}

/// `expansion` without the cache.
pub async fn compute_expansion(store: &Store, person_id_: &str) -> anyhow::Result<Expansion> {
    let row: Option<PersonRow> = store.get(db::PERSON, person_id_).await?;
    let org_role = match row {
        Some(r) => r.person.org_role,
        None if person_id_ == BOOTSTRAP_PERSON_ID => OrgRole::Owner,
        None => OrgRole::Member,
    };
    let direct = groups_of(store, person_id_).await?;
    Ok(expand(person_id_, org_role, &direct))
}

/// The expansion of a person with these direct memberships.
pub fn expand(person_id_: &str, org_role: OrgRole, direct: &[String]) -> Expansion {
    let mut paths: BTreeSet<String> = BTreeSet::new();
    paths.insert(ROOT_GROUP.to_string());
    paths.insert(person_id(person_id_));
    for group in direct {
        paths.extend(ancestors(group));
    }
    let paths: Vec<String> = paths.into_iter().collect();
    Expansion {
        person_id: person_id_.to_string(),
        org_role,
        deny_ids: paths.clone(),
        paths,
    }
}

// ---------------------------------------------------------------------------
// Groups and memberships (people, invitations and `people-v1` use these too)
// ---------------------------------------------------------------------------

/// Whether `path` is a group: `company` always is.
pub async fn group_exists(store: &Store, path: &str) -> anyhow::Result<bool> {
    if path == ROOT_GROUP {
        return Ok(true);
    }
    Ok(store.get::<GroupRow>(db::GROUP, path).await?.is_some())
}

/// Stores the `company` group if it isn't stored yet (`people-v1` calls it). Call with
/// the write lock held.
pub async fn ensure_root_locked(store: &Store) -> anyhow::Result<GroupRow> {
    if let Some(row) = store.get::<GroupRow>(db::GROUP, ROOT_GROUP).await? {
        return Ok(row);
    }
    let row = root_row();
    store.put(db::GROUP, ROOT_GROUP, &row).await?;
    Ok(row)
}

fn root_row() -> GroupRow {
    GroupRow {
        path: ROOT_GROUP.into(),
        name: ROOT_GROUP_NAME.into(),
        source: SOURCE_MANUAL.into(),
        created_at: Utc::now(),
    }
}

/// A person's direct membership rows, sorted by path.
pub async fn memberships_of(store: &Store, person_id_: &str) -> anyhow::Result<Vec<MembershipRow>> {
    let mut rows: Vec<MembershipRow> = store
        .list_where(db::MEMBERSHIP, "person_id", person_id_)
        .await?;
    rows.sort_by(|a, b| a.group_path.cmp(&b.group_path));
    Ok(rows)
}

/// A person's direct groups, sorted (what `Person.groups` holds).
pub async fn groups_of(store: &Store, person_id_: &str) -> anyhow::Result<Vec<String>> {
    let mut groups: Vec<String> = memberships_of(store, person_id_)
        .await?
        .into_iter()
        .map(|m| m.group_path)
        .collect();
    groups.dedup();
    Ok(groups)
}

/// Every person's direct groups, sorted, by person id (for listing people).
pub async fn all_groups(store: &Store) -> anyhow::Result<HashMap<Id, Vec<String>>> {
    let rows: Vec<MembershipRow> = store.list(db::MEMBERSHIP).await?;
    let mut out: HashMap<Id, Vec<String>> = HashMap::new();
    for row in rows {
        out.entry(row.person_id).or_default().push(row.group_path);
    }
    for groups in out.values_mut() {
        groups.sort();
        groups.dedup();
    }
    Ok(out)
}

/// 400 `unknown_group` naming the first of `groups` that isn't a group.
pub async fn check_groups_exist(store: &Store, groups: &[String]) -> AppResult<()> {
    for path in groups {
        if !is_group_path(path) || !group_exists(store, path).await? {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                access_code::UNKNOWN_GROUP,
                format!("There's no group {path}. Create it first under Organisation → Groups."),
            ));
        }
    }
    Ok(())
}

/// Replaces `person_id`'s direct `manual` memberships with `groups` (deduplicated;
/// memberships from another source are kept). Every path must be a group (400
/// `unknown_group`). Returns the person's direct groups afterwards. Call with the write
/// lock held; once it is released, `state.expansions.invalidate_all()` and
/// `memberships_changed` (or call `set_memberships`, which does all three).
pub async fn set_memberships_locked(
    store: &Store,
    person_id_: &str,
    groups: &[String],
) -> AppResult<Vec<String>> {
    let wanted: BTreeSet<String> = groups.iter().map(|g| g.trim().to_string()).collect();
    let wanted: Vec<String> = wanted.into_iter().collect();
    check_groups_exist(store, &wanted).await?;
    if wanted.iter().any(|g| g == ROOT_GROUP) {
        ensure_root_locked(store).await?;
    }
    let existing = memberships_of(store, person_id_).await?;
    for row in &existing {
        if row.source == SOURCE_MANUAL && !wanted.contains(&row.group_path) {
            store.delete(db::MEMBERSHIP, &row.id).await?;
        }
    }
    for path in &wanted {
        if existing.iter().any(|r| &r.group_path == path) {
            continue;
        }
        let id = MembershipRow::key(person_id_, path);
        let row = MembershipRow {
            id: id.clone(),
            person_id: person_id_.to_string(),
            group_path: path.clone(),
            source: SOURCE_MANUAL.into(),
        };
        store.put(db::MEMBERSHIP, &id, &row).await?;
    }
    Ok(groups_of(store, person_id_).await?)
}

/// `set_memberships_locked` under the write lock; then the cache is cleared and
/// `memberships_changed` goes out. What `PUT /api/people/{id}/groups` calls.
pub async fn set_memberships(
    state: &AppState,
    person_id_: &str,
    groups: &[String],
) -> AppResult<Vec<String>> {
    let now = {
        let _w = state.write_lock.lock().await;
        set_memberships_locked(&state.store, person_id_, groups).await?
    };
    state.expansions.invalidate_all();
    memberships_changed(state, person_id_, &now);
    Ok(now)
}

/// `set_memberships` made by `actor` (`PUT /api/people/{id}/groups`), journaled as a
/// `MembershipChange` with the person's direct groups before and after, under the same
/// write lock. Nothing is journaled when nothing changed.
pub async fn set_memberships_by(
    state: &AppState,
    actor: &Principal,
    person_id_: &str,
    groups: &[String],
) -> AppResult<Vec<String>> {
    let now = {
        let _w = state.write_lock.lock().await;
        let before = groups_of(&state.store, person_id_).await?;
        let now = set_memberships_locked(&state.store, person_id_, groups).await?;
        if before != now {
            audit::record(
                state,
                audit::by(
                    actor,
                    AuditKind::MembershipChange,
                    "person",
                    person_id_,
                    json!({"groups": before}),
                    json!({"groups": now}),
                ),
            );
        }
        now
    };
    state.expansions.invalidate_all();
    memberships_changed(state, person_id_, &now);
    Ok(now)
}

/// The events of a membership change: `MembershipChanged` to everyone (a directory),
/// `AccessChanged` to the person.
pub fn memberships_changed(state: &AppState, person_id_: &str, groups: &[String]) {
    state.emit(Event::MembershipChanged {
        person_id: person_id_.to_string(),
        groups: groups.to_vec(),
    });
    state.emit_to(
        Audience::Person(person_id_.to_string()),
        Event::AccessChanged {
            person_id: person_id_.to_string(),
        },
    );
}

/// Every group with its direct member count, sorted by path; `company` is listed even
/// before it is stored.
pub async fn list(store: &Store) -> anyhow::Result<Vec<Group>> {
    let mut rows: BTreeMap<String, GroupRow> = store
        .list::<GroupRow>(db::GROUP)
        .await?
        .into_iter()
        .map(|r| (r.path.clone(), r))
        .collect();
    rows.entry(ROOT_GROUP.to_string()).or_insert_with(root_row);
    let mut counts: HashMap<String, u32> = HashMap::new();
    for m in store.list::<MembershipRow>(db::MEMBERSHIP).await? {
        *counts.entry(m.group_path).or_default() += 1;
    }
    Ok(rows
        .values()
        .map(|r| r.to_group(counts.get(&r.path).copied().unwrap_or(0)))
        .collect())
}

/// The parent of a group path (`None` for `company`).
pub fn parent(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(parent, _)| parent)
}

fn group_name(name: &str) -> AppResult<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::bad_request("Give the group a name."));
    }
    if name.chars().count() > GROUP_NAME_MAX {
        return Err(AppError::bad_request(format!(
            "A group's name is at most {GROUP_NAME_MAX} characters."
        )));
    }
    Ok(name.to_string())
}

fn managed_by_source(source: &str) -> AppError {
    AppError::new(
        StatusCode::BAD_REQUEST,
        access_code::MANAGED_BY_SOURCE,
        format!("This group comes from {source}; change it there."),
    )
}

async fn member_count(store: &Store, path: &str) -> anyhow::Result<u32> {
    Ok(store
        .list_where::<MembershipRow>(db::MEMBERSHIP, "group_path", path)
        .await?
        .len() as u32)
}

/// After a group write, with the lock released: the cache is cleared and the group
/// goes to everyone.
fn group_written(state: &AppState, group: Group) {
    state.expansions.invalidate_all();
    state.emit(Event::GroupChanged { group });
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// `GET /api/groups`: every group with its direct member count, sorted by path.
pub async fn list_groups(
    State(state): State<AppState>,
    _p: CurrentPrincipal,
) -> AppResult<Json<Vec<Group>>> {
    Ok(Json(list(&state.store).await?))
}

/// `POST /api/groups` (admins).
pub async fn create_group(
    State(state): State<AppState>,
    RequireAdmin(admin): RequireAdmin,
    ApiJson(new): ApiJson<NewGroup>,
) -> AppResult<Json<Group>> {
    let path = new.path.trim().to_string();
    if !is_group_path(&path) {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "bad_path",
            "A group's path is company/… in lowercase letters, digits and dashes, at most 8 levels deep.",
        ));
    }
    let name = group_name(&new.name)?;
    let group = {
        let _w = state.write_lock.lock().await;
        if path == ROOT_GROUP
            || state
                .store
                .get::<GroupRow>(db::GROUP, &path)
                .await?
                .is_some()
        {
            return Err(AppError::conflict(
                "exists",
                format!("There's already a group {path}."),
            ));
        }
        let parent = parent(&path).unwrap_or(ROOT_GROUP);
        if !group_exists(&state.store, parent).await? {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                access_code::UNKNOWN_GROUP,
                format!("There's no group {parent} to put it under. Create that first."),
            ));
        }
        if parent == ROOT_GROUP {
            ensure_root_locked(&state.store).await?;
        }
        let row = GroupRow {
            path: path.clone(),
            name,
            source: SOURCE_MANUAL.into(),
            created_at: Utc::now(),
        };
        state.store.put(db::GROUP, &path, &row).await?;
        audit::record(
            &state,
            audit::by(
                &admin,
                AuditKind::GroupChange,
                "group",
                &path,
                Value::Null,
                json!({"event": "created", "path": path, "parent": parent}),
            ),
        );
        row.to_group(0)
    };
    group_written(&state, group.clone());
    Ok(Json(group))
}

/// `PATCH /api/groups/{*path}` (admins): rename (moving a path is not L1).
pub async fn patch_group(
    State(state): State<AppState>,
    RequireAdmin(admin): RequireAdmin,
    Path(path): Path<String>,
    ApiJson(patch): ApiJson<GroupPatch>,
) -> AppResult<Json<Group>> {
    let group = {
        let _w = state.write_lock.lock().await;
        let mut row = if path == ROOT_GROUP {
            ensure_root_locked(&state.store).await?
        } else {
            state
                .store
                .get::<GroupRow>(db::GROUP, &path)
                .await?
                .ok_or_else(|| AppError::not_found("group"))?
        };
        if row.source != SOURCE_MANUAL {
            return Err(managed_by_source(&row.source));
        }
        if let Some(name) = &patch.name {
            let name = group_name(name)?;
            if name != row.name {
                row.name = name;
                state.store.put(db::GROUP, &path, &row).await?;
                // The name itself is never journaled (the Audit view joins it).
                audit::record(
                    &state,
                    audit::by(
                        &admin,
                        AuditKind::GroupChange,
                        "group",
                        &path,
                        json!({"path": path}),
                        json!({"event": "renamed", "path": path}),
                    ),
                );
            }
        }
        row.to_group(member_count(&state.store, &path).await?)
    };
    group_written(&state, group.clone());
    Ok(Json(group))
}

/// `DELETE /api/groups/{*path}` (admins): only an empty group, never `company`.
pub async fn delete_group(
    State(state): State<AppState>,
    RequireAdmin(admin): RequireAdmin,
    Path(path): Path<String>,
) -> AppResult<StatusCode> {
    if path == ROOT_GROUP {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "root_group",
            "The company group holds everyone; it can't be deleted.",
        ));
    }
    {
        let _w = state.write_lock.lock().await;
        let row = state
            .store
            .get::<GroupRow>(db::GROUP, &path)
            .await?
            .ok_or_else(|| AppError::not_found("group"))?;
        if row.source != SOURCE_MANUAL {
            return Err(managed_by_source(&row.source));
        }
        let prefix = format!("{path}/");
        let children = state
            .store
            .list::<GroupRow>(db::GROUP)
            .await?
            .into_iter()
            .filter(|g| g.path.starts_with(&prefix))
            .count();
        if children > 0 {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                access_code::HAS_CHILDREN,
                format!(
                    "{path} has {children} group{} under it. Delete those first.",
                    if children == 1 { "" } else { "s" }
                ),
            ));
        }
        let members = member_count(&state.store, &path).await?;
        if members > 0 {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                access_code::HAS_MEMBERS,
                format!(
                    "{path} has {members} member{}. Take them out of it first.",
                    if members == 1 { "" } else { "s" }
                ),
            ));
        }
        state.store.delete(db::GROUP, &path).await?;
        audit::record(
            &state,
            audit::by(
                &admin,
                AuditKind::GroupChange,
                "group",
                &path,
                json!({"path": path}),
                json!({"event": "deleted"}),
            ),
        );
    }
    state.expansions.invalidate_all();
    state.emit(Event::GroupDeleted { path });
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_adds_ancestors_and_the_person() {
        let e = expand(
            "p1",
            OrgRole::Member,
            &["company/eng/web".into(), "company/sales".into()],
        );
        assert_eq!(
            e.paths,
            vec![
                "company",
                "company/eng",
                "company/eng/web",
                "company/sales",
                "person/p1"
            ]
        );
        assert_eq!(e.deny_ids, e.paths);
        assert_eq!(
            expand("p2", OrgRole::Admin, &[]).paths,
            vec!["company", "person/p2"]
        );
    }

    #[test]
    fn parents() {
        assert_eq!(parent("company/a/b"), Some("company/a"));
        assert_eq!(parent("company/a"), Some("company"));
        assert_eq!(parent("company"), None);
    }

    #[test]
    fn a_stale_expansion_is_not_cached() {
        let cache = Expansions::new();
        let g = cache.generation();
        cache.invalidate_all();
        cache.insert_if_current(g, Arc::new(expand("p", OrgRole::Member, &[])));
        assert!(cache.get("p").is_none());
        let g = cache.generation();
        cache.insert_if_current(g, Arc::new(expand("p", OrgRole::Member, &[])));
        assert!(cache.get("p").is_some());
        let mut rx = cache.watch();
        rx.mark_unchanged();
        cache.invalidate_all();
        assert!(rx.has_changed().unwrap());
        assert!(cache.get("p").is_none());
    }

    #[tokio::test]
    async fn memberships_replace_only_manual_rows() {
        let store = Store::memory().await.unwrap();
        for path in ["company/eng", "company/sales"] {
            let row = GroupRow {
                path: path.into(),
                name: path.into(),
                source: SOURCE_MANUAL.into(),
                created_at: Utc::now(),
            };
            store.put(db::GROUP, path, &row).await.unwrap();
        }
        let synced = MembershipRow {
            id: MembershipRow::key("p", "company/sales"),
            person_id: "p".into(),
            group_path: "company/sales".into(),
            source: "github-org".into(),
        };
        store
            .put(db::MEMBERSHIP, &synced.id, &synced)
            .await
            .unwrap();
        let now =
            set_memberships_locked(&store, "p", &["company/eng".into(), "company/eng".into()])
                .await
                .unwrap();
        assert_eq!(now, vec!["company/eng", "company/sales"]);
        let now = set_memberships_locked(&store, "p", &[]).await.unwrap();
        assert_eq!(now, vec!["company/sales"]);
        let e = set_memberships_locked(&store, "p", &["company/nope".into()])
            .await
            .unwrap_err();
        assert_eq!(e.code, access_code::UNKNOWN_GROUP);
        let e = compute_expansion(&store, "p").await.unwrap();
        assert_eq!(e.paths, vec!["company", "company/sales", "person/p"]);
        assert_eq!(e.org_role, OrgRole::Member);
        assert_eq!(
            compute_expansion(&store, BOOTSTRAP_PERSON_ID)
                .await
                .unwrap()
                .org_role,
            OrgRole::Owner
        );
        // `company` is listed before it is stored, with its members counted.
        set_memberships_locked(&store, "q", &[ROOT_GROUP.into()])
            .await
            .unwrap();
        let groups = list(&store).await.unwrap();
        assert_eq!(
            groups
                .iter()
                .map(|g| (g.path.as_str(), g.members))
                .collect::<Vec<_>>(),
            vec![("company", 1), ("company/eng", 0), ("company/sales", 1)]
        );
    }
}
