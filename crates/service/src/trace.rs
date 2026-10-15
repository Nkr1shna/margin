//! Trace rows for every model call, retrieval, tool call, ingest and job step, written
//! to SurrealDB as time series (`docs/agent-rag-architecture.md` §2.11): array ids
//! `trace:[kind, at, ulid]`, written in batches by one writer task off a bounded
//! channel; rollups are the `trace_hourly`/`trace_daily` views; raw rows are deleted by
//! range after `Settings.trace_retention_days`. Rows hold ids, numbers and hashes only.
//!
//! Conventions for whoever records rows:
//! - `trace_id` is the turn id for everything a chat turn does (its steps, tool calls,
//!   the retrievals its tools run, its condense and title calls) and the job id (lesson,
//!   video, proposal, document) for a job, so a tool sets `trace_id = ctx.turn_id`.
//! - Model calls carry `usage`; `cost_usd` is priced here, when the row is recorded,
//!   from `evals/prices.toml` (or `prices.toml` in the data folder), so a later price
//!   change doesn't rewrite history.
//! - `error_code` and `stop_reason` are codes (`[A-Za-z0-9_.:-]`, at most 64
//!   characters); anything else is recorded as `other`, so an error message (which can
//!   quote a prompt) never lands in a trace.
//! - A job step's name (`render`, …) is in `tool`.
//! - `principal` is who the work is for (`person/<id>` or `agent/<id>`, L1): a row
//!   recorded inside `with_principal` gets it when it doesn't name one, so a request
//!   handler or a turn task scopes itself once and every row under it is stamped.
//!   `GET /api/traces` and the rollups are for organisation admins only.

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

use anyhow::Context;
use arc_swap::ArcSwap;
use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use futures_util::Stream;
use margin_shared::context::{RollupBucket, TraceKind, TraceRollup, TraceRow, TraceUsage};
use margin_shared::pipeline::Route;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Notify, broadcast, mpsc};

use crate::db::{self, Store};
use crate::error::{AppError, AppResult};
use crate::principal::RequireAdmin;
use crate::state::AppState;

tokio::task_local! {
    /// The principal id the current task works for (`with_principal`).
    static PRINCIPAL: String;
}

/// Runs `f` with `principal` (`person/<id>` or `agent/<id>`) as the principal of every
/// trace row recorded inside it that doesn't name one.
pub async fn with_principal<F: std::future::Future>(principal: String, f: F) -> F::Output {
    PRINCIPAL.scope(principal, f).await
}

/// The principal `with_principal` set for this task, if any.
pub fn current_principal() -> Option<String> {
    PRINCIPAL.try_with(Clone::clone).ok()
}

/// Whether a row's `actor` is an outside agent's: `agent:<name>` (a device token naming
/// its agent) or `agent/<id>` (an agent token).
fn is_agent_actor(actor: &str) -> bool {
    actor.starts_with("agent:") || actor.starts_with(margin_shared::AGENT_PREFIX)
}

/// Rows waiting for the writer; when it is full new rows are dropped (and counted), so
/// tracing never slows a turn.
pub const TRACE_CHANNEL: usize = 4_096;
/// Rows the writer puts in one statement.
pub const WRITE_BATCH: usize = db::INSERT_MANY_MAX;
/// Rows read per range query when serving `GET /api/traces`.
const PAGE: usize = 500;
/// Rows kept for slow live subscribers (`Event::Trace`).
const LIVE_CHANNEL: usize = 1_024;
/// The price table built in (`evals/prices.toml`).
pub const PRICES_TOML: &str = include_str!("../../../evals/prices.toml");
/// A `prices.toml` in the data folder replaces the built-in table.
pub const PRICES_FILE: &str = "prices.toml";
/// Hourly rollups of deleted rows are kept this long; daily ones forever.
pub const HOURLY_ROLLUP_KEEP_DAYS: i64 = 365;
/// Rollups of rows retention deleted (the views follow deletes, so what they summed is
/// carried here first). Same shape as the views.
pub const TRACE_HOURLY_KEPT: &str = "trace_hourly_kept";
pub const TRACE_DAILY_KEPT: &str = "trace_daily_kept";
/// How often the retention job looks at the clock, and how often it deletes.
const RETENTION_TICK: Duration = Duration::from_secs(60 * 60);
const RETENTION_EVERY: chrono::Duration = chrono::Duration::hours(24);
/// How long `GET /api/traces` waits for rows recorded before it to be written.
const READ_FLUSH_WAIT: Duration = Duration::from_secs(2);
/// Longest code kept in `error_code`/`stop_reason`.
const CODE_MAX: usize = 64;

/// Prices per million tokens by model, from `evals/prices.toml`, for `cost_usd`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Prices {
    #[serde(default)]
    pub models: HashMap<String, ModelPrice>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
pub struct ModelPrice {
    pub input: f64,
    #[serde(default)]
    pub cached_input: f64,
    #[serde(default)]
    pub cache_write: f64,
    pub output: f64,
    /// USD per 1 000 hosted web searches.
    #[serde(default)]
    pub web_search: f64,
    /// A higher tier once a call's input passes `over` tokens (Claude Haiku 5.5).
    #[serde(default)]
    pub long_context: Option<LongContextPrice>,
}

/// The input and output prices of a call whose input passes `over` tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
pub struct LongContextPrice {
    pub over: u64,
    pub input: f64,
    pub output: f64,
}

impl Prices {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        toml::from_str(text).context("the price table can't be read")
    }

    /// The built-in table.
    pub fn builtin() -> Self {
        Self::parse(PRICES_TOML).unwrap_or_else(|e| {
            tracing::error!("{e:#}");
            Self::default()
        })
    }

    /// The model's prices: its own row, else the longest row id it starts with followed
    /// by `-` (a dated snapshot of a listed model). `None` for a model not listed.
    pub fn get(&self, model: &str) -> Option<&ModelPrice> {
        if let Some(p) = self.models.get(model) {
            return Some(p);
        }
        self.models
            .iter()
            .filter(|(id, _)| {
                model
                    .strip_prefix(id.as_str())
                    .is_some_and(|rest| rest.starts_with('-'))
            })
            .max_by_key(|(id, _)| id.len())
            .map(|(_, p)| p)
    }
}

/// Hands rows to the writer task.
pub struct Tracer {
    tx: mpsc::Sender<TraceRow>,
    rx: Mutex<Option<mpsc::Receiver<TraceRow>>>,
    prices: ArcSwap<Prices>,
    live: broadcast::Sender<TraceRow>,
    /// Rows accepted, rows written or given up on, rows dropped (channel full or a
    /// failed write).
    queued: AtomicU64,
    done: AtomicU64,
    dropped: AtomicU64,
    written: Notify,
    /// The audit trail's writer queue (`audit::record`), started with the tracer's.
    pub audit: crate::audit::Auditor,
}

