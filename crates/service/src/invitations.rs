//! Invitations by GitHub login (L1-auth, `docs/l1-brief.md` D3, D4). An invitation
//! turns into a person at the invited login's first sign-in (`people::admit`).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use margin_shared::{Id, Invitation, NewInvitation};
use serde::{Deserialize, Serialize};

use crate::error::{ApiJson, AppResult};
use crate::principal::RequireAdmin;
use crate::state::AppState;

/// A stored invitation: the wire type plus the login lowercased (`invitation_login`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvitationRow {
    #[serde(flatten)]
    pub invitation: Invitation,
    pub login_key: String,
}

/// `GET /api/invitations` (admins): pending and accepted, newest first.
pub async fn list_invitations(
    State(_state): State<AppState>,
    _admin: RequireAdmin,
) -> AppResult<Json<Vec<Invitation>>> {
    unimplemented!("L1, auth")
}

/// `POST /api/invitations` (admins; `role: Admin` needs the owner).
pub async fn create_invitation(
    State(_state): State<AppState>,
    _admin: RequireAdmin,
    ApiJson(_new): ApiJson<NewInvitation>,
) -> AppResult<Json<Invitation>> {
    unimplemented!("L1, auth")
}

/// `DELETE /api/invitations/{id}` (admins): revokes a pending one.
pub async fn delete_invitation(
    State(_state): State<AppState>,
    _admin: RequireAdmin,
    Path(_id): Path<Id>,
) -> AppResult<StatusCode> {
    unimplemented!("L1, auth")
}
