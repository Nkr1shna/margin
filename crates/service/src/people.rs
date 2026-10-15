//! People: who may sign in, the `people-v1` migration, admission at the GitHub callback,
//! and the people routes (L1-auth, `docs/l1-brief.md` D2, D3, D4, D9).

use std::collections::BTreeSet;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::Utc;
use margin_shared::context::AuditKind;
use margin_shared::{
    Event, Id, OrgRole, Person, PersonPatch, PersonSettings, PersonState, ROOT_GROUP, access_code,
    handle_for_login, is_group_path, is_handle, person_id,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::audit;
use crate::db;
use crate::error::{ApiJson, AppError, AppResult};
use crate::groups;
use crate::invitations::InvitationRow;
use crate::principal::{CurrentPrincipal, Principal, RequireAdmin, admin_required};
use crate::state::{AppState, Audience, DeviceRow, SETTINGS_ID, new_id};

/// The one-time migration that creates the bootstrap owner and hangs every existing row
/// on them (`SettingsRow.migrations`).
pub const MIGRATION: &str = "people-v1";

/// Longest display name.
pub const NAME_MAX: usize = 100;
/// Longest avatar URL.
pub const AVATAR_URL_MAX: usize = 2048;

/// A stored person: the wire `Person` plus the login lowercased, for the unique index
/// (`person_login`). `person.groups` is filled from `membership` rows on the way out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersonRow {
    #[serde(flatten)]
    pub person: Person,
    pub login_key: String,
}

impl PersonRow {
    pub fn new(person: Person) -> Self {
        let login_key = person.github_login.to_lowercase();
        Self { person, login_key }
    }
}