impl Tracer {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(TRACE_CHANNEL);
        let (live, _) = broadcast::channel(LIVE_CHANNEL);
        Self {
            tx,
            rx: Mutex::new(Some(rx)),
            prices: ArcSwap::from_pointee(Prices::builtin()),
            live,
            queued: AtomicU64::new(0),
            done: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            written: Notify::new(),
            audit: crate::audit::Auditor::new(),
        }
    }

    /// Queues a row; never waits. An empty `id` gets a new ulid; a model call with usage
    /// and no `cost_usd` is priced now; codes are checked (see the module docs).
    pub fn record(&self, mut row: TraceRow) {
        if row.id.is_empty() {
            row.id = new_row_id();
        }
        if row.cost_usd.is_none()
            && let (Some(model), Some(usage)) = (&row.model, &row.usage)
        {
            row.cost_usd = cost_usd(&self.prices.load(), row.provider.as_deref(), model, usage);
        }
        if row.principal.is_none() {
            row.principal = current_principal();
        }
        // A query is kept only for an agent's context-API search, and only so long.
        let agent = row.via.is_some() && row.actor.as_deref().is_some_and(is_agent_actor);
        row.query = row.query.filter(|_| agent).map(|q| {
            q.chars()
                .take(margin_shared::context::TRACE_QUERY_MAX)
                .collect()
        });
        row.error_code = row.error_code.map(|c| sanitize_code(&c));
        row.stop_reason = row.stop_reason.map(|c| sanitize_code(&c));
        match self.tx.try_send(row) {
            Ok(()) => {
                self.queued.fetch_add(1, Ordering::SeqCst);
            }
            Err(_) => {
                let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_power_of_two() {
                    tracing::warn!(dropped = n, "trace rows dropped: the writer is behind");
                }
            }
        }
    }

    /// Starts a span of `kind` under `trace_id`; finishing it records the row with its
    /// duration.
    pub fn span(&self, kind: TraceKind, trace_id: &str) -> TraceSpan {
        let mut row = TraceRow::new(kind, new_row_id(), Utc::now());
        row.trace_id = trace_id.to_string();
        // Spans can finish in another task (a stream dropped elsewhere): stamp now.
        row.principal = current_principal();
        TraceSpan {
            row,
            started: Instant::now(),
        }
    }

    /// The receiving end, for the writer task; `None` once taken.
    pub fn take_receiver(&self) -> Option<mpsc::Receiver<TraceRow>> {
        self.rx.lock().ok()?.take()
    }

    /// Replaces the price table (rows recorded from now on use it).
    pub fn set_prices(&self, prices: Prices) {
        self.prices.store(std::sync::Arc::new(prices));
    }

    pub fn prices(&self) -> std::sync::Arc<Prices> {
        self.prices.load_full()
    }

    /// `model_call` and `retrieval` rows as they are written, for `Event::Trace` to
    /// sockets that asked for them (`/api/events?traces=1`).
    pub fn subscribe(&self) -> broadcast::Receiver<TraceRow> {
        self.live.subscribe()
    }

    /// Rows dropped so far (the channel was full, or a write failed).
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Waits, at most `wait`, until every row recorded before the call is written (or
    /// given up on). False on timeout.
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

    fn written(&self, rows: &[TraceRow], ok: bool) {
        if ok {
            for row in rows {
                if matches!(row.kind, TraceKind::ModelCall | TraceKind::Retrieval) {
                    // No subscriber is fine.
                    let _ = self.live.send(row.clone());
                }
            }
        } else {
            self.dropped.fetch_add(rows.len() as u64, Ordering::Relaxed);
        }
        self.done.fetch_add(rows.len() as u64, Ordering::SeqCst);
        self.written.notify_waiters();
    }
}

impl Default for Tracer {
    fn default() -> Self {
        Self::new()
    }
}

/// A row being timed.
pub struct TraceSpan {
    pub row: TraceRow,
    pub started: std::time::Instant,
}

impl TraceSpan {
    /// Records the row with `ok` and the elapsed time.
    pub fn finish(mut self, tracer: &Tracer, ok: bool) {
        self.row.ok = ok;
        self.row.duration_ms = self.started.elapsed().as_secs_f64() * 1000.0;
        tracer.record(self.row);
    }

    /// Records the row as failed with `code`.
    pub fn fail(mut self, tracer: &Tracer, code: &str) {
        self.row.error_code = Some(code.to_string());
        self.finish(tracer, false);
    }
}

/// The attempts of one job model call (`engines::job_model`): when the latest retried
/// attempt began and how long the provider took on the latest attempt. A [`SpanGuard`]
/// holding one times only the last attempt (each failed attempt before it has a row of
/// its own) and records the provider's share of it as `provider_ms`.
#[derive(Debug, Default)]
pub struct AttemptClock {
    retried_at: Mutex<Option<Instant>>,
    provider: Mutex<Option<Duration>>,
}

impl AttemptClock {
    /// A retried attempt begins now: the call's row is timed from here.
    pub fn retry_started(&self) {
        *self.retried_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    }

    /// The latest attempt's call to the provider took `took`.
    pub fn provider_took(&self, took: Duration) {
        *self.provider.lock().unwrap_or_else(|e| e.into_inner()) = Some(took);
    }

    /// Restarts `span`'s timer at the latest retry and sets its `provider_ms`.
    fn apply(&self, span: &mut TraceSpan) {
        if let Some(at) = *self.retried_at.lock().unwrap_or_else(|e| e.into_inner()) {
            span.started = at;
            span.row.at =
                Utc::now() - chrono::TimeDelta::from_std(at.elapsed()).unwrap_or_default();
        }
        if let Some(took) = *self.provider.lock().unwrap_or_else(|e| e.into_inner()) {
            span.row.provider_ms = Some(took.as_secs_f64() * 1000.0);
        }
    }
}

/// A span that records itself as failed with `cancelled` if it is dropped unfinished
/// (a stopped turn drops the futures it was running).
pub struct SpanGuard {
    span: Option<TraceSpan>,
    state: AppState,
    clock: Option<std::sync::Arc<AttemptClock>>,
}

impl SpanGuard {
    pub fn new(span: TraceSpan, state: &AppState) -> Self {
        Self {
            span: Some(span),
            state: state.clone(),
            clock: None,
        }
    }

    /// The row, to fill in before it is finished.
    pub fn row(&mut self) -> &mut TraceRow {
        &mut self.span.as_mut().expect("an unfinished span").row
    }

    /// The clock of this call's attempts, for `engines::JobCalls`: the row is timed
    /// from the last retried attempt's start and gets the provider's share of it.
    pub fn clock(&mut self) -> std::sync::Arc<AttemptClock> {
        self.clock.get_or_insert_with(Default::default).clone()
    }

    fn take(&mut self) -> Option<TraceSpan> {
        let mut span = self.span.take()?;
        if let Some(clock) = &self.clock {
            clock.apply(&mut span);
        }
        Some(span)
    }

    pub fn finish(mut self, ok: bool) {
        if let Some(span) = self.take() {
            span.finish(&self.state.tracer, ok);
        }
    }

    pub fn fail(mut self, code: &str) {
        if let Some(span) = self.take() {
            span.fail(&self.state.tracer, code);
        }
    }
}

impl Drop for SpanGuard {
    fn drop(&mut self) {
        if let Some(span) = self.take() {
            span.fail(&self.state.tracer, "cancelled");
        }
    }
}

/// A new row id: a ulid, so ids sort by time within a millisecond's kind and window.
pub fn new_row_id() -> String {
    ulid::Ulid::new().to_string()
}

