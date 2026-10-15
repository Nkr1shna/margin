//! The audit trail (roadmap L2, `docs/l2-brief.md` D7): who changed who may read or do
//! what. Rows (`margin_shared::context::AuditRow`) go to the `audit` table, ids
//! `audit:[kind, ns, ulid]` like traces (`trace::id_time`), written by one writer task
//! off a bounded channel (the trace channel's sibling, `Tracer::audit`); a full channel
//! is an error in the log and the row is written by a task of its own, so no row is
//! dropped. A write that fails is retried, then logged with the row's ids (never
//! silently lost). Retention never deletes audit rows (`trace::delete_before` ranges
//! over `trace` only). Admins read them through `GET routes::AUDIT` and live as
//! `Event::AuditAppended`.
//!
//! What is recorded (each in the same write-lock section as the change):
//! - `AudienceChange`: `audience::set` (L2-write calls `record(state, audience_change(..))`).
//! - `MembershipChange`: `PUT /api/people/{id}/groups`; an invitation accepted at
//!   sign-in.
//! - `GroupChange`: a group created, renamed or deleted.
//! - `PersonChange`: a person's role or state (disable, enable); an invitation created
//!   or revoked (`subject.kind` `invitation`).
//! - `SignIn`: a sign-in approved or denied (`after.event` `sign_in` /
//!   `sign_in_denied`, recorded at admission before a device exists), and the device
//!   minted when the app collects an approved one (`device_signed_in`, `device_id` set).
//! - `DeviceRevoked`: a device revoked (`subject.kind` `device`), by `DELETE
//!   /api/devices/{id}` or with its person disabled (`after.reason`).
//! - `AgentChange`: an agent created, its ceiling or `disabled` changed, deleted.
//! - `AgentToken`: a token minted or revoked (its prefix only).
//! - `Export`: `margin-service export` runs, read from `<data>/exports.log` at start.
//!
//! `before`/`after` hold ids, paths, roles, states, ceilings and token prefixes: never
//! content and never names (`record` strips `name`, `title`, `text`, `body` keys as a
//! second guard). Names are joined at read time, and only for rows the admin can read.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Context;
use axum::body::{Body, Bytes};
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use margin_shared::context::{AuditKind, AuditRow, AuditSubject, Document};
use margin_shared::{Audience, AudienceUnit, Event, PrincipalId, Topic, person_id};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Notify, OnceCell, mpsc};

use crate::db::{self, Store};
use crate::error::{AppError, AppResult};
use crate::pipeline::Reader;
use crate::principal::{Principal, PrincipalKind, RequireAdmin};
use crate::state::{AppState, DeviceRow, Owned, can_read};
use crate::trace::id_time;

/// The table audit rows are stored in.
pub const AUDIT: &str = "audit";
/// Rows waiting for the writer before `record` writes them from a task of their own.
pub const AUDIT_CHANNEL: usize = 1_024;
/// Rows the writer puts in one statement.
const WRITE_BATCH: usize = db::INSERT_MANY_MAX;
/// Attempts at writing a batch before its rows are logged as lost.
const WRITE_ATTEMPTS: u32 = 3;
/// Rows read per range query.
const PAGE: usize = 500;
/// The most rows `GET routes::AUDIT?limit=` returns.
pub const LIMIT_MAX: usize = 1_000;
/// How long `GET routes::AUDIT` waits for rows recorded before it to be written.
const READ_FLUSH_WAIT: Duration = Duration::from_secs(2);
/// Keys `record` removes from `before`/`after` wherever they are (no names, no content).
const CONTENT_KEYS: [&str; 6] = ["name", "title", "text", "body", "content", "github_login"];

const SCHEMA: &str = "
DEFINE TABLE IF NOT EXISTS audit SCHEMAFULL;
DEFINE FIELD IF NOT EXISTS kind         ON audit TYPE string;
DEFINE FIELD IF NOT EXISTS at           ON audit TYPE datetime;
DEFINE FIELD IF NOT EXISTS actor        ON audit TYPE string;
DEFINE FIELD IF NOT EXISTS on_behalf_of ON audit TYPE option<string>;
DEFINE FIELD IF NOT EXISTS subject      ON audit TYPE object;
DEFINE FIELD IF NOT EXISTS subject.kind ON audit TYPE string;
DEFINE FIELD IF NOT EXISTS subject.id   ON audit TYPE string;
DEFINE FIELD IF NOT EXISTS before       ON audit TYPE option<object> FLEXIBLE;
DEFINE FIELD IF NOT EXISTS after        ON audit TYPE option<object> FLEXIBLE;
DEFINE FIELD IF NOT EXISTS device_id    ON audit TYPE option<string>;
DEFINE INDEX IF NOT EXISTS audit_subject ON audit FIELDS subject.id;
";

