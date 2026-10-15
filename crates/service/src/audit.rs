//! The audit trail (roadmap L2, `docs/l2-brief.md` D7): who changed who may read or do
//! what. Rows (`margin_shared::context::AuditRow`) go to the `audit` table, ids
//! `audit:[kind, at, ulid]` like traces, written by `trace::start`'s writer through a
//! bounded channel (a full channel writes inline), never range-deleted by retention;
//! admins read them through `GET routes::AUDIT` and live as `Event::AuditAppended`.
//!
//! The first commit declares the signatures; L2-audit builds them. `record` and
//! `start` do nothing until then, so callers (L2-write's `audience::set`) can call
//! `record` from the start.

use axum::extract::{Query, State};
use axum::response::Response;
use chrono::{DateTime, Utc};
use margin_shared::context::{AuditKind, AuditRow};
use serde::Deserialize;

use crate::error::AppResult;
use crate::principal::RequireAdmin;
use crate::state::AppState;

/// The table audit rows are stored in.
pub const AUDIT: &str = "audit";

/// Journals one change, in the same breath as the change (call it under the write lock
/// that made it). Never fails the change: a row that can't be queued is written inline,
/// and one that can't be written is logged as an error. A no-op until L2-audit lands.
pub fn record(_state: &AppState, _row: AuditRow) {}

/// Starts the audit writer beside the trace writer (`trace::start`) and turns unread
/// lines of `<data>/exports.log` into `AuditKind::Export` rows. A no-op until L2-audit
/// lands.
pub fn start(_state: &AppState) {}

/// `GET routes::AUDIT`'s query.
#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    pub kind: Option<AuditKind>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    /// Only rows about this subject id.
    pub subject: Option<String>,
}

/// `GET routes::AUDIT` (admins): JSON lines, oldest first; a row's `subject_name` only
/// when the admin can read the subject.
pub async fn list(
    State(_state): State<AppState>,
    _admin: RequireAdmin,
    Query(_q): Query<AuditQuery>,
) -> AppResult<Response> {
    unimplemented!("L2-audit: audit::list")
}