/// The token counts of a call as a trace records them.
pub fn trace_usage(usage: &margin_shared::Usage) -> TraceUsage {
    TraceUsage {
        input: usage.input_tokens,
        cached: usage.cached_input_tokens,
        cache_write: usage.cache_write_input_tokens,
        output: usage.output_tokens,
        reasoning: usage.reasoning_tokens,
        web_searches: usage.web_search_requests,
        web_fetches: usage.web_fetch_requests,
    }
}

/// A model call row of `route` under `trace_id`, stamped with the pipeline in use.
pub fn model_call(
    state: &AppState,
    trace_id: &str,
    route: Route,
    model: &str,
    provider: &str,
    prompt_version: String,
) -> TraceSpan {
    let mut span = state.tracer.span(TraceKind::ModelCall, trace_id);
    span.row.route = Some(route);
    span.row.model = Some(model.to_string());
    span.row.provider = Some(provider.to_string());
    span.row.prompt_version = Some(prompt_version);
    span.row.pipeline_hash = Some(state.pipeline.load().hash.clone());
    span.row.actor = Some("owner".into());
    span
}

/// `name-<8 hex>`: a prompt's version from its fixed text, so a changed prompt is a
/// new version without anyone bumping a number. The hash is of instructions the code
/// holds, never of anything the owner wrote.
pub fn prompt_version(name: &str, fixed_text: &str) -> String {
    format!("{name}-{:08x}", fnv1a(fixed_text.as_bytes()) as u32)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// A code as given when it looks like one, else `other`.
pub fn sanitize_code(code: &str) -> String {
    let ok = !code.is_empty()
        && code.len() <= CODE_MAX
        && code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'));
    if ok { code.to_string() } else { "other".into() }
}

// ---------------------------------------------------------------------------
// Writer and retention
// ---------------------------------------------------------------------------

/// Starts the writer, the retention job and the alert job (`alerts::run`), and loads
/// the price table. Called once at startup (`server::Service::start`); later calls do
/// nothing.
pub fn start(state: &AppState) {
    let Some(rx) = state.tracer.take_receiver() else {
        return;
    };
    let path = state.config.data_dir.join(PRICES_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => match Prices::parse(&text) {
            Ok(prices) => state.tracer.set_prices(prices),
            Err(e) => tracing::error!("{}: {e:#}; using the built-in prices", path.display()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("couldn't read {}: {e}", path.display()),
    }
    tokio::spawn(run_writer(state.clone(), rx));
    tokio::spawn(run_retention(state.clone()));
    tokio::spawn(crate::alerts::run(state.clone()));
}

/// The writer task: batches rows into `trace:[kind, at, ulid]` records.
pub async fn run_writer(state: AppState, mut rx: mpsc::Receiver<TraceRow>) {
    while let Some(first) = rx.recv().await {
        let mut batch = vec![first];
        while batch.len() < WRITE_BATCH {
            match rx.try_recv() {
                Ok(row) => batch.push(row),
                Err(_) => break,
            }
        }
        let ok = match write_rows(&state.store, &batch).await {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(rows = batch.len(), "couldn't write trace rows: {e:#}");
                false
            }
        };
        state.tracer.written(&batch, ok);
    }
}

/// Writes up to `WRITE_BATCH` rows in one statement, as `trace:[kind, ns, ulid]`
/// (see `id_time`) with `at` a datetime (JSON can't carry one, so rows go through a
/// `FOR … CREATE` rather than `INSERT`); `None` fields are left out (`option<…>` fields
/// refuse `NULL`).
pub async fn write_rows(store: &Store, rows: &[TraceRow]) -> anyhow::Result<()> {
    anyhow::ensure!(
        rows.len() <= WRITE_BATCH,
        "at most {WRITE_BATCH} trace rows per statement, not {}",
        rows.len()
    );
    if rows.is_empty() {
        return Ok(());
    }
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let Value::Object(mut content) = serde_json::to_value(row)? else {
            anyhow::bail!("a trace row isn't an object");
        };
        content.remove("id");
        content.remove("at");
        content.retain(|_, v| !v.is_null());
        out.push(json!({
            "kind": row.kind.as_str(),
            "at": time_text(row.at),
            "ns": id_time(row.at),
            "ulid": row.id,
            "content": content,
        }));
    }
    store
        .query_rows::<Value>(
            "FOR $r IN $rows { \
               LET $at = <datetime>$r.at; \
               CREATE type::record('trace', [$r.kind, $r.ns, $r.ulid]) \
                 CONTENT object::extend($r.content, { at: $at }) RETURN NONE; \
             };",
            vec![("rows", Value::Array(out))],
        )
        .await
        .context("couldn't write trace rows")?;
    Ok(())
}

/// The time element of a trace id: nanoseconds since 1970, an int. A datetime there
/// (as §2.11 first had it) doesn't work in SurrealDB 3.3.4: datetimes inside array
/// record ids sort by their text, so `…T10:00:00.001Z` sorts before `…T10:00:00Z` and a
/// range bound on a whole second misses or takes rows. Ints sort as numbers.
pub fn id_time(at: DateTime<Utc>) -> i64 {
    at.timestamp_nanos_opt().unwrap_or(if at.timestamp() < 0 {
        i64::MIN
    } else {
        i64::MAX
    })
}

fn time_text(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

fn epoch() -> DateTime<Utc> {
    Utc.timestamp_opt(0, 0).unwrap()
}

/// Runs retention at start and then once a day (checked hourly).
async fn run_retention(state: AppState) {
    let mut tick = tokio::time::interval(RETENTION_TICK);
    let mut last: Option<DateTime<Utc>> = None;
    let mut shutdown = state.shutdown.clone();
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shutdown.changed() => return,
        }
        let now = Utc::now();
        if last.is_some_and(|l| now - l < RETENTION_EVERY) {
            continue;
        }
        match apply_retention(&state, now).await {
            Ok(()) => last = Some(now),
            Err(e) => tracing::warn!("trace retention failed: {e:#}"),
        }
    }
}

/// The start of the day `days` before `now`: rows before it are deleted. Whole days, so
/// the hourly and daily buckets carried into the kept rollups are complete.
pub fn retention_cutoff(now: DateTime<Utc>, days: u32) -> DateTime<Utc> {
    let day = (now - chrono::Duration::days(i64::from(days))).date_naive();
    Utc.from_utc_datetime(&day.and_hms_opt(0, 0, 0).unwrap())
}

/// Deletes raw rows older than the retention setting, by range per kind; rollups stay.
/// The views follow deletes, so in the same transaction what they summed for the
/// deleted buckets is added to `trace_hourly_kept`/`trace_daily_kept` first. Kept
/// hourly rollups older than a year go; daily ones stay.
pub async fn apply_retention(state: &AppState, now: DateTime<Utc>) -> anyhow::Result<()> {
    let days = state.settings_row().await?.trace_retention_days;
    let cutoff = retention_cutoff(now, days);
    let year = now - chrono::Duration::days(HOURLY_ROLLUP_KEEP_DAYS);
    delete_before(&state.store, cutoff, year).await?;
    tracing::debug!(cutoff = %cutoff, "trace retention applied");
    Ok(())
}