/// A row on its way to the store, with the last part of its id.
#[derive(Debug, Clone)]
struct Pending {
    ulid: String,
    row: AuditRow,
}

/// Hands audit rows to the writer task; lives beside the tracer (`Tracer::audit`).
pub struct Auditor {
    tx: mpsc::Sender<Pending>,
    rx: Mutex<Option<mpsc::Receiver<Pending>>>,
    /// Units of work accepted (rows, and the start's `exports.log` read) and finished.
    queued: AtomicU64,
    done: AtomicU64,
    written: Notify,
    /// Rows that could not be written after every attempt.
    lost: AtomicU64,
    schema: OnceCell<()>,
}

impl Auditor {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(AUDIT_CHANNEL);
        Self {
            tx,
            rx: Mutex::new(Some(rx)),
            queued: AtomicU64::new(0),
            done: AtomicU64::new(0),
            written: Notify::new(),
            lost: AtomicU64::new(0),
            schema: OnceCell::new(),
        }
    }

    /// Rows that could not be written (each one logged as an error with its ids).
    pub fn lost(&self) -> u64 {
        self.lost.load(Ordering::Relaxed)
    }

    fn finished(&self, n: u64) {
        self.done.fetch_add(n, Ordering::SeqCst);
        self.written.notify_waiters();
    }

    /// Waits, at most `wait`, until every row recorded before the call is written (or
    /// logged as lost). False on timeout.
    pub async fn flush(&self, wait: Duration) -> bool {
        let target = self.queued.load(Ordering::SeqCst);
        let done = async {
            loop {
                let notified = self.written.notified();
                if self.done.load(Ordering::SeqCst) >= target {
                    return;
                }
                notified.await;
            }
        };
        tokio::time::timeout(wait, done).await.is_ok()
    }
}

