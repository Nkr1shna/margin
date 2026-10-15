//! Invitations by GitHub login (L1-auth, `docs/l1-brief.md` D3, D4). An invitation
//! turns into a person at the invited login's first sign-in (`people::admit`).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{Duration, Utc};
use margin_shared::{Event, INVITATION_DAYS, Id, Invitation, NewInvitation, OrgRole, access_code};
use serde::{Deserialize, Serialize};

use crate::db;
use crate::error::{ApiJson, AppError, AppResult};
use crate::people;
use crate::principal::RequireAdmin;
use crate::state::{AppState, Audience, new_id};

/// A stored invitation: the wire type plus the login lowercased (`invitation_login`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvitationRow {
    #[serde(flatten)]
    pub invitation: Invitation,
    pub login_key: String,
}

/// A GitHub login: 1..=39 of `[A-Za-z0-9-]`, no leading or trailing `-`, no `--`.
pub fn is_github_login(login: &str) -> bool {
    (1..=39).contains(&login.len())
        && !login.starts_with('-')
        && !login.ends_with('-')
        && !login.contains("--")
        && login
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Whether the invitation can still be accepted.
pub fn is_pending(inv: &Invitation) -> bool {
    inv.accepted_at.is_none() && inv.expires_at > Utc::now()
}

/// `GET /api/invitations` (admins): pending and accepted, newest first.
pub async fn list_invitations(
    State(state): State<AppState>,
    _admin: RequireAdmin,
) -> AppResult<Json<Vec<Invitation>>> {
    let rows: Vec<InvitationRow> = state.store.list(db::INVITATION).await?;
    let mut out: Vec<Invitation> = rows.into_iter().map(|r| r.invitation).collect();
    out.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    Ok(Json(out))
}

/// `POST /api/invitations` (admins; `role: Admin` needs the owner).
pub async fn create_invitation(
    State(state): State<AppState>,
    RequireAdmin(p): RequireAdmin,
    ApiJson(new): ApiJson<NewInvitation>,
) -> AppResult<Json<Invitation>> {
    let login = new.github_login.trim().trim_start_matches('@').to_string();
    if !is_github_login(&login) {
        return Err(AppError::bad_request(
            "Give the person's GitHub login, like octocat.",
        ));
    }
    match new.role {
        OrgRole::Owner => {
            return Err(AppError::bad_request(
                "Invite someone as a member or an admin; there is one owner.",
            ));
        }
        OrgRole::Admin if p.org_role != OrgRole::Owner => {
            return Err(AppError::new(
                StatusCode::FORBIDDEN,
                access_code::ADMIN_REQUIRED,
                "Only the organisation owner can invite an admin. Invite them as a member.",
            ));
        }
        _ => {}
    }
    let groups = people::tidy_groups(new.groups);
    let invitation = {
        let _w = state.write_lock.lock().await;
        people::check_groups(&state, &groups).await?;
        if people::person_by_login(&state, &login).await?.is_some() {
            return Err(AppError::conflict(
                access_code::ALREADY_MEMBER,
                format!(
                    "@{login} is already in this Margin. If they're disabled, enable them under \
                     Organisation → People."
                ),
            ));
        }
        let existing: Vec<InvitationRow> = state
            .store
            .list_where(db::INVITATION, "login_key", &login.to_lowercase())
            .await?;
        if existing.iter().any(|r| is_pending(&r.invitation)) {
            return Err(AppError::conflict(
                access_code::ALREADY_INVITED,
                format!("@{login} is already invited. Revoke that invitation to send a new one."),
            ));
        }
        let now = Utc::now();
        let row = InvitationRow {
            login_key: login.to_lowercase(),
            invitation: Invitation {
                id: new_id(),
                github_login: login,
                role: new.role,
                groups,
                invited_by: p.person_id.clone(),
                created_at: now,
                expires_at: now + Duration::days(INVITATION_DAYS),
                accepted_at: None,
                accepted_by: None,
            },
        };
        state
            .store
            .put(db::INVITATION, &row.invitation.id, &row)
            .await?;
        row.invitation
    };
    tracing::info!(invitation = %invitation.id, "invitation created");
    state.emit_to(
        Audience::Admins,
        Event::InvitationChanged {
            invitation: invitation.clone(),
        },
    );
    Ok(Json(invitation))
}

/// `DELETE /api/invitations/{id}` (admins): revokes a pending one.
pub async fn delete_invitation(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    Path(id): Path<Id>,
) -> AppResult<StatusCode> {
    {
        let _w = state.write_lock.lock().await;
        let row: Option<InvitationRow> = state.store.get(db::INVITATION, &id).await?;
        match row {
            Some(r) if r.invitation.accepted_at.is_none() => {
                state.store.delete(db::INVITATION, &id).await?;
            }
            _ => return Err(AppError::not_found("invitation")),
        }
    }
    state.emit_to(Audience::Admins, Event::InvitationDeleted { id });
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_logins() {
        assert!(is_github_login("bob-dev"));
        assert!(is_github_login("Nkr1shna"));
        assert!(is_github_login("a"));
        assert!(!is_github_login(""));
        assert!(!is_github_login("-bob"));
        assert!(!is_github_login("bob-"));
        assert!(!is_github_login("bo--b"));
        assert!(!is_github_login("bob dev"));
        assert!(!is_github_login(&"a".repeat(40)));
    }
}