/// The 95th percentile of `duration_ms` over the raw rows of rollup row `row` (a
/// SurrealQL expression for a view row) in a bucket `width` wide (`1h`, `1d`): the
/// views can't keep a percentile (SurrealDB 3.3.4 refuses `math::percentile` in a
/// materialized aggregate view), so it is computed from the rows, by id range, when
/// a rollup is read and when retention carries a bucket into the kept tables.
/// SurrealDB interpolates between ranks.
fn p95_of(row: &str, width: &str) -> String {
    format!(
        "math::percentile((SELECT VALUE duration_ms FROM \
           trace:[{row}.kind, time::nano({row}.bucket), '']..[{row}.kind, time::nano({row}.bucket + {width}), ''] \
           WHERE route = {row}.route AND provider = {row}.provider AND model = {row}.model), 95)"
    )
}

/// Deletes raw rows before `cutoff` (carrying their rollups into the kept tables) and
/// kept hourly rollups before `hourly_keep`, in one transaction.
pub async fn delete_before(
    store: &Store,
    cutoff: DateTime<Utc>,
    hourly_keep: DateTime<Utc>,
) -> anyhow::Result<()> {
    let mut sql = format!(
        "DEFINE TABLE IF NOT EXISTS {TRACE_HOURLY_KEPT} SCHEMALESS; \
         DEFINE TABLE IF NOT EXISTS {TRACE_DAILY_KEPT} SCHEMALESS; \
         BEGIN TRANSACTION; \
         LET $cut = <datetime>$cut_text; \
         LET $year = <datetime>$year_text;",
    );
    for (view, kept, width) in [
        (db::TRACE_HOURLY, TRACE_HOURLY_KEPT, "1h"),
        (db::TRACE_DAILY, TRACE_DAILY_KEPT, "1d"),
    ] {
        let p95 = p95_of("$r", width);
        sql.push_str(&format!(
            "FOR $r IN (SELECT * FROM {view} WHERE bucket < $cut AND calls > 0) {{ \
               LET $key = record::id($r.id); \
               LET $new_p95 = {p95}; \
               LET $old = (SELECT * FROM type::record('{kept}', $key))[0]; \
               LET $calls = ($old.calls ?? 0) + $r.calls; \
               LET $p95 = IF $old.p95_ms != NONE THEN math::max([$old.p95_ms, $new_p95]) ELSE $new_p95 END; \
               UPSERT type::record('{kept}', $key) CONTENT {{ \
                 kind: $r.kind, route: $r.route, provider: $r.provider, model: $r.model, \
                 bucket: $r.bucket, calls: $calls, \
                 input: ($old.input ?? 0) + ($r.input ?? 0), \
                 cached: ($old.cached ?? 0) + ($r.cached ?? 0), \
                 output: ($old.output ?? 0) + ($r.output ?? 0), \
                 cost_usd: ($old.cost_usd ?? 0) + ($r.cost_usd ?? 0), \
                 mean_ms: (($old.mean_ms ?? 0) * ($old.calls ?? 0) + ($r.mean_ms ?? 0) * $r.calls) / $calls, \
                 p95_ms: $p95 \
               }} RETURN NONE; \
             }};"
        ));
    }
    for kind in TraceKind::ALL {
        sql.push_str(&format!(
            "DELETE trace:['{k}', $min, '']..=['{k}', $cut_ns, ''] RETURN NONE;",
            k = kind.as_str()
        ));
    }
    sql.push_str(&format!(
        "DELETE {TRACE_HOURLY_KEPT} WHERE bucket < $year RETURN NONE; COMMIT TRANSACTION;"
    ));
    store
        .query_rows::<Value>(
            &sql,
            vec![
                ("cut_text", json!(time_text(cutoff))),
                ("year_text", json!(time_text(hourly_keep))),
                ("cut_ns", json!(id_time(cutoff))),
                ("min", json!(i64::MIN)),
            ],
        )
        .await
        .context("couldn't apply trace retention")?;
    Ok(())
}

/// `cost_usd` of a call from its usage and the price table. `input` counts every input
/// token, `cached` and `cache_write` among them; reasoning is part of `output`; hosted web
/// searches add `web_search` per 1 000; past a model's `long_context.over` input tokens
/// its tier's input and output prices apply. `None` for a model the table doesn't list
/// (never a guess), and always for ChatGPT calls (`provider` "chatgpt": plan-billed, no
/// dollar price; the rollups still count their tokens).
pub fn cost_usd(
    prices: &Prices,
    provider: Option<&str>,
    model: &str,
    usage: &margin_shared::context::TraceUsage,
) -> Option<f64> {
    if provider == Some(margin_shared::Provider::ChatGpt.as_str()) {
        return None;
    }
    let p = prices.get(model)?;
    let (input, output) = match p.long_context {
        Some(tier) if usage.input > tier.over => (tier.input, tier.output),
        _ => (p.input, p.output),
    };
    let fresh = usage
        .input
        .saturating_sub(usage.cached)
        .saturating_sub(usage.cache_write);
    let micro = fresh as f64 * input
        + usage.cached as f64 * p.cached_input
        + usage.cache_write as f64 * p.cache_write
        + usage.output as f64 * output;
    Some(micro / 1_000_000.0 + usage.web_searches as f64 * p.web_search / 1_000.0)
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TracesQuery {
    pub kind: Option<TraceKind>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub trace_id: Option<String>,
}

fn bad_range() -> AppError {
    AppError::new(
        StatusCode::BAD_REQUEST,
        "bad_range",
        "`from` must not be after `to`.",
    )
}

/// `GET routes::TRACES` (admins): JSON lines, oldest first. Rows recorded before the
/// request are written first (up to 2 s), so a reader sees its own calls.
pub async fn list_traces(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    Query(q): Query<TracesQuery>,
) -> AppResult<Response> {
    state.tracer.flush(READ_FLUSH_WAIT).await;
    let from = q.from.unwrap_or_else(epoch);
    let to = q.to.unwrap_or_else(Utc::now);
    if from > to {
        return Err(bad_range());
    }
    let trace_id = q.trace_id.filter(|t| !t.is_empty());
    let kinds: Vec<TraceKind> = match q.kind {
        Some(k) => vec![k],
        None => TraceKind::ALL.to_vec(),
    };
    let mut merge = Merge {
        store: state.store.clone(),
        cursors: kinds
            .into_iter()
            .map(|kind| Cursor::new(kind, from, to, trace_id.clone()))
            .collect(),
    };
    // The first page now, so a store error is a 500 rather than a cut-off body.
    merge.fill().await?;
    let body = futures_util::stream::unfold(Some(merge), |merge| async move {
        let mut merge = merge?;
        match merge.next_lines(PAGE).await {
            Ok(Some(lines)) => Some((Ok::<_, std::io::Error>(Bytes::from(lines)), Some(merge))),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("couldn't read trace rows: {e:#}");
                Some((Err(std::io::Error::other("trace read failed")), None))
            }
        }
    });
    Ok((
        [(header::CONTENT_TYPE, "application/x-ndjson")],
        Body::from_stream(body),
    )
        .into_response())
}

/// Rows of one kind in a window, a page at a time.
struct Cursor {
    kind: TraceKind,
    to: DateTime<Utc>,
    trace_id: Option<String>,
    /// Where the next page starts (inclusive: id time, ulid) and the ulid last
    /// returned, skipped.
    after: (i64, String),
    last: Option<String>,
    buf: VecDeque<TraceRow>,
    done: bool,
}