impl Default for Auditor {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Building rows
// ---------------------------------------------------------------------------

/// A row of `kind` about `subject` made by `p` now: `actor` is `p.id`, `on_behalf_of`
/// the owner when `p` is an agent, `device_id` the calling device.
pub fn by(
    p: &Principal,
    kind: AuditKind,
    subject_kind: &str,
    subject_id: &str,
    before: Value,
    after: Value,
) -> AuditRow {
    let mut row = row(kind, p.id.clone(), subject_kind, subject_id, before, after);
    if let PrincipalKind::Agent { owner_id, .. } = &p.kind {
        row.on_behalf_of = Some(person_id(owner_id));
    }
    row.device_id = p.device.as_ref().map(|d| d.id.clone());
    row
}

/// A row of `kind` about `subject` made by `actor` now (`person/<id>`, `agent/<id>`,
/// `host/<unix user>`, `github/<numeric id>` for a sign-in that matched no one).
pub fn row(
    kind: AuditKind,
    actor: PrincipalId,
    subject_kind: &str,
    subject_id: &str,
    before: Value,
    after: Value,
) -> AuditRow {
    AuditRow {
        kind,
        at: Utc::now(),
        actor,
        on_behalf_of: None,
        subject: AuditSubject {
            kind: subject_kind.to_string(),
            id: subject_id.to_string(),
        },
        before,
        after,
        device_id: None,
        subject_name: None,
    }
}

/// `audience::set`'s row: the unit's audience before and after (paths and ids only).
/// L2-write calls `record(state, audience_change(p, unit, id, &before, &after))` under
/// the write lock that stores the new audience.
pub fn audience_change(
    p: &Principal,
    unit: AudienceUnit,
    id: &str,
    before: &Audience,
    after: &Audience,
) -> AuditRow {
    by(
        p,
        AuditKind::AudienceChange,
        unit.as_str(),
        id,
        json!({"access": before.access, "deny": before.deny}),
        json!({"access": after.access, "deny": after.deny}),
    )
}

/// A device revoked by `p` (`DELETE /api/devices/{id}`).
pub fn device_revoked(p: &Principal, device: &DeviceRow) -> AuditRow {
    by(
        p,
        AuditKind::DeviceRevoked,
        "device",
        &device.id,
        json!({"person_id": device.person_id}),
        json!({"event": "device_revoked"}),
    )
}

/// A device revoked because its person was disabled by `p`.
pub fn device_revoked_with_person(p: &Principal, device: &DeviceRow) -> AuditRow {
    let mut row = device_revoked(p, device);
    row.after = json!({"event": "device_revoked", "reason": "person_disabled"});
    row
}

/// Removes names and content from `before`/`after`, and keeps them objects (the table's
/// fields are objects): anything else is kept under `value`.
fn scrub(v: Value) -> Value {
    fn walk(v: &mut Value) {
        match v {
            Value::Object(map) => {
                map.retain(|k, _| !CONTENT_KEYS.contains(&k.as_str()));
                map.values_mut().for_each(walk);
            }
            Value::Array(items) => items.iter_mut().for_each(walk),
            _ => {}
        }
    }
    let mut v = match v {
        Value::Null | Value::Object(_) => v,
        other => json!({ "value": other }),
    };
    walk(&mut v);
    v
}

// ---------------------------------------------------------------------------
// Recording and writing
// ---------------------------------------------------------------------------

/// Journals one change, in the same breath as the change (call it under the write lock
/// that made it). Never fails the change and never waits: the row is queued for the
/// writer; when the queue is full the row is written by a task of its own (and the log
/// says the writer is behind), so it isn't dropped.
pub fn record(state: &AppState, row: AuditRow) {
    let auditor = &state.tracer.audit;
    let mut row = row;
    row.before = scrub(row.before);
    row.after = scrub(row.after);
    row.subject_name = None;
    let pending = Pending {
        ulid: crate::trace::new_row_id(),
        row,
    };
    auditor.queued.fetch_add(1, Ordering::SeqCst);
    let pending = match auditor.tx.try_send(pending) {
        Ok(()) => return,
        Err(mpsc::error::TrySendError::Full(p) | mpsc::error::TrySendError::Closed(p)) => p,
    };
    tracing::error!(
        kind = pending.row.kind.as_str(),
        subject = %pending.row.subject.id,
        "the audit writer is behind; writing this row on its own"
    );
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            let state = state.clone();
            handle.spawn(async move {
                write_and_announce(&state, vec![pending]).await;
            });
        }
        Err(_) => {
            log_lost(&pending, "no runtime");
            auditor.lost.fetch_add(1, Ordering::Relaxed);
            auditor.finished(1);
        }
    }
}

/// Waits (at most `wait`) for every audit row recorded so far to be written: before
/// reading them, and when the service stops.
pub async fn flush(state: &AppState, wait: Duration) -> bool {
    state.tracer.audit.flush(wait).await
}

/// Starts the audit writer beside the trace writer (`trace::start`) and turns lines of
/// `<data>/exports.log` not yet in the table into `AuditKind::Export` rows. Called
/// once at startup; later calls do nothing.
pub fn start(state: &AppState) {
    let auditor = &state.tracer.audit;
    let Some(rx) = auditor.rx.lock().ok().and_then(|mut r| r.take()) else {
        return;
    };
    // The exports read counts as queued work, so a read right after start sees it.
    auditor.queued.fetch_add(1, Ordering::SeqCst);
    let state = state.clone();
    tokio::spawn(async move {
        match ingest_exports(&state).await {
            Ok(0) => {}
            Ok(n) => tracing::info!("{n} export(s) from exports.log added to the audit log"),
            Err(e) => tracing::error!("couldn't read exports.log into the audit log: {e:#}"),
        }
        state.tracer.audit.finished(1);
        run_writer(state, rx).await;
    });
}

async fn run_writer(state: AppState, mut rx: mpsc::Receiver<Pending>) {
    while let Some(first) = rx.recv().await {
        let mut batch = vec![first];
        while batch.len() < WRITE_BATCH {
            match rx.try_recv() {
                Ok(p) => batch.push(p),
                Err(_) => break,
            }
        }
        write_and_announce(&state, batch).await;
    }
}

