//! Trace rows for every model call, retrieval, tool call, ingest and job step, written
//! to SurrealDB as time series (`docs/agent-rag-architecture.md` §2.11): array ids
//! `trace:[kind, at, ulid]`, written in batches by one writer task off a bounded
//! channel; rollups are the `trace_hourly`/`trace_daily` views; raw rows are deleted by
//! range after `Settings.trace_retention_days`. Rows hold ids, numbers and hashes only.
//!
//! R1-trace fills the bodies; the signatures are the seam.

use std::collections::HashMap;
use std::sync::Mutex;

use axum::Json;
use axum::extract::{Query, State};
use axum::response::Response;
use chrono::{DateTime, Utc};
use margin_shared::context::{RollupBucket, TraceKind, TraceRollup, TraceRow};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::error::AppResult;
use crate::state::AppState;

/// Rows waiting for the writer; when it is full new rows are dropped (and counted), so
/// tracing never slows a turn.
pub const TRACE_CHANNEL: usize = 4_096;

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
}

/// Hands rows to the writer task.
pub struct Tracer {
    tx: mpsc::Sender<TraceRow>,
    rx: Mutex<Option<mpsc::Receiver<TraceRow>>>,
}

impl Tracer {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(TRACE_CHANNEL);
        Self {
            tx,
            rx: Mutex::new(Some(rx)),
        }
    }

    /// Queues a row; never waits.
    pub fn record(&self, _row: TraceRow) {
        let _ = &self.tx;
        unimplemented!("R1, trace: Tracer::record")
    }

    /// Starts a span of `kind` under `trace_id`; finishing it records the row with its
    /// duration.
    pub fn span(&self, _kind: TraceKind, _trace_id: &str) -> TraceSpan {
        unimplemented!("R1, trace: Tracer::span")
    }

    /// The receiving end, for the writer task; `None` once taken.
    pub fn take_receiver(&self) -> Option<mpsc::Receiver<TraceRow>> {
        self.rx.lock().ok()?.take()
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
    pub fn finish(self, _tracer: &Tracer, _ok: bool) {
        unimplemented!("R1, trace: TraceSpan::finish")
    }
}

/// The writer task: batches rows into `trace:[kind, at, ulid]` records.
pub async fn run_writer(_state: AppState, _rx: mpsc::Receiver<TraceRow>) {
    unimplemented!("R1, trace: run_writer")
}

/// Deletes raw rows older than the retention setting, by range per kind; rollups stay.
pub async fn apply_retention(_state: &AppState, _now: DateTime<Utc>) -> anyhow::Result<()> {
    unimplemented!("R1, trace: apply_retention")
}

/// `cost_usd` of a call from its usage and the price table.
pub fn cost_usd(
    _prices: &Prices,
    _model: &str,
    _usage: &margin_shared::context::TraceUsage,
) -> Option<f64> {
    unimplemented!("R1, trace: cost_usd")
}

#[derive(Debug, Deserialize)]
pub struct TracesQuery {
    pub kind: Option<TraceKind>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub trace_id: Option<String>,
}

/// `GET routes::TRACES`: JSON lines, oldest first.
pub async fn list_traces(
    State(_state): State<AppState>,
    Query(_q): Query<TracesQuery>,
) -> AppResult<Response> {
    unimplemented!("R1, trace: GET /api/traces")
}

#[derive(Debug, Deserialize)]
pub struct RollupQuery {
    pub bucket: RollupBucket,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
}

/// `GET routes::TRACES_ROLLUP`.
pub async fn rollup(
    State(_state): State<AppState>,
    Query(_q): Query<RollupQuery>,
) -> AppResult<Json<Vec<TraceRollup>>> {
    unimplemented!("R1, trace: GET /api/traces/rollup")
}