/// What the GitHub callback does with a login (D3).
#[derive(Debug, Clone, PartialEq)]
pub enum Admission {
    /// An active person with that login.
    Person(Person),
    /// A pending invitation, turned into a person just now.
    Invited(Person),
    /// The sentence the sign-in page shows.
    Denied(String),
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// The stored person with `login` (any case).
pub async fn person_by_login(state: &AppState, login: &str) -> anyhow::Result<Option<PersonRow>> {
    let rows: Vec<PersonRow> = state
        .store
        .list_where(db::PERSON, "login_key", &login.to_lowercase())
        .await?;
    Ok(rows.into_iter().next())
}

/// `person_id`'s direct memberships (group paths), sorted (`groups::groups_of`).
pub async fn groups_of(state: &AppState, person_id_: &str) -> anyhow::Result<Vec<String>> {
    groups::groups_of(&state.store, person_id_).await
}

/// The wire person: `groups` from the memberships; `settings` only when `own`.
pub async fn public_person(state: &AppState, row: &PersonRow, own: bool) -> AppResult<Person> {
    let mut person = row.person.clone();
    person.groups = groups_of(state, &person.id).await?;
    if !own {
        person.settings = PersonSettings::default();
    }
    Ok(person)
}

/// The person `id` as stored.
pub async fn person_row(state: &AppState, id: &str) -> AppResult<Option<PersonRow>> {
    Ok(state.store.get::<PersonRow>(db::PERSON, id).await?)
}

/// A handle for `login` no other person has: the lowercased login, else `-2`, `-3`, ….
async fn free_handle(state: &AppState, login: &str) -> anyhow::Result<String> {
    let base = handle_for_login(login);
    let base = if is_handle(&base) {
        base
    } else {
        // GitHub logins are handles already; a one-letter login is padded.
        format!("{base}-1")
    };
    let rows: Vec<PersonRow> = state.store.list(db::PERSON).await?;
    let taken: BTreeSet<String> = rows.into_iter().map(|r| r.person.handle).collect();
    if !taken.contains(&base) {
        return Ok(base);
    }
    for n in 2.. {
        let suffix = format!("-{n}");
        let stem: String = base.chars().take(39 - suffix.len()).collect();
        let candidate = format!("{}{suffix}", stem.trim_end_matches('-'));
        if !taken.contains(&candidate) {
            return Ok(candidate);
        }
    }
    unreachable!()
}

/// The paths of `groups` that aren't existing groups (`groups::group_exists`).
pub async fn unknown_groups(state: &AppState, groups: &[String]) -> AppResult<Vec<String>> {
    let mut unknown = Vec::new();
    for g in groups {
        if !is_group_path(g) || !groups::group_exists(&state.store, g).await? {
            unknown.push(g.clone());
        }
    }
    Ok(unknown)
}

/// 400 `unknown_group` naming the first path that isn't a group
/// (`groups::check_groups_exist`).
pub async fn check_groups(state: &AppState, groups: &[String]) -> AppResult<()> {
    groups::check_groups_exist(&state.store, groups).await
}

/// Sorted and without repeats.
pub fn tidy_groups(groups: Vec<String>) -> Vec<String> {
    groups
        .into_iter()
        .map(|g| g.trim().to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

// ---------------------------------------------------------------------------
// people-v1
// ---------------------------------------------------------------------------

/// A row of any table, kept whole as JSON (`put_many` needs its key).
#[derive(Serialize)]
#[serde(transparent)]
struct RawRow(serde_json::Value);

impl db::HasKey for RawRow {
    fn key(&self) -> String {
        self.0
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    }
}

fn has_owner(row: &serde_json::Value) -> bool {
    row.get("owner").is_some_and(|o| o.is_string())
}

/// Whether `people-v1` has run.
pub async fn migration_done(state: &AppState) -> anyhow::Result<bool> {
    Ok(state
        .settings_row()
        .await?
        .migrations
        .iter()
        .any(|m| m == MIGRATION))
}

/// The organisation owner's row, if there is one.
pub async fn owner_row(state: &AppState) -> anyhow::Result<Option<PersonRow>> {
    let rows: Vec<PersonRow> = state.store.list(db::PERSON).await?;
    Ok(rows
        .into_iter()
        .find(|r| r.person.org_role == OrgRole::Owner))
}

/// `people-v1` (D2): run once in `Service::start`, after `trace::start` and before
/// `recover`. Creates the bootstrap owner from `config.allowed_github_login` (with the
/// `company` group and the owner's membership of it), gives every device without a
/// person to the owner, and makes every library row without an owner the owner's and
/// private to them. Documents get their `owner`; their `access` (and the passages')
/// stays `company` until L2. Idempotent: a crash part-way re-runs it. Nothing is deleted.
pub async fn migrate(state: &AppState) -> anyhow::Result<()> {
    let _w = state.write_lock.lock().await;
    if let Some(owner) = owner_row(state).await?
        && !owner
            .person
            .github_login
            .eq_ignore_ascii_case(&state.config.allowed_github_login)
    {
        tracing::warn!(
            "allowed_github_login names @{} but the organisation owner is @{}; the owner is \
             a person now and the setting is ignored",
            state.config.allowed_github_login,
            owner.person.github_login
        );
    }
    let done = state
        .settings_row()
        .await?
        .migrations
        .iter()
        .any(|m| m == MIGRATION);
    if done {
        return Ok(());
    }

    // 1. The owner.
    groups::ensure_root_locked(&state.store).await?;
    let owner = match owner_row(state).await? {
        Some(o) => o,
        None => {
            let login = state.config.allowed_github_login.trim().to_string();
            anyhow::ensure!(
                !login.is_empty(),
                "allowed_github_login is empty: the service needs its owner's GitHub login"
            );
            let row = PersonRow::new(Person {
                id: new_id(),
                handle: free_handle(state, &login).await?,
                name: login.clone(),
                github_login: login,
                github_id: None,
                avatar_url: None,
                org_role: OrgRole::Owner,
                state: PersonState::Active,
                groups: Vec::new(),
                settings: PersonSettings::default(),
                created_at: Utc::now(),
            });
            state.store.put(db::PERSON, &row.person.id, &row).await?;
            row
        }
    };
    let owner_id = owner.person.id.clone();
    let mut owner_groups = groups_of(state, &owner_id).await?;
    if !owner_groups.iter().any(|g| g == ROOT_GROUP) {
        owner_groups.push(ROOT_GROUP.to_string());
        groups::set_memberships_locked(&state.store, &owner_id, &owner_groups)
            .await
            .map_err(|e| anyhow::anyhow!(e.message))?;
    }

    // 2. Devices.
    let devices: Vec<DeviceRow> = state.store.list(db::DEVICE).await?;
    let mut moved_devices = 0;
    for mut d in devices {
        if d.person_id.is_none() {
            d.person_id = Some(owner_id.clone());
            state.store.put(db::DEVICE, &d.id, &d).await?;
            moved_devices += 1;
        }
    }

    // 3. Library rows: the owner's, private to them.
    let private = serde_json::json!([person_id(&owner_id)]);
    let mut counts = Vec::new();
    for table in [
        db::TOPIC,
        db::LESSON,
        db::NOTE,
        db::CHAT,
        db::SOURCE,
        db::WORKSPACE,
        db::PROPOSAL,
    ] {
        let rows: Vec<serde_json::Value> = state.store.list(table).await?;
        let todo: Vec<RawRow> = rows
            .into_iter()
            .filter(|r| !has_owner(r) && r.get("id").is_some_and(|i| i.is_string()))
            .map(|mut r| {
                if let Some(obj) = r.as_object_mut() {
                    obj.insert("owner".into(), owner_id.clone().into());
                    obj.insert("access".into(), private.clone());
                    obj.insert("deny".into(), serde_json::json!([]));
                }
                RawRow(r)
            })
            .collect();
        state.store.put_many(table, &todo).await?;
        counts.push(format!("{table} {}", todo.len()));
    }

    // 4. Documents: the owner, and nothing else (their access stays `company`).
    let docs: Vec<serde_json::Value> = state.store.list(db::DOCUMENT).await?;
    let docs: Vec<RawRow> = docs
        .into_iter()
        .filter(|r| !has_owner(r) && r.get("id").is_some_and(|i| i.is_string()))
        .map(|mut r| {
            if let Some(obj) = r.as_object_mut() {
                obj.insert("owner".into(), owner_id.clone().into());
            }
            RawRow(r)
        })
        .collect();
    state.store.put_many(db::DOCUMENT, &docs).await?;
    counts.push(format!("document {}", docs.len()));

    // 5. Done.
    let mut settings = state.settings_row().await?;
    settings.migrations.push(MIGRATION.to_string());
    state
        .store
        .put(db::SETTINGS, SETTINGS_ID, &settings)
        .await?;
    drop(_w);
    state.expansions.invalidate_all();
    tracing::info!(
        "people-v1: owner @{}, {moved_devices} device(s), {}",
        owner.person.handle,
        counts.join(", ")
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Admission (D3)
// ---------------------------------------------------------------------------

/// Who `login` (GitHub's numeric `github_id`) signs in as (D3).
pub async fn admit(state: &AppState, login: &str, github_id: u64) -> AppResult<Admission> {
    let mut changed = None;
    let mut accepted = None;
    let admission = {
        let _w = state.write_lock.lock().await;
        match person_by_login(state, login).await? {
            Some(mut row) => match row.person.state {
                PersonState::Disabled => {
                    record_sign_in(state, Some(&row.person.id), github_id, Err("disabled"));
                    Admission::Denied(format!(
                        "The GitHub account @{login} is disabled on this Margin. Ask an admin."
                    ))
                }
                PersonState::Active | PersonState::Invited => match row.person.github_id {
                    Some(id) if id != github_id => {
                        record_sign_in(
                            state,
                            Some(&row.person.id),
                            github_id,
                            Err("another_github_account"),
                        );
                        Admission::Denied(format!(
                            "The GitHub account @{login} isn't the one that was invited."
                        ))
                    }
                    _ => {
                        if row.person.github_id.is_none() || row.person.state != PersonState::Active
                        {
                            let before = json!({"state": row.person.state});
                            row.person.github_id = Some(github_id);
                            row.person.state = PersonState::Active;
                            state.store.put(db::PERSON, &row.person.id, &row).await?;
                            if before["state"] != json!(PersonState::Active) {
                                audit::record(
                                    state,
                                    audit::row(
                                        AuditKind::PersonChange,
                                        person_id(&row.person.id),
                                        "person",
                                        &row.person.id,
                                        before,
                                        json!({"event": "activated", "state": PersonState::Active}),
                                    ),
                                );
                            }
                            changed = Some(row.clone());
                        }
                        record_sign_in(state, Some(&row.person.id), github_id, Ok(()));
                        Admission::Person(row.person)
                    }
                },
            },
            None => match pending_invitation(state, login).await? {
                Some(mut inv) => {
                    let now = Utc::now();
                    let groups = tidy_groups(
                        inv.invitation
                            .groups
                            .iter()
                            .cloned()
                            .chain([ROOT_GROUP.to_string()])
                            .collect(),
                    );
                    // Groups deleted since the invitation are skipped.
                    let unknown = unknown_groups(state, &groups).await?;
                    let groups: Vec<String> = groups
                        .into_iter()
                        .filter(|g| !unknown.contains(g))
                        .collect();
                    let row = PersonRow::new(Person {
                        id: new_id(),
                        github_login: login.to_string(),
                        github_id: Some(github_id),
                        handle: free_handle(state, login).await?,
                        name: login.to_string(),
                        avatar_url: None,
                        org_role: match inv.invitation.role {
                            OrgRole::Admin => OrgRole::Admin,
                            _ => OrgRole::Member,
                        },
                        state: PersonState::Active,
                        groups: Vec::new(),
                        settings: PersonSettings::default(),
                        created_at: now,
                    });
                    state.store.put(db::PERSON, &row.person.id, &row).await?;
                    // Their invited groups and `company` (`groups` holds both).
                    groups::set_memberships_locked(&state.store, &row.person.id, &groups).await?;
                    inv.invitation.accepted_at = Some(now);
                    inv.invitation.accepted_by = Some(row.person.id.clone());
                    state
                        .store
                        .put(db::INVITATION, &inv.invitation.id, &inv)
                        .await?;
                    let me = person_id(&row.person.id);
                    audit::record(
                        state,
                        audit::row(
                            AuditKind::PersonChange,
                            me.clone(),
                            "person",
                            &row.person.id,
                            Value::Null,
                            json!({
                                "event": "joined",
                                "invitation_id": inv.invitation.id,
                                "invited_by": person_id(&inv.invitation.invited_by),
                                "role": row.person.org_role,
                                "state": row.person.state,
                            }),
                        ),
                    );
                    audit::record(
                        state,
                        audit::row(
                            AuditKind::MembershipChange,
                            me,
                            "person",
                            &row.person.id,
                            json!({"groups": []}),
                            json!({
                                "groups": groups,
                                "invitation_id": inv.invitation.id,
                            }),
                        ),
                    );
                    record_sign_in(state, Some(&row.person.id), github_id, Ok(()));
                    accepted = Some(inv);
                    changed = Some(row.clone());
                    Admission::Invited(row.person)
                }
                None => {
                    record_sign_in(state, None, github_id, Err("not_invited"));
                    Admission::Denied(format!(
                        "The GitHub account @{login} isn't allowed to use this Margin. Ask an \
                         admin to invite you."
                    ))
                }
            },
        }
    };
    if let Some(row) = changed {
        state.expansions.invalidate_all();
        let person = public_person(state, &row, false).await?;
        state.emit(Event::PersonChanged { person });
    }
    if let Some(inv) = accepted {
        state.emit_to(
            Audience::Admins,
            Event::InvitationChanged {
                invitation: inv.invitation,
            },
        );
    }
    Ok(match admission {
        Admission::Invited(p) => {
            let row = PersonRow::new(p);
            Admission::Invited(public_person(state, &row, true).await?)
        }
        other => other,
    })
}

/// Journals a GitHub sign-in: approved (`sign_in`, the person is the actor) or denied
/// (`sign_in_denied` with a reason code; a login that matches no person is
/// `github/<numeric id>`). Ids and codes only, never the login.
fn record_sign_in(
    state: &AppState,
    person: Option<&str>,
    github_id: u64,
    outcome: Result<(), &str>,
) {
    let github = format!("github/{github_id}");
    let (actor, subject_kind, subject_id) = match person {
        Some(p) => (person_id(p), "person", p.to_string()),
        None => (github.clone(), "github_account", github_id.to_string()),
    };
    let after = match outcome {
        Ok(()) => json!({"event": "sign_in", "github_id": github_id}),
        Err(reason) => json!({"event": "sign_in_denied", "reason": reason, "github_id": github_id}),
    };
    audit::record(
        state,
        audit::row(
            AuditKind::PersonChange,
            actor,
            subject_kind,
            &subject_id,
            Value::Null,
            after,
        ),
    );
}

/// The newest open invitation for `login`.
async fn pending_invitation(
    state: &AppState,
    login: &str,
) -> anyhow::Result<Option<InvitationRow>> {
    let now = Utc::now();
    let mut rows: Vec<InvitationRow> = state
        .store
        .list_where(db::INVITATION, "login_key", &login.to_lowercase())
        .await?;
    rows.retain(|r| r.invitation.accepted_at.is_none() && r.invitation.expires_at > now);
    rows.sort_by_key(|r| std::cmp::Reverse(r.invitation.created_at));
    Ok(rows.into_iter().next())
}

// ---------------------------------------------------------------------------
// Routes (D4)
// ---------------------------------------------------------------------------

/// `GET /api/people`: the directory; `settings` only on the caller's own row.
pub async fn list_people(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
) -> AppResult<Json<Vec<Person>>> {
    let mut rows: Vec<PersonRow> = state.store.list(db::PERSON).await?;
    rows.sort_by(|a, b| {
        a.person
            .handle
            .cmp(&b.person.handle)
            .then_with(|| a.person.id.cmp(&b.person.id))
    });
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(public_person(&state, row, row.person.id == p.person_id).await?);
    }
    Ok(Json(out))
}

/// `GET /api/people/{id}`.
pub async fn get_person(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    Path(id): Path<Id>,
) -> AppResult<Json<Person>> {
    let row = person_row(&state, &id)
        .await?
        .ok_or_else(|| AppError::not_found("person"))?;
    Ok(Json(public_person(&state, &row, id == p.person_id).await?))
}

fn owner_fixed() -> AppError {
    AppError::new(
        StatusCode::BAD_REQUEST,
        access_code::OWNER_FIXED,
        "The organisation owner's role and state can't change.",
    )
}

fn owner_only_admins() -> AppError {
    AppError::new(
        StatusCode::FORBIDDEN,
        access_code::ADMIN_REQUIRED,
        "Only the organisation owner can make someone an admin or remove an admin.",
    )
}

fn check_name(name: &str) -> AppResult<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::bad_request("Give a name."));
    }
    if name.chars().count() > NAME_MAX {
        return Err(AppError::bad_request(format!(
            "A name is at most {NAME_MAX} characters."
        )));
    }
    Ok(name.to_string())
}

fn check_avatar_url(url: &str) -> AppResult<String> {
    let url = url.trim();
    let ok = url.len() <= AVATAR_URL_MAX
        && (url.starts_with("https://") || url.starts_with("http://"))
        && reqwest::Url::parse(url).is_ok();
    if !ok {
        return Err(AppError::bad_request(
            "A picture is an http(s) address of an image.",
        ));
    }
    Ok(url.to_string())
}

/// `PATCH /api/people/{id}`: self, admin or owner, by field (D4).
pub async fn patch_person(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    Path(id): Path<Id>,
    ApiJson(patch): ApiJson<PersonPatch>,
) -> AppResult<Json<Person>> {
    let is_self = id == p.person_id;
    let personal = patch.name.is_some() || patch.avatar_url.is_some() || patch.settings.is_some();
    let governing = patch.org_role.is_some() || patch.state.is_some();
    let name = patch.name.as_deref().map(check_name).transpose()?;
    let avatar_url = match &patch.avatar_url {
        Some(Some(u)) => Some(Some(check_avatar_url(u)?)),
        Some(None) => Some(None),
        None => None,
    };
    let (row, revoke, access_changed) = {
        let _w = state.write_lock.lock().await;
        let Some(mut row) = person_row(&state, &id).await? else {
            return Err(AppError::not_found("person"));
        };
        if personal && !is_self {
            return Err(AppError::new(
                StatusCode::FORBIDDEN,
                access_code::NOT_OWNER,
                format!(
                    "Only @{} changes their own name, picture and settings.",
                    row.person.handle
                ),
            ));
        }
        let mut revoke = false;
        let mut access_changed = false;
        let before = json!({"role": row.person.org_role, "state": row.person.state});
        let mut events: Vec<&str> = Vec::new();
        if governing {
            if !p.is_admin() {
                return Err(admin_required());
            }
            let role_change = patch.org_role.filter(|r| *r != row.person.org_role);
            let state_change = patch.state.filter(|s| *s != row.person.state);
            if row.person.org_role == OrgRole::Owner
                && (role_change.is_some() || state_change.is_some())
            {
                return Err(owner_fixed());
            }
            if let Some(role) = role_change {
                match role {
                    OrgRole::Owner => return Err(owner_fixed()),
                    // Making or removing an admin is the owner's (owner, Q3).
                    _ if p.org_role != OrgRole::Owner => return Err(owner_only_admins()),
                    _ => {}
                }
                row.person.org_role = role;
                access_changed = true;
                events.push("role_changed");
            }
            if let Some(new_state) = state_change {
                match new_state {
                    PersonState::Invited => {
                        return Err(AppError::bad_request(
                            "A person is either active or disabled.",
                        ));
                    }
                    PersonState::Disabled if is_self => {
                        return Err(AppError::bad_request("You can't disable yourself."));
                    }
                    PersonState::Disabled => {
                        revoke = true;
                        events.push("disabled");
                    }
                    PersonState::Active => events.push("enabled"),
                }
                row.person.state = new_state;
                access_changed = true;
            }
        }
        if let Some(name) = name {
            row.person.name = name;
        }
        if let Some(url) = avatar_url {
            row.person.avatar_url = url;
        }
        if let Some(settings) = patch.settings {
            row.person.settings = settings;
        }
        state.store.put(db::PERSON, &row.person.id, &row).await?;
        if !events.is_empty() {
            audit::record(
                &state,
                audit::by(
                    &p,
                    AuditKind::PersonChange,
                    "person",
                    &row.person.id,
                    before,
                    json!({
                        "event": events.join(","),
                        "role": row.person.org_role,
                        "state": row.person.state,
                    }),
                ),
            );
        }
        (row, revoke, access_changed)
    };
    if revoke {
        let devices: Vec<DeviceRow> = state
            .store
            .list_where(db::DEVICE, "person_id", &row.person.id)
            .await?;
        crate::auth::revoke_person_devices(&state, &row.person.id).await?;
        for device in &devices {
            audit::record(&state, audit::device_revoked_with_person(&p, device));
        }
        tracing::info!(person = %row.person.id, "person disabled");
    }
    state.expansions.invalidate_all();
    let public = public_person(&state, &row, false).await?;
    state.emit(Event::PersonChanged { person: public });
    if access_changed {
        state.emit_to(
            Audience::Person(row.person.id.clone()),
            Event::AccessChanged {
                person_id: row.person.id.clone(),
            },
        );
    }
    Ok(Json(public_person(&state, &row, is_self).await?))
}

/// `PUT /api/people/{id}/groups` (admins): replaces the direct `manual` memberships.
pub async fn put_person_groups(
    State(state): State<AppState>,
    RequireAdmin(admin): RequireAdmin,
    Path(id): Path<Id>,
    ApiJson(groups): ApiJson<Vec<String>>,
) -> AppResult<Json<Person>> {
    let Some(row) = person_row(&state, &id).await? else {
        return Err(AppError::not_found("person"));
    };
    // Replaces the manual memberships (journaled as a `MembershipChange`), clears the
    // expansion cache and sends `MembershipChanged` (everyone) and `AccessChanged` (the
    // person).
    groups::set_memberships_by(&state, &admin, &id, &tidy_groups(groups)).await?;
    Ok(Json(public_person(&state, &row, false).await?))
}

/// The caller's person row (the agent's owner for an agent).
async fn own_row(state: &AppState, p: &Principal) -> AppResult<PersonRow> {
    person_row(state, &p.person_id)
        .await?
        .ok_or_else(AppError::unauthorized)
}

/// `GET /api/me/settings`.
pub async fn get_my_settings(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
) -> AppResult<Json<PersonSettings>> {
    Ok(Json(own_row(&state, &p).await?.person.settings))
}

/// `PATCH /api/me/settings`: the whole value.
pub async fn patch_my_settings(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    ApiJson(settings): ApiJson<PersonSettings>,
) -> AppResult<Json<PersonSettings>> {
    let row = {
        let _w = state.write_lock.lock().await;
        let mut row = own_row(&state, &p).await?;
        row.person.settings = settings;
        state.store.put(db::PERSON, &row.person.id, &row).await?;
        row
    };
    state.expansions.invalidate_all();
    let person = public_person(&state, &row, false).await?;
    state.emit(Event::PersonChanged { person });
    Ok(Json(row.person.settings))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_are_tidied() {
        assert_eq!(
            tidy_groups(vec![
                "company/b".into(),
                " company/a ".into(),
                "company/b".into()
            ]),
            vec!["company/a", "company/b"]
        );
    }

    #[test]
    fn names_and_avatars_are_checked() {
        assert_eq!(check_name("  Bob ").unwrap(), "Bob");
        assert!(check_name("   ").is_err());
        assert!(check_name(&"x".repeat(NAME_MAX + 1)).is_err());
        assert!(check_avatar_url("https://example.com/a.png").is_ok());
        assert!(check_avatar_url("javascript:alert(1)").is_err());
    }

    #[test]
    fn rows_without_an_owner_are_found() {
        assert!(!has_owner(&serde_json::json!({"id": "t"})));
        assert!(!has_owner(&serde_json::json!({"id": "t", "owner": null})));
        assert!(has_owner(&serde_json::json!({"id": "t", "owner": "p"})));
        assert_eq!(
            db::HasKey::key(&RawRow(serde_json::json!({"id": "t1"}))),
            "t1"
        );
    }
}