/// Writes rows (retrying), sends `AuditAppended` to the admins for each written row and
/// marks them done; rows that can't be written are logged with their ids.
async fn write_and_announce(state: &AppState, batch: Vec<Pending>) {
    let auditor = &state.tracer.audit;
    let n = batch.len() as u64;
    let mut last_error = None;
    for attempt in 0..WRITE_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(200 * u64::from(attempt))).await;
        }
        match write_rows(&state.store, &auditor.schema, &batch).await {
            Ok(()) => {
                for p in batch {
                    state.emit_to(
                        crate::state::Audience::Admins,
                        Event::AuditAppended { row: p.row },
                    );
                }
                auditor.finished(n);
                return;
            }
            Err(e) => last_error = Some(e),
        }
    }
    let e = last_error.map(|e| format!("{e:#}")).unwrap_or_default();
    for p in &batch {
        log_lost(p, &e);
    }
    auditor.lost.fetch_add(n, Ordering::Relaxed);
    auditor.finished(n);
}

fn log_lost(p: &Pending, why: &str) {
    tracing::error!(
        kind = p.row.kind.as_str(),
        at = %p.row.at,
        actor = %p.row.actor,
        subject_kind = %p.row.subject.kind,
        subject = %p.row.subject.id,
        before = %p.row.before,
        after = %p.row.after,
        "AUDIT ROW NOT WRITTEN: {why}"
    );
}

/// Defines the `audit` table once per process (idempotent).
async fn ensure_schema(store: &Store, schema: &OnceCell<()>) -> anyhow::Result<()> {
    schema
        .get_or_try_init(|| async {
            store
                .query_rows::<Value>(SCHEMA, vec![])
                .await
                .context("couldn't define the audit table")?;
            anyhow::Ok(())
        })
        .await?;
    Ok(())
}

fn time_text(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

/// Writes up to `WRITE_BATCH` rows in one statement as `audit:[kind, ns, ulid]`.
/// `UPSERT`, so a retried batch (or an export line read again) writes the same record.
async fn write_rows(store: &Store, schema: &OnceCell<()>, rows: &[Pending]) -> anyhow::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    ensure_schema(store, schema).await?;
    for chunk in rows.chunks(WRITE_BATCH) {
        let mut out = Vec::with_capacity(chunk.len());
        for p in chunk {
            let Value::Object(mut content) = serde_json::to_value(&p.row)? else {
                anyhow::bail!("an audit row isn't an object");
            };
            content.remove("at");
            content.remove("subject_name");
            content.retain(|_, v| !v.is_null());
            out.push(json!({
                "kind": p.row.kind.as_str(),
                "at": time_text(p.row.at),
                "ns": id_time(p.row.at),
                "ulid": p.ulid,
                "content": content,
            }));
        }
        store
            .query_rows::<Value>(
                "FOR $r IN $rows { \
                   LET $at = <datetime>$r.at; \
                   UPSERT type::record('audit', [$r.kind, $r.ns, $r.ulid]) \
                     CONTENT object::extend($r.content, { at: $at }) RETURN NONE; \
                 };",
                vec![("rows", Value::Array(out))],
            )
            .await
            .context("couldn't write audit rows")?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// exports.log
// ---------------------------------------------------------------------------

/// One line of `<data>/exports.log` (`export::run` appends them).
#[derive(Debug, Clone, PartialEq, serde::Serialize, Deserialize)]
pub struct ExportLine {
    pub at: DateTime<Utc>,
    /// The person id whose library was exported.
    pub person: String,
    /// The unix user who ran it.
    pub by: String,
    pub host: String,
    /// The folder written to.
    pub out: String,
    /// False when the export failed part way (some files may have been written).
    #[serde(default = "yes")]
    pub ok: bool,
}

fn yes() -> bool {
    true
}

impl ExportLine {
    /// The audit row of this export.
    pub fn to_row(&self) -> AuditRow {
        let mut r = row(
            AuditKind::Export,
            format!("host/{}", self.by),
            "person",
            &self.person,
            Value::Null,
            json!({"host": self.host, "out": self.out, "ok": self.ok}),
        );
        r.at = self.at;
        r
    }
}

/// The id part of an export line's row: from the line's own content, so reading the
/// log again at every start writes nothing new.
fn export_ulid(line: &str) -> String {
    let hash = crate::state::sha256_hex(line.trim());
    format!("export-{}", &hash[..26])
}

/// Adds every line of `exports.log` that has no row yet; returns how many it added.
async fn ingest_exports(state: &AppState) -> anyhow::Result<usize> {
    let path = state.config.data_dir.join(crate::export::EXPORTS_LOG);
    let text = match tokio::fs::read_to_string(&path).await {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).with_context(|| format!("couldn't read {}", path.display())),
    };
    let mut fresh = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let parsed: ExportLine = match serde_json::from_str(line) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("{} line {}: not an export line: {e}", path.display(), i + 1);
                continue;
            }
        };
        let row = parsed.to_row();
        let ulid = export_ulid(line);
        if !exists(&state.store, &state.tracer.audit.schema, &row, &ulid).await? {
            fresh.push(Pending { ulid, row });
        }
    }
    let n = fresh.len();
    if n > 0 {
        write_rows(&state.store, &state.tracer.audit.schema, &fresh).await?;
        for p in fresh {
            state.emit_to(
                crate::state::Audience::Admins,
                Event::AuditAppended { row: p.row },
            );
        }
    }
    Ok(n)
}

