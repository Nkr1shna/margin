//! People: who may sign in, the `people-v1` migration, admission at the GitHub callback,
//! and the people routes (L1-auth, `docs/l1-brief.md` D2, D3, D4, D9).

use axum::Json;
use axum::extract::{Path, State};
use margin_shared::{Id, Person, PersonPatch, PersonSettings};
use serde::{Deserialize, Serialize};

use crate::error::{ApiJson, AppResult};
use crate::principal::{CurrentPrincipal, RequireAdmin};
use crate::state::AppState;

/// The one-time migration that creates the bootstrap owner and hangs every existing row
/// on them (`SettingsRow.migrations`).
pub const MIGRATION: &str = "people-v1";

/// A stored person: the wire `Person` plus the login lowercased, for the unique index
/// (`person_login`). `person.groups` is filled from `membership` rows on the way out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersonRow {
    #[serde(flatten)]
    pub person: Person,
    pub login_key: String,
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

/// `people-v1` (D2): run once in `Service::start`, after `trace::start` and before
/// `recover`. First commit: does nothing (L1-auth lands it).
pub async fn migrate(_state: &AppState) -> anyhow::Result<()> {
    Ok(())
}

/// Who `login` (GitHub's numeric `github_id`) signs in as (D3).
pub async fn admit(_state: &AppState, _login: &str, _github_id: u64) -> AppResult<Admission> {
    unimplemented!("L1, auth")
}

/// `GET /api/people`: the directory; `settings` only on the caller's own row.
pub async fn list_people(
    State(_state): State<AppState>,
    _p: CurrentPrincipal,
) -> AppResult<Json<Vec<Person>>> {
    unimplemented!("L1, auth")
}

/// `GET /api/people/{id}`.
pub async fn get_person(
    State(_state): State<AppState>,
    _p: CurrentPrincipal,
    Path(_id): Path<Id>,
) -> AppResult<Json<Person>> {
    unimplemented!("L1, auth")
}

/// `PATCH /api/people/{id}`: self, admin or owner, by field (D4).
pub async fn patch_person(
    State(_state): State<AppState>,
    _p: CurrentPrincipal,
    Path(_id): Path<Id>,
    ApiJson(_patch): ApiJson<PersonPatch>,
) -> AppResult<Json<Person>> {
    unimplemented!("L1, auth")
}

/// `PUT /api/people/{id}/groups` (admins): replaces the direct `manual` memberships.
pub async fn put_person_groups(
    State(_state): State<AppState>,
    _admin: RequireAdmin,
    Path(_id): Path<Id>,
    ApiJson(_groups): ApiJson<Vec<String>>,
) -> AppResult<Json<Person>> {
    unimplemented!("L1, auth")
}

/// `GET /api/me/settings`.
pub async fn get_my_settings(
    State(_state): State<AppState>,
    _p: CurrentPrincipal,
) -> AppResult<Json<PersonSettings>> {
    unimplemented!("L1, auth")
}

/// `PATCH /api/me/settings`: the whole value.
pub async fn patch_my_settings(
    State(_state): State<AppState>,
    _p: CurrentPrincipal,
    ApiJson(_settings): ApiJson<PersonSettings>,
) -> AppResult<Json<PersonSettings>> {
    unimplemented!("L1, auth")
}
