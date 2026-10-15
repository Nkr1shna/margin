//! Audiences (roadmap L2, `docs/l2-brief.md` D2, D3): what stamps a document, the
//! `passages-access-v1` migration, and publishing (`PUT routes::audience(unit, id)`)
//! with its cascade down to passages.
//!
//! The first commit declares the signatures; L2-write builds them. `migrate` is a no-op
//! until then, so a service started on the first commit behaves as before.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use margin_shared::context::Document;
use margin_shared::{Audience, AudienceUnit, AudienceView, access_code};

use crate::error::{ApiJson, AppError, AppResult};
use crate::principal::{CurrentPrincipal, Principal};
use crate::state::{AppState, Ownership};

/// Recorded in `SettingsRow.migrations` once every document and its passages carry the
/// audience of the row that owns them.
pub const ACCESS_MIGRATION: &str = "passages-access-v1";

/// The audience a document takes from the row that owns it: a lesson's, note's or
/// chat's; a source's; a folder file's source row (the folder's); an agent document's
/// own (`[person/<owner>]` unless ingested or published with one). A document whose
/// owning row is gone keeps what it has.
pub async fn of_document(_state: &AppState, _doc: &Document) -> anyhow::Result<Ownership> {
    unimplemented!("L2-write: audience::of_document")
}

/// Sets the audience of a unit `p` owns (admins are not exempt): validates it, stores
/// `access = [person/<owner>] ∪ a.access` and `deny = a.deny − {person/<owner>}`,
/// cascades to everything under the unit and its documents' passages under the write
/// lock, journals an `AuditKind::AudienceChange` row (`audit::record`), then emits the
/// rows' `*Changed`, `DocumentChanged` to the new audience and `AudienceRevoked` to who
/// lost access. Answers the stored audience.
pub async fn set(
    _state: &AppState,
    _p: &Principal,
    _unit: AudienceUnit,
    _id: &str,
    _a: Audience,
) -> AppResult<Audience> {
    unimplemented!("L2-write: audience::set")
}

/// The stored audience of a unit `p` can read, and where it follows another from.
pub async fn view(
    _state: &AppState,
    _p: &Principal,
    _unit: AudienceUnit,
    _id: &str,
) -> AppResult<AudienceView> {
    unimplemented!("L2-write: audience::view")
}

/// `passages-access-v1`: re-stamps every document and its passages from the row that
/// owns it (`of_document`, `Store::set_document_audience`), after `people-v1`, under
/// the write lock, once. A no-op until L2-write lands.
pub async fn migrate(_state: &AppState) -> anyhow::Result<()> {
    Ok(())
}

/// The unit a path segment names, else 400 `not_a_unit` saying where to change it.
pub fn unit(segment: &str) -> AppResult<AudienceUnit> {
    AudienceUnit::parse(segment).ok_or_else(|| {
        let message = match segment {
            "lesson" | "note" => {
                "Lessons and notes follow their topic's audience; change it on the topic."
            }
            "chat" => "Chats stay private to you; there's no audience to change.",
            _ => "That can't have an audience. Change it on its topic, folder or source.",
        };
        AppError::new(StatusCode::BAD_REQUEST, access_code::NOT_A_UNIT, message)
    })
}

/// `PUT routes::audience(unit, id)`.
pub async fn put_audience(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    Path((segment, id)): Path<(String, String)>,
    ApiJson(a): ApiJson<Audience>,
) -> AppResult<Json<Audience>> {
    let unit = unit(&segment)?;
    if let Err(message) = margin_shared::validate(&a) {
        let code = if a.access.len() > margin_shared::AUDIENCE_MAX
            || a.deny.len() > margin_shared::AUDIENCE_MAX
        {
            access_code::AUDIENCE_TOO_LARGE
        } else {
            access_code::BAD_ACCESS
        };
        return Err(AppError::new(StatusCode::BAD_REQUEST, code, message));
    }
    Ok(Json(set(&state, &p, unit, &id, a).await?))
}

/// `GET routes::audience(unit, id)`.
pub async fn get_audience(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    Path((segment, id)): Path<(String, String)>,
) -> AppResult<Json<AudienceView>> {
    let unit = unit(&segment)?;
    Ok(Json(view(&state, &p, unit, &id).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_units_have_an_audience() {
        assert_eq!(unit("topic").unwrap(), AudienceUnit::Topic);
        for segment in ["lesson", "note", "chat", "x"] {
            let e = unit(segment).unwrap_err();
            assert_eq!(e.code, access_code::NOT_A_UNIT);
            assert_eq!(e.status, StatusCode::BAD_REQUEST);
        }
        assert!(unit("lesson").unwrap_err().message.contains("topic"));
    }
}