async fn exists(
    store: &Store,
    schema: &OnceCell<()>,
    row: &AuditRow,
    ulid: &str,
) -> anyhow::Result<bool> {
    ensure_schema(store, schema).await?;
    let found: Vec<Value> = store
        .query_rows(
            "SELECT VALUE id FROM type::record('audit', [$kind, $ns, $ulid]);",
            vec![
                ("kind", json!(row.kind.as_str())),
                ("ns", json!(id_time(row.at))),
                ("ulid", json!(ulid)),
            ],
        )
        .await?;
    Ok(found.iter().any(|v| !v.is_null()))
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// `GET routes::AUDIT`'s query.
#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    pub kind: Option<AuditKind>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    /// Only rows about this subject id.
    pub subject: Option<String>,
    /// Only the newest `limit` rows of the window (at most `LIMIT_MAX`), still oldest
    /// first: the Audit view's page; "Load more" asks again with `to` just before the
    /// oldest row it has.
    pub limit: Option<usize>,
}

fn bad_range() -> AppError {
    AppError::new(
        StatusCode::BAD_REQUEST,
        "bad_range",
        "`from` must not be after `to`.",
    )
}

fn epoch() -> DateTime<Utc> {
    Utc.timestamp_opt(0, 0).unwrap()
}

/// `GET routes::AUDIT` (admins): JSON lines, oldest first. Rows recorded before the
/// request are written first (up to 2 s). A row's `subject_name` is joined only when
/// the admin can read the subject ("a topic of @alice" otherwise): admins read no
/// content through the audit log.
pub async fn list(
    State(state): State<AppState>,
    RequireAdmin(admin): RequireAdmin,
    Query(q): Query<AuditQuery>,
) -> AppResult<Response> {
    flush(&state, READ_FLUSH_WAIT).await;
    let from = q.from.unwrap_or_else(epoch);
    let to = q.to.unwrap_or_else(Utc::now);
    if from > to {
        return Err(bad_range());
    }
    ensure_schema(&state.store, &state.tracer.audit.schema).await?;
    let subject = q.subject.filter(|s| !s.is_empty());
    let kinds: Vec<AuditKind> = match q.kind {
        Some(k) => vec![k],
        None => AuditKind::ALL.to_vec(),
    };
    let mut names = Names::new(state.clone(), admin);
    if let Some(limit) = q.limit {
        let limit = limit.clamp(1, LIMIT_MAX);
        let mut rows = newest(&state.store, &kinds, from, to, subject.as_deref(), limit).await?;
        let mut out = String::new();
        for row in &mut rows {
            row.subject_name = names.of(&row.subject).await;
            out.push_str(&serde_json::to_string(row).map_err(anyhow::Error::from)?);
            out.push('\n');
        }
        return Ok(ndjson(Body::from(out)));
    }
    let mut merge = Merge {
        store: state.store.clone(),
        cursors: kinds
            .into_iter()
            .map(|kind| Cursor::new(kind, from, to, subject.clone()))
            .collect(),
    };
    // The first page now, so a store error is a 500 rather than a cut-off body.
    merge.fill().await?;
    let body = futures_util::stream::unfold(Some((merge, names)), |st| async move {
        let (mut merge, mut names) = st?;
        match merge.next_rows(PAGE).await {
            Ok(rows) if rows.is_empty() => None,
            Ok(mut rows) => {
                let mut out = String::new();
                for row in &mut rows {
                    row.subject_name = names.of(&row.subject).await;
                    if let Ok(line) = serde_json::to_string(row) {
                        out.push_str(&line);
                        out.push('\n');
                    }
                }
                Some((
                    Ok::<_, std::io::Error>(Bytes::from(out)),
                    Some((merge, names)),
                ))
            }
            Err(e) => {
                tracing::warn!("couldn't read audit rows: {e:#}");
                Some((Err(std::io::Error::other("audit read failed")), None))
            }
        }
    });
    Ok(ndjson(Body::from_stream(body)))
}