impl Cursor {
    fn new(
        kind: TraceKind,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        trace_id: Option<String>,
    ) -> Self {
        Self {
            kind,
            to,
            trace_id,
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
        let filter = if self.trace_id.is_some() {
            "WHERE trace_id = $trace_id"
        } else {
            ""
        };
        let sql = format!(
            "SELECT *, record::id(id) AS key FROM trace:[$kind, $from, $after]..=[$kind, $to, '~'] \
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
                    ("trace_id", json!(self.trace_id.clone().unwrap_or_default())),
                    ("page", json!(PAGE)),
                ],
            )
            .await?;
        let full = rows.len() >= PAGE;
        let mut fresh = 0;
        for raw in rows {
            let (ns, row) = row_from_db(raw)?;
            if self.last.as_deref() == Some(row.id.as_str()) {
                continue;
            }
            self.after = (ns, row.id.clone());
            self.last = Some(row.id.clone());
            self.buf.push_back(row);
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

    /// Up to `n` rows as JSON lines; `None` when nothing is left.
    async fn next_lines(&mut self, n: usize) -> anyhow::Result<Option<String>> {
        let mut out = String::new();
        for _ in 0..n {
            self.fill().await?;
            let next = self
                .cursors
                .iter()
                .enumerate()
                .filter_map(|(i, c)| c.buf.front().map(|r| (i, r.at, r.id.clone())))
                .min_by(|a, b| (a.1, &a.2).cmp(&(b.1, &b.2)));
            let Some((i, _, _)) = next else { break };
            let row = self.cursors[i].buf.pop_front().expect("a front row");
            out.push_str(&serde_json::to_string(&row)?);
            out.push('\n');
        }
        Ok((!out.is_empty()).then_some(out))
    }
}

/// A stored row (the id's ulid back in `id`) and its id time.
fn row_from_db(raw: Value) -> anyhow::Result<(i64, TraceRow)> {
    let Value::Object(mut obj) = raw else {
        anyhow::bail!("a trace record isn't an object");
    };
    let key = obj.remove("key");
    let part = |i: usize| key.as_ref().and_then(|k| k.get(i)).cloned();
    let ns = part(1)
        .and_then(|v| v.as_i64())
        .context("a trace record without its time in the id")?;
    let ulid = part(2)
        .and_then(|v| v.as_str().map(str::to_string))
        .context("a trace record without an array id")?;
    obj.insert("id".into(), Value::String(ulid));
    let row = serde_json::from_value(Value::Object(obj)).context("bad trace record")?;
    Ok((ns, row))
}

/// Every stored trace row of `kind` (all of them when `None`) between `from` and `to`,
/// oldest first. For tests and the export.
pub async fn read_rows(
    store: &Store,
    kind: Option<TraceKind>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    trace_id: Option<String>,
) -> anyhow::Result<Vec<TraceRow>> {
    let kinds: Vec<TraceKind> = kind.map_or_else(|| TraceKind::ALL.to_vec(), |k| vec![k]);
    let mut merge = Merge {
        store: store.clone(),
        cursors: kinds
            .into_iter()
            .map(|k| Cursor::new(k, from, to, trace_id.clone()))
            .collect(),
    };
    let mut rows = Vec::new();
    while let Some(lines) = merge.next_lines(PAGE).await? {
        for line in lines.lines() {
            rows.push(serde_json::from_str(line)?);
        }
    }
    Ok(rows)
}

#[derive(Debug, Deserialize)]
pub struct RollupQuery {
    pub bucket: RollupBucket,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
}

/// A rollup row as stored (sums come back as numbers of either kind).
#[derive(Debug, Deserialize)]
struct StoredRollup {
    kind: margin_shared::context::TraceKind,
    #[serde(default)]
    route: Option<Route>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    bucket: DateTime<Utc>,
    #[serde(default)]
    calls: f64,
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    cached: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
    #[serde(default)]
    cost_usd: Option<f64>,
    #[serde(default)]
    mean_ms: Option<f64>,
    #[serde(default)]
    p95_ms: Option<f64>,
}

/// `GET routes::TRACES_ROLLUP`: the view's buckets plus those kept from rows retention
/// deleted, oldest first. Admins only.
pub async fn rollup(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    Query(q): Query<RollupQuery>,
) -> AppResult<Json<Vec<TraceRollup>>> {
    state.tracer.flush(READ_FLUSH_WAIT).await;
    let from = q.from.unwrap_or_else(epoch);
    let to = q.to.unwrap_or_else(Utc::now);
    if from > to {
        return Err(bad_range());
    }
    Ok(Json(read_rollup(&state.store, q.bucket, from, to).await?))
}

/// See `rollup`.
pub async fn read_rollup(
    store: &Store,
    bucket: RollupBucket,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> anyhow::Result<Vec<TraceRollup>> {
    let (view, kept, width) = match bucket {
        RollupBucket::Hour => (db::TRACE_HOURLY, TRACE_HOURLY_KEPT, "1h"),
        RollupBucket::Day => (db::TRACE_DAILY, TRACE_DAILY_KEPT, "1d"),
    };
    type Key = (
        TraceKind,
        Option<Route>,
        Option<String>,
        Option<String>,
        DateTime<Utc>,
    );
    let mut merged: HashMap<Key, TraceRollup> = HashMap::new();
    for table in [view, kept] {
        // The view's p95 from its raw rows; the kept table's as carried.
        let fields = if table == view {
            format!("*, {} AS p95_ms", p95_of("$parent", width))
        } else {
            "*".to_string()
        };
        let rows: Vec<StoredRollup> = store
            .query_rows(
                &format!(
                    "DEFINE TABLE IF NOT EXISTS {kept} SCHEMALESS; \
                     LET $from = <datetime>$from_text; LET $to = <datetime>$to_text; \
                     SELECT {fields} OMIT id FROM {table} WHERE bucket >= $from AND bucket <= $to;"
                ),
                vec![
                    ("from_text", json!(time_text(from))),
                    ("to_text", json!(time_text(to))),
                ],
            )
            .await?;
        for r in rows {
            if r.calls < 0.5 {
                continue;
            }
            let calls = r.calls.round() as u64;
            let mean = r.mean_ms.filter(|m| m.is_finite()).unwrap_or(0.0);
            let key = (
                r.kind,
                r.route,
                r.provider.clone(),
                r.model.clone(),
                r.bucket,
            );
            let int = |v: Option<f64>| v.filter(|v| v.is_finite()).unwrap_or(0.0).round() as u64;
            let add = TraceRollup {
                kind: r.kind,
                route: r.route,
                provider: r.provider,
                model: r.model,
                bucket: r.bucket,
                calls,
                input: int(r.input),
                cached: int(r.cached),
                output: int(r.output),
                cost_usd: r.cost_usd.filter(|c| c.is_finite()).unwrap_or(0.0),
                mean_ms: mean,
                p95_ms: r.p95_ms.filter(|p| p.is_finite()),
            };
            match merged.get_mut(&key) {
                Some(have) => {
                    let total = have.calls + add.calls;
                    have.mean_ms = (have.mean_ms * have.calls as f64
                        + add.mean_ms * add.calls as f64)
                        / total as f64;
                    have.calls = total;
                    have.input += add.input;
                    have.cached += add.cached;
                    have.output += add.output;
                    have.cost_usd += add.cost_usd;
                    // A bucket split between the view and the kept table: the larger
                    // p95 of the two (a bound, never an understatement).
                    have.p95_ms = match (have.p95_ms, add.p95_ms) {
                        (Some(a), Some(b)) => Some(a.max(b)),
                        (a, b) => a.or(b),
                    };
                }
                None => {
                    merged.insert(key, add);
                }
            }
        }
    }
    let mut out: Vec<TraceRollup> = merged.into_values().collect();
    let order = |r: &TraceRollup| {
        (
            r.bucket,
            r.kind.as_str(),
            r.route.map(|route| route as u8),
            r.provider.clone(),
            r.model.clone(),
        )
    };
    out.sort_by_key(order);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Wrapping a stream to finish a model call's span
// ---------------------------------------------------------------------------

/// A model call's stream that records its span when the call ends: usage and stop
/// reason from the terminal event, `ok: false` with a code when it fails, ends early or
/// is dropped (a stopped turn).
///
/// `provider_ms` is the provider's share of the span: the time until the stream was
/// handed over (the request and its response headers, timed from the span's start), plus
/// every wait for the provider's next event (from a poll that found nothing ready to the
/// poll that got the event) and the polls themselves. What is left of `duration_ms` is
/// Margin's: building the request, and handling each event between polls.
pub struct TracedStream {
    inner: margin_ai::ModelStream,
    span: Option<TraceSpan>,
    state: AppState,
    provider: Duration,
    /// When a poll last found nothing ready, until the next event arrives.
    waiting_since: Option<Instant>,
    /// Whether the stream brought a finished item yet.
    output: bool,
    /// Set when the stream fails before any finished item (the agent loop retries
    /// that step), cleared when it ends otherwise.
    failed_without_output: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl TracedStream {
    pub fn new(inner: margin_ai::ModelStream, span: TraceSpan, state: AppState) -> Self {
        let provider = span.started.elapsed();
        Self {
            inner,
            span: Some(span),
            state,
            provider,
            waiting_since: None,
            output: false,
            failed_without_output: None,
        }
    }

    /// Sets `flag` when the stream fails before any finished item and clears it when
    /// it ends otherwise.
    pub fn flag_failure_without_output(
        mut self,
        flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        self.failed_without_output = Some(flag);
        self
    }

    /// The provider's time so far, in ms.
    pub fn provider_ms(&self) -> f64 {
        self.provider.as_secs_f64() * 1000.0
    }

    fn end(
        &mut self,
        ok: bool,
        code: Option<&str>,
        usage: Option<&margin_shared::Usage>,
        stop: Option<&str>,
        model: Option<&str>,
    ) {
        let Some(mut span) = self.span.take() else {
            return;
        };
        if let Some(flag) = &self.failed_without_output {
            flag.store(!ok && !self.output, Ordering::Relaxed);
        }
        // Dropped while waiting for the provider: that wait was the provider's.
        if let Some(since) = self.waiting_since.take() {
            self.provider += since.elapsed();
        }
        let provider_ms = self.provider_ms();
        span.row.provider_ms = Some(provider_ms);
        // What the provider answered with, when it said.
        if let Some(model) = model.filter(|m| !m.is_empty()) {
            span.row.model = Some(model.to_string());
        }
        if let Some(u) = usage {
            span.row.usage = Some(trace_usage(u));
        }
        span.row.stop_reason = stop.map(str::to_string);
        span.row.error_code = code.map(str::to_string);
        span.finish(&self.state.tracer, ok);
    }
}

impl Stream for TracedStream {
    type Item = Result<margin_ai::ModelEvent, margin_ai::AiError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        use margin_ai::ModelEvent as E;
        let polled_at = Instant::now();
        let polled = self.inner.as_mut().poll_next(cx);
        if self.span.is_some() {
            let poll = polled_at.elapsed();
            let add = match (&polled, self.waiting_since) {
                (Poll::Pending, None) => {
                    self.waiting_since = Some(polled_at);
                    Duration::ZERO
                }
                // Still waiting: the wait counts once, when the event arrives.
                (Poll::Pending, Some(_)) => Duration::ZERO,
                (Poll::Ready(_), Some(since)) => {
                    self.waiting_since = None;
                    polled_at.duration_since(since) + poll
                }
                (Poll::Ready(_), None) => poll,
            };
            self.provider += add;
        }
        match &polled {
            Poll::Ready(Some(Ok(event))) => match event {
                E::Done { item, .. } => {
                    let compacted = item.view == margin_ai::ItemView::Compaction;
                    self.output = true;
                    // The provider compacted the history in this call (D3).
                    if compacted && let Some(span) = self.span.as_mut() {
                        span.row.compaction = Some(margin_shared::context::Compaction::Server);
                    }
                }
                E::Completed {
                    usage,
                    model,
                    stop_reason,
                    ..
                } => {
                    let usage = *usage;
                    let stop = if stop_reason.is_empty() {
                        "completed".to_string()
                    } else {
                        stop_reason.clone()
                    };
                    let model = model.clone();
                    self.end(true, None, Some(&usage), Some(&stop), Some(&model));
                }
                E::Incomplete { reason, usage } => {
                    let (reason, usage) = (format!("incomplete:{reason}"), *usage);
                    self.end(true, None, Some(&usage), Some(&reason), None);
                }
                E::Failed { code, .. } => {
                    let code = code.clone().unwrap_or_else(|| "failed".into());
                    self.end(false, Some(&code), None, Some("failed"), None);
                }
                _ => {}
            },
            Poll::Ready(Some(Err(e))) => {
                let code = ai_error_code(e);
                self.end(false, Some(code), None, None, None);
            }
            Poll::Ready(None) => self.end(false, Some("ended_early"), None, None, None),
            Poll::Pending => {}
        }
        polled
    }
}

impl Drop for TracedStream {
    fn drop(&mut self) {
        self.end(false, Some("cancelled"), None, None, None);
    }
}

/// A code for a failed model call (never its message): the chat error codes of
/// `docs/agent-providers-brief.md` D7 for what the provider said, by its error code
/// first (a spend cap, a ChatGPT plan share used up or not eligible), then by status.
pub fn ai_error_code(e: &margin_ai::AiError) -> &'static str {
    use margin_ai::AiError;
    use margin_shared::error_code as code;
    match e {
        AiError::Http { code: Some(c), .. } if c == "enforced_spend_limit_reached" => {
            code::SPEND_LIMIT_REACHED
        }
        AiError::Http { code: Some(c), .. }
            if c == "subscription_sharing_usage_limit_exceeded"
                || c == "subscription_sharing_usage_unavailable" =>
        {
            code::PLAN_LIMIT_REACHED
        }
        AiError::Http { code: Some(c), .. } if c == "subscription_sharing_user_not_eligible" => {
            code::PLAN_INELIGIBLE
        }
        AiError::Http { status: 429, .. } => code::RATE_LIMITED,
        AiError::Http { status, .. } if *status >= 500 => code::SERVER_ERROR,
        AiError::Http {
            status: 401..=403, ..
        } => code::AUTH_FAILED,
        AiError::Http { .. } => "bad_request",
        AiError::Network(_) => "network",
        AiError::Parse(_) => "parse",
        AiError::Incomplete(_) => "incomplete",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        text.parse().unwrap()
    }

    fn row(kind: TraceKind, at_text: &str, trace_id: &str) -> TraceRow {
        let mut r = TraceRow::new(kind, new_row_id(), at(at_text));
        r.trace_id = trace_id.into();
        r
    }

    #[tokio::test]
    async fn rows_inside_with_principal_are_stamped() {
        let tracer = Tracer::new();
        let mut rx = tracer.take_receiver().unwrap();
        tracer.record(row(TraceKind::ToolCall, "2026-10-14T08:00:00Z", "t0"));
        let span = with_principal("agent/a1".into(), async {
            tracer.record(row(TraceKind::ToolCall, "2026-10-14T08:00:00Z", "t1"));
            let mut named = row(TraceKind::ToolCall, "2026-10-14T08:00:00Z", "t2");
            named.principal = Some("person/p".into());
            tracer.record(named);
            assert_eq!(current_principal().as_deref(), Some("agent/a1"));
            tracer.span(TraceKind::ModelCall, "t3")
        })
        .await;
        // Finished outside the scope: stamped when it started.
        span.finish(&tracer, true);
        let got: Vec<Option<String>> = (0..4).map(|_| rx.try_recv().unwrap().principal).collect();
        assert_eq!(
            got,
            vec![
                None,
                Some("agent/a1".into()),
                Some("person/p".into()),
                Some("agent/a1".into())
            ]
        );
        assert!(current_principal().is_none());
    }

    #[tokio::test]
    async fn an_agent_tokens_query_is_kept() {
        let tracer = Tracer::new();
        let mut rx = tracer.take_receiver().unwrap();
        for actor in ["agent/a1", "agent:claude-code", "owner"] {
            let mut r = row(TraceKind::Retrieval, "2026-10-14T08:00:00Z", "t");
            r.actor = Some(actor.into());
            r.via = Some(margin_shared::context::VIA_MCP.into());
            r.query = Some("carnot".into());
            tracer.record(r);
        }
        let kept: Vec<bool> = (0..3)
            .map(|_| rx.try_recv().unwrap().query.is_some())
            .collect();
        assert_eq!(kept, vec![true, true, false]);
    }

    #[test]
    fn prices_parse_and_cost_calls() {
        let prices = Prices::builtin();
        let opus = prices.get("claude-opus-5-5").expect("listed");
        assert_eq!(opus.input, 4.0);
        assert!(
            prices.get("claude-opus-5-5-20261001").is_some(),
            "a snapshot"
        );
        assert!(prices.get("claude-opus-5").is_none());
        assert!(prices.get("unknown-model").is_none());
        let usage = TraceUsage {
            input: 1_000_000,
            cached: 500_000,
            cache_write: 100_000,
            output: 10_000,
            reasoning: 5_000,
            web_searches: 0,
            ..Default::default()
        };
        // 400k fresh × 4 + 500k × 0.2 + 100k × 5 + 10k × 20, per million.
        let cost = cost_usd(&prices, Some("claude"), "claude-opus-5-5", &usage).unwrap();
        assert!((cost - (1.6 + 0.1 + 0.5 + 0.2)).abs() < 1e-9, "{cost}");
        assert_eq!(
            cost_usd(&prices, Some("openai"), "gpt-unknown", &usage),
            None
        );

        // Web searches at $10 per 1 000.
        let searched = TraceUsage {
            web_searches: 3,
            ..usage
        };
        let cost = cost_usd(&prices, Some("claude"), "claude-opus-5-5", &searched).unwrap();
        assert!((cost - (2.4 + 0.03)).abs() < 1e-9, "{cost}");
        // Haiku 5.5 past 100k input tokens: $0.50 / $2.50 instead of $0.10 / $0.50.
        let short = TraceUsage {
            input: 100_000,
            output: 1_000_000,
            ..Default::default()
        };
        let cost = cost_usd(&prices, Some("claude"), "claude-haiku-5-5", &short).unwrap();
        assert!((cost - (0.01 + 0.5)).abs() < 1e-9, "{cost}");
        let long = TraceUsage {
            input: 200_000,
            ..short
        };
        let cost = cost_usd(&prices, Some("claude"), "claude-haiku-5-5", &long).unwrap();
        assert!((cost - (0.1 + 2.5)).abs() < 1e-9, "{cost}");
        // OpenAI is priced; ChatGPT never is, whatever the model.
        let luna = cost_usd(&prices, Some("openai"), "gpt-6-luna", &short).unwrap();
        assert!((luna - (0.01 + 0.5)).abs() < 1e-9, "{luna}");
        assert_eq!(
            cost_usd(&prices, Some("chatgpt"), "gpt-6-luna", &short),
            None
        );
    }

    #[test]
    fn codes_never_carry_messages() {
        assert_eq!(sanitize_code("rate_limited"), "rate_limited");
        assert_eq!(
            sanitize_code("incomplete:max_output_tokens"),
            "incomplete:max_output_tokens"
        );
        assert_eq!(sanitize_code("the prompt said: hello"), "other");
        assert_eq!(sanitize_code(&"a".repeat(65)), "other");
        assert_eq!(sanitize_code(""), "other");
    }

    #[test]
    fn prompt_versions_follow_the_text() {
        let a = prompt_version("agent", "You are…");
        assert!(a.starts_with("agent-") && a.len() == "agent-".len() + 8);
        assert_eq!(a, prompt_version("agent", "You are…"));
        assert_ne!(a, prompt_version("agent", "You are!"));
    }

    #[test]
    fn retention_cuts_at_the_start_of_a_day() {
        let now = at("2026-10-14T10:03:21Z");
        assert_eq!(retention_cutoff(now, 90), at("2026-07-16T00:00:00Z"));
        assert_eq!(retention_cutoff(now, 1), at("2026-10-13T00:00:00Z"));
    }

    #[tokio::test]
    async fn records_are_priced_and_flushed() {
        let tracer = Tracer::new();
        let mut rx = tracer.take_receiver().unwrap();
        assert!(tracer.take_receiver().is_none());
        let mut span = tracer.span(TraceKind::ModelCall, "t1");
        span.row.model = Some("claude-haiku-5-5".into());
        span.row.usage = Some(TraceUsage {
            input: 2_000_000,
            output: 0,
            ..Default::default()
        });
        span.row.error_code = Some("a message with spaces".into());
        span.finish(&tracer, true);
        let got = rx.recv().await.unwrap();
        assert_eq!(got.trace_id, "t1");
        // 2M input tokens on Haiku 5.5: past 100k, its long-context $0.50 applies.
        assert!((got.cost_usd.unwrap() - 1.0).abs() < 1e-9);
        assert_eq!(got.error_code.as_deref(), Some("other"));
        assert!(got.duration_ms >= 0.0 && !got.id.is_empty());
        // Nothing marks it written: flush times out rather than hanging.
        assert!(!tracer.flush(Duration::from_millis(20)).await);
        tracer.written(&[got], true);
        assert!(tracer.flush(Duration::from_millis(20)).await);
    }

    #[tokio::test]
    async fn rows_round_trip_by_kind_window_and_trace() {
        let s = Store::memory().await.unwrap();
        let mut call = row(TraceKind::ModelCall, "2026-10-14T10:00:00.5Z", "t1");
        call.route = Some(Route::Chat);
        call.model = Some("m".into());
        call.usage = Some(TraceUsage {
            input: 10,
            cached: 4,
            output: 2,
            ..Default::default()
        });
        call.cost_usd = Some(0.5);
        call.step = Some(1);
        let mut tool = row(TraceKind::ToolCall, "2026-10-14T10:00:01Z", "t1");
        tool.tool = Some("search".into());
        tool.output_bytes = Some(123);
        let mut retrieval = row(TraceKind::Retrieval, "2026-10-14T10:00:00.7Z", "t1");
        retrieval.hit_ids = vec!["p1".into(), "p2".into()];
        retrieval.scores = vec![0.5, 0.25];
        retrieval.scope = Some(margin_shared::context::Scope::Library);
        retrieval.legs = Some(Default::default());
        let other = row(TraceKind::ModelCall, "2026-10-14T10:00:02Z", "t2");
        let old = row(TraceKind::ModelCall, "2026-10-13T10:00:00Z", "t0");
        write_rows(
            &s,
            &[
                call.clone(),
                tool.clone(),
                retrieval.clone(),
                other.clone(),
                old.clone(),
            ],
        )
        .await
        .unwrap();

        let all = read_rows(&s, None, epoch(), at("2100-01-01T00:00:00Z"), None)
            .await
            .unwrap();
        let ids: Vec<&str> = all.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, [&old.id, &call.id, &retrieval.id, &tool.id, &other.id]);
        assert_eq!(all[1], call, "every field round-trips");
        assert_eq!(all[2], retrieval);
        assert_eq!(all[3], tool);

        let t1 = read_rows(
            &s,
            None,
            epoch(),
            at("2100-01-01T00:00:00Z"),
            Some("t1".into()),
        )
        .await
        .unwrap();
        assert_eq!(t1.len(), 3);
        let calls = read_rows(
            &s,
            Some(TraceKind::ModelCall),
            at("2026-10-14T00:00:00Z"),
            at("2026-10-15T00:00:00Z"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            calls.iter().map(|r| &r.id).collect::<Vec<_>>(),
            [&call.id, &other.id]
        );
    }

    #[tokio::test]
    async fn reading_pages_through_many_rows() {
        let s = Store::memory().await.unwrap();
        let base = at("2026-10-14T10:00:00Z");
        let mut written = Vec::new();
        for chunk in 0..((PAGE * 2 + 10) / WRITE_BATCH + 1) {
            let rows: Vec<TraceRow> = (0..WRITE_BATCH)
                .map(|i| {
                    // Pairs of rows share a millisecond, so pages split inside one.
                    let n = (chunk * WRITE_BATCH + i) as i64;
                    TraceRow::new(
                        TraceKind::ToolCall,
                        new_row_id(),
                        base + chrono::Duration::milliseconds(n / 2),
                    )
                })
                .collect();
            write_rows(&s, &rows).await.unwrap();
            written.extend(rows);
        }
        written.sort_by(|a, b| (a.at, &a.id).cmp(&(b.at, &b.id)));
        let read = read_rows(&s, None, epoch(), at("2100-01-01T00:00:00Z"), None)
            .await
            .unwrap();
        assert_eq!(read.len(), written.len());
        for (i, (a, b)) in read.iter().zip(&written).enumerate() {
            assert_eq!((a.at, &a.id), (b.at, &b.id), "row {i}");
        }
    }

    #[tokio::test]
    async fn retention_deletes_raw_rows_and_keeps_their_rollups() {
        let s = Store::memory().await.unwrap();
        let call = |at_text: &str, input: u64, ms: f64| {
            let mut r = row(TraceKind::ModelCall, at_text, "t");
            r.route = Some(Route::Small);
            r.provider = Some("openai".into());
            r.model = Some("m".into());
            r.usage = Some(TraceUsage {
                input,
                cached: 1,
                output: 2,
                ..Default::default()
            });
            r.cost_usd = Some(0.25);
            r.duration_ms = ms;
            r
        };
        let rows = vec![
            call("2025-01-01T10:10:00Z", 10, 100.0),
            call("2025-01-01T10:20:00Z", 30, 300.0),
            call("2026-10-12T09:00:00Z", 5, 50.0),
            call("2026-10-14T09:00:00Z", 7, 70.0),
            row(TraceKind::Retrieval, "2026-10-12T09:30:00Z", "t"),
            row(TraceKind::Retrieval, "2026-10-14T09:30:00Z", "t"),
        ];
        write_rows(&s, &rows).await.unwrap();
        let window = (epoch(), at("2100-01-01T00:00:00Z"));
        let daily_before = read_rollup(&s, RollupBucket::Day, window.0, window.1)
            .await
            .unwrap();
        let hourly_before = read_rollup(&s, RollupBucket::Hour, window.0, window.1)
            .await
            .unwrap();

        // Keep hourly rollups from 2025-06-01 on; delete raw rows before 2026-10-13.
        let cutoff = at("2026-10-13T00:00:00Z");
        delete_before(&s, cutoff, at("2025-06-01T00:00:00Z"))
            .await
            .unwrap();
        // Twice is the same as once (nothing double counts).
        delete_before(&s, cutoff, at("2025-06-01T00:00:00Z"))
            .await
            .unwrap();

        let left = read_rows(&s, None, window.0, window.1, None).await.unwrap();
        assert_eq!(left.len(), 2);
        assert!(left.iter().all(|r| r.at >= cutoff));
        // Daily rollups are kept whole.
        let daily = read_rollup(&s, RollupBucket::Day, window.0, window.1)
            .await
            .unwrap();
        assert_eq!(daily, daily_before);
        let jan = daily
            .iter()
            .find(|r| r.bucket == at("2025-01-01T00:00:00Z"))
            .unwrap();
        assert_eq!(
            (jan.calls, jan.input, jan.cached, jan.output),
            (2, 40, 2, 4)
        );
        assert!((jan.cost_usd - 0.5).abs() < 1e-9 && (jan.mean_ms - 200.0).abs() < 1e-9);
        // The p95 (interpolated over 100 and 300 ms) computed while the rows lived, and
        // carried into the kept rollup when they went.
        let p95 = jan.p95_ms.expect("a p95");
        assert!((p95 - 290.0).abs() < 1e-9, "{p95}");
        assert!(
            daily_before.iter().all(|r| r.p95_ms.is_some()),
            "{daily_before:#?}"
        );
        // Hourly rollups older than the hourly keep are gone; the rest are whole.
        let hourly = read_rollup(&s, RollupBucket::Hour, window.0, window.1)
            .await
            .unwrap();
        let expected: Vec<TraceRollup> = hourly_before
            .into_iter()
            .filter(|r| r.bucket >= at("2025-06-01T00:00:00Z"))
            .collect();
        assert_eq!(hourly, expected);
        assert_eq!(hourly.len(), 4);
    }

    #[tokio::test]
    async fn too_many_rows_for_one_statement_are_refused() {
        let s = Store::memory().await.unwrap();
        let rows: Vec<TraceRow> = (0..=WRITE_BATCH)
            .map(|_| TraceRow::new(TraceKind::Turn, new_row_id(), Utc::now()))
            .collect();
        assert!(write_rows(&s, &rows).await.is_err());
    }
}