fn ndjson(body: Body) -> Response {
    ([(header::CONTENT_TYPE, "application/x-ndjson")], body).into_response()
}

/// Every stored audit row of `kind` (all when `None`) between `from` and `to`, oldest
/// first, without names. For tests and tools on the host.
pub async fn read_rows(
    state: &AppState,
    kind: Option<AuditKind>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> anyhow::Result<Vec<AuditRow>> {
    flush(state, READ_FLUSH_WAIT).await;
    ensure_schema(&state.store, &state.tracer.audit.schema).await?;
    let kinds: Vec<AuditKind> = kind.map_or_else(|| AuditKind::ALL.to_vec(), |k| vec![k]);
    let mut merge = Merge {
        store: state.store.clone(),
        cursors: kinds
            .into_iter()
            .map(|k| Cursor::new(k, from, to, None))
            .collect(),
    };
    let mut rows = Vec::new();
    loop {
        let page = merge.next_rows(PAGE).await?;
        if page.is_empty() {
            return Ok(rows);
        }
        rows.extend(page);
    }
}

/// The newest `limit` rows of `kinds` in the window, oldest first.
async fn newest(
    store: &Store,
    kinds: &[AuditKind],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    subject: Option<&str>,
    limit: usize,
) -> anyhow::Result<Vec<AuditRow>> {
    let filter = if subject.is_some() {
        "WHERE subject.id = $subject"
    } else {
        ""
    };
    let mut all: Vec<(i64, String, AuditRow)> = Vec::new();
    for kind in kinds {
        let sql = format!(
            "SELECT *, record::id(id) AS key FROM audit:[$kind, $from, '']..=[$kind, $to, '~'] \
             {filter} ORDER BY id DESC LIMIT $limit;"
        );
        let raw: Vec<Value> = store
            .query_rows(
                &sql,
                vec![
                    ("kind", json!(kind.as_str())),
                    ("from", json!(id_time(from))),
                    ("to", json!(id_time(to))),
                    ("subject", json!(subject.unwrap_or_default())),
                    ("limit", json!(limit)),
                ],
            )
            .await?;
        for r in raw {
            all.push(row_from_db(r)?);
        }
    }
    all.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let skip = all.len().saturating_sub(limit);
    Ok(all.into_iter().skip(skip).map(|(_, _, r)| r).collect())
}

/// Rows of one kind in a window, a page at a time.
struct Cursor {
    kind: AuditKind,
    to: DateTime<Utc>,
    subject: Option<String>,
    /// Where the next page starts (inclusive: id time, ulid) and the ulid last
    /// returned, skipped.
    after: (i64, String),
    last: Option<String>,
    buf: VecDeque<(i64, String, AuditRow)>,
    done: bool,
}

impl Cursor {
    fn new(
        kind: AuditKind,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        subject: Option<String>,
    ) -> Self {
        Self {
            kind,
            to,
            subject,
            after: (id_time(from), String::new()),
            last: None,
            buf: VecDeque::new(),
            done: false,
        }
    }

    async fn fill(&mut self, store: &Store) -> anyhow::Result<()> {
        if !self.buf.is_empty() || self.done {
            return Ok(());
        }
        let filter = if self.subject.is_some() {
            "WHERE subject.id = $subject"
        } else {
            ""
        };
        let sql = format!(
            "SELECT *, record::id(id) AS key FROM audit:[$kind, $from, $after]..=[$kind, $to, '~'] \
             {filter} LIMIT $page;"
        );
        let rows: Vec<Value> = store
            .query_rows(
                &sql,
                vec![
                    ("kind", json!(self.kind.as_str())),
                    ("from", json!(self.after.0)),
                    ("to", json!(id_time(self.to))),
                    ("after", json!(self.after.1)),
                    ("subject", json!(self.subject.clone().unwrap_or_default())),
                    ("page", json!(PAGE)),
                ],
            )
            .await?;
        let full = rows.len() >= PAGE;
        let mut fresh = 0;
        for raw in rows {
            let (ns, ulid, row) = row_from_db(raw)?;
            if self.last.as_deref() == Some(ulid.as_str()) {
                continue;
            }
            self.after = (ns, ulid.clone());
            self.last = Some(ulid.clone());
            self.buf.push_back((ns, ulid, row));
            fresh += 1;
        }
        if !full || fresh == 0 {
            self.done = true;
        }
        Ok(())
    }
}

/// Several cursors merged by time.
struct Merge {
    store: Store,
    cursors: Vec<Cursor>,
}

impl Merge {
    async fn fill(&mut self) -> anyhow::Result<()> {
        for c in &mut self.cursors {
            c.fill(&self.store).await?;
        }
        Ok(())
    }

    /// Up to `n` rows, oldest first; empty when nothing is left.
    async fn next_rows(&mut self, n: usize) -> anyhow::Result<Vec<AuditRow>> {
        let mut out = Vec::new();
        for _ in 0..n {
            self.fill().await?;
            let next = self
                .cursors
                .iter()
                .enumerate()
                .filter_map(|(i, c)| c.buf.front().map(|(ns, u, _)| (i, *ns, u.clone())))
                .min_by(|a, b| (a.1, &a.2).cmp(&(b.1, &b.2)));
            let Some((i, _, _)) = next else { break };
            let (_, _, row) = self.cursors[i].buf.pop_front().expect("a front row");
            out.push(row);
        }
        Ok(out)
    }
}

/// A stored row, its id time and its ulid.
fn row_from_db(raw: Value) -> anyhow::Result<(i64, String, AuditRow)> {
    let Value::Object(mut obj) = raw else {
        anyhow::bail!("an audit record isn't an object");
    };
    let key = obj.remove("key");
    obj.remove("id");
    let part = |i: usize| key.as_ref().and_then(|k| k.get(i)).cloned();
    let ns = part(1)
        .and_then(|v| v.as_i64())
        .context("an audit record without its time in the id")?;
    let ulid = part(2)
        .and_then(|v| v.as_str().map(str::to_string))
        .context("an audit record without an array id")?;
    let row = serde_json::from_value(Value::Object(obj)).context("bad audit record")?;
    Ok((ns, ulid, row))
}

// ---------------------------------------------------------------------------
// Names (owner Q7)
// ---------------------------------------------------------------------------

/// Subject names for one admin's read, cached per subject.
struct Names {
    state: AppState,
    admin: std::sync::Arc<Principal>,
    cache: HashMap<(String, String), Option<String>>,
    handles: HashMap<String, Option<String>>,
}

impl Names {
    fn new(state: AppState, admin: std::sync::Arc<Principal>) -> Self {
        Self {
            state,
            admin,
            cache: HashMap::new(),
            handles: HashMap::new(),
        }
    }

    async fn of(&mut self, subject: &AuditSubject) -> Option<String> {
        let key = (subject.kind.clone(), subject.id.clone());
        if let Some(name) = self.cache.get(&key) {
            return name.clone();
        }
        let name = match self.resolve(subject).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("couldn't name an audit subject: {e:#}");
                None
            }
        };
        self.cache.insert(key, name.clone());
        name
    }

    async fn handle(&mut self, person: &str) -> anyhow::Result<Option<String>> {
        if let Some(h) = self.handles.get(person) {
            return Ok(h.clone());
        }
        let h = self
            .state
            .store
            .get::<crate::people::PersonRow>(db::PERSON, person)
            .await?
            .map(|r| r.person.handle);
        self.handles.insert(person.to_string(), h.clone());
        Ok(h)
    }

    /// "a topic of @alice" (or "a topic" when the owner is gone).
    async fn of_someone(&mut self, what: &str, owner: &str) -> anyhow::Result<Option<String>> {
        Ok(Some(match self.handle(owner).await? {
            Some(h) => format!("{what} of @{h}"),
            None => what.to_string(),
        }))
    }

    async fn owned<T: serde::de::DeserializeOwned + Owned>(
        &mut self,
        table: &'static str,
        id: &str,
        what: &str,
        name: impl Fn(&T) -> String,
    ) -> anyhow::Result<Option<String>> {
        let Some(row) = self.state.store.get::<T>(table, id).await? else {
            return Ok(None);
        };
        if can_read(&self.admin, &row) {
            Ok(Some(name(&row)))
        } else {
            let owner = row.owner_id().to_string();
            self.of_someone(what, &owner).await
        }
    }

    async fn resolve(&mut self, s: &AuditSubject) -> anyhow::Result<Option<String>> {
        let store = self.state.store.clone();
        match s.kind.as_str() {
            "topic" => {
                self.owned::<Topic>(db::TOPIC, &s.id, "a topic", |t| t.title.clone())
                    .await
            }
            "workspace" => {
                self.owned::<margin_shared::Workspace>(db::WORKSPACE, &s.id, "a folder", |w| {
                    w.name.clone()
                })
                .await
            }
            "source" => {
                self.owned::<margin_shared::Source>(db::SOURCE, &s.id, "a source", |x| {
                    x.name.clone()
                })
                .await
            }
            "document" => {
                let Some(doc) = store.get::<Document>(db::DOCUMENT, &s.id).await? else {
                    return Ok(None);
                };
                let owner = doc.owner.clone().unwrap_or_default();
                let access = crate::state::effective_access(doc.owner.as_deref(), &doc.access);
                if Reader::of(&self.admin).allows(&access, &doc.deny) {
                    Ok(Some(doc.name))
                } else {
                    self.of_someone("a document", &owner).await
                }
            }
            "person" => Ok(self.handle(&s.id).await?.map(|h| format!("@{h}"))),
            "group" => Ok(store
                .get::<crate::groups::GroupRow>(db::GROUP, &s.id)
                .await?
                .map(|g| g.name)
                .or_else(|| (s.id == margin_shared::ROOT_GROUP).then(|| "Company".into()))),
            "agent" => {
                let Some(row) = store
                    .get::<crate::agents::AgentRow>(db::AGENT, &s.id)
                    .await?
                else {
                    return Ok(None);
                };
                Ok(Some(match self.handle(&row.agent.owner_id).await? {
                    Some(h) => format!("{} (@{h})", row.agent.name),
                    None => row.agent.name,
                }))
            }
            "invitation" => Ok(store
                .get::<crate::invitations::InvitationRow>(db::INVITATION, &s.id)
                .await?
                .map(|r| format!("@{}", r.invitation.github_login))),
            "device" => {
                let Some(row) = store.get::<DeviceRow>(db::DEVICE, &s.id).await? else {
                    return Ok(None);
                };
                Ok(Some(match row.person_id.as_deref() {
                    Some(p) => match self.handle(p).await? {
                        Some(h) => format!("{} (@{h})", row.name),
                        None => row.name,
                    },
                    None => row.name,
                }))
            }
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_lose_names_and_content() {
        let v = scrub(json!({
            "path": "company/eng", "name": "Engineering",
            "nested": [{"title": "Secret", "id": "t1"}], "text": "body"
        }));
        assert_eq!(v, json!({"path": "company/eng", "nested": [{"id": "t1"}]}));
        assert_eq!(scrub(json!(["a"])), json!({"value": ["a"]}));
        assert_eq!(scrub(Value::Null), Value::Null);
    }

    #[test]
    fn export_lines_become_rows_with_stable_ids() {
        let line = r#"{"at":"2026-10-15T08:00:00Z","person":"p1","by":"root","host":"mini","out":"/tmp/x"}"#;
        let parsed: ExportLine = serde_json::from_str(line).unwrap();
        assert!(parsed.ok);
        let row = parsed.to_row();
        assert_eq!(row.kind, AuditKind::Export);
        assert_eq!(row.actor, "host/root");
        assert_eq!(row.subject.id, "p1");
        assert_eq!(row.at, parsed.at);
        assert_eq!(export_ulid(line), export_ulid(&format!("{line}\n")));
        assert_ne!(export_ulid(line), export_ulid(&line.replace("p1", "p2")));
    }
}
