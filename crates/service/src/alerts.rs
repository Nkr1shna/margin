//! Alerts from the traces (`docs/phase5-brief.md` D6): every `ALERT_TICK_SECS` the
//! alert job reads the last `[alerts] window_minutes` of trace rows per kind, computes
//! the error rate, p50/p95 durations, the rate-limited share, the ingest backlog,
//! memory and the HTTP 5xx count, and compares them with `Config.alerts`. A breach
//! raises an `Alert` (kept here, written as a `TraceKind::Alert` row, logged at
//! `warn` to journald, posted to the optional webhook); it clears after
//! `clear_after_minutes` without one.
//!
//! An alert row (`kind: alert`) carries the alert's name in `error_code`, `ok: false`
//! when it rose and `true` when it cleared, the value in `duration_ms`, and the trace
//! kind it is about (when it is about one) in `tool`, as a job step's name is.

use std::collections::VecDeque;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
use margin_shared::context::{Alert, AlertEvent, AlertList, TraceKind, TraceRow};
use margin_shared::error_code;
use margin_shared::routes::ALERT_TICK_SECS;
use serde_json::{Value, json};

use crate::config::AlertConfig;
use crate::db::Store;
use crate::error::AppResult;
use crate::state::AppState;
use crate::trace::{self, Tracer};
use margin_shared::sync::Locked;

/// The alerts kept in `App.alerts`: those active now and the last rises and clears.
#[derive(Debug, Clone, Default)]
pub struct Alerts {
    pub active: Vec<Alert>,
    /// Newest last; at most `RECENT_MAX`.
    pub recent: Vec<AlertEvent>,
    /// `App.http_errors` as each tick read it, oldest first, back to the first sample
    /// at or before the window's start: the 5xx count of a window is the difference.
    http_samples: VecDeque<(DateTime<Utc>, u64)>,
}

/// Rises and clears `GET /api/alerts` lists.
pub const RECENT_MAX: usize = 50;

/// How long a webhook post may take; it is never retried (a failure is a log line).
pub const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(10);

/// `trace_id` of the alert rows.
pub const ALERT_TRACE_ID: &str = "alerts";

/// The alert names.
pub const ERROR_RATE: &str = "error_rate";
pub const P95_MS: &str = "p95_ms";
pub const RATE_LIMITED_SHARE: &str = "rate_limited_share";
pub const INGEST_BACKLOG: &str = "ingest_backlog";
pub const RSS_BYTES: &str = "rss_bytes";
pub const HTTP_5XX: &str = "http_5xx";

/// The kinds whose error rate is watched (alert rows are the job's own).
pub const WATCHED_KINDS: [TraceKind; 6] = [
    TraceKind::ModelCall,
    TraceKind::Retrieval,
    TraceKind::ToolCall,
    TraceKind::Ingest,
    TraceKind::JobStep,
    TraceKind::Turn,
];

/// A failed row's code that isn't a failure: the owner stopped the turn.
const CANCELLED: &str = "cancelled";

impl Alerts {
    /// What `GET /api/version` shows: how many are active and since when the oldest.
    pub fn summary(&self) -> (u32, Option<DateTime<Utc>>) {
        (
            u32::try_from(self.active.len()).unwrap_or(u32::MAX),
            self.active.iter().map(|a| a.since).min(),
        )
    }

    /// Records the 5xx counter as read at `now` and returns how many 5xx answers fell
    /// in the window ending at `now` (all since start when no sample is that old).
    pub fn http_5xx_in_window(
        &mut self,
        now: DateTime<Utc>,
        window: chrono::Duration,
        counter: u64,
    ) -> u64 {
        let start = now - window;
        self.http_samples.push_back((now, counter));
        // Keep one sample at or before the window's start as the baseline.
        while self.http_samples.len() > 1 && self.http_samples[1].0 <= start {
            self.http_samples.pop_front();
        }
        let baseline = match self.http_samples.front() {
            Some((t, n)) if *t <= start => *n,
            _ => 0,
        };
        counter.saturating_sub(baseline)
    }

    /// Applies one tick's breaches at `now`: a new breach rises; a breach already
    /// active is seen again (value and `last_seen` updated); an active alert not seen
    /// for `clear_after` clears. Returns the rises and clears, in that order.
    pub fn apply(
        &mut self,
        breaches: Vec<Breach>,
        now: DateTime<Utc>,
        clear_after: chrono::Duration,
    ) -> Vec<AlertEvent> {
        let mut events = Vec::new();
        for b in &breaches {
            match self
                .active
                .iter_mut()
                .find(|a| a.name == b.name && a.kind == b.kind)
            {
                Some(a) => {
                    a.value = b.value;
                    a.threshold = b.threshold;
                    a.last_seen = now;
                }
                None => {
                    let alert = Alert {
                        name: b.name.to_string(),
                        kind: b.kind,
                        value: b.value,
                        threshold: b.threshold,
                        since: now,
                        last_seen: now,
                    };
                    self.active.push(alert.clone());
                    events.push(AlertEvent {
                        alert,
                        raised: true,
                        at: now,
                    });
                }
            }
        }
        let mut kept = Vec::with_capacity(self.active.len());
        for a in self.active.drain(..) {
            let breached = breaches
                .iter()
                .any(|b| b.name == a.name && b.kind == a.kind);
            if !breached && now - a.last_seen >= clear_after {
                events.push(AlertEvent {
                    alert: a,
                    raised: false,
                    at: now,
                });
            } else {
                kept.push(a);
            }
        }
        self.active = kept;
        self.recent.extend(events.iter().cloned());
        let over = self.recent.len().saturating_sub(RECENT_MAX);
        self.recent.drain(..over);
        events
    }
}

/// A threshold crossed in one tick.
#[derive(Debug, Clone, PartialEq)]
pub struct Breach {
    pub name: &'static str,
    pub kind: Option<TraceKind>,
    pub value: f64,
    pub threshold: f64,
}

/// One kind's numbers over the window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KindStats {
    pub calls: usize,
    /// Rows with `ok: false`, the owner's stops (`cancelled`) not counted.
    pub errors: usize,
    /// Rows that ended `rate_limited`.
    pub rate_limited: usize,
    pub p50_ms: f64,
    pub p95_ms: f64,
}

impl KindStats {
    pub fn from_rows(rows: &[TraceRow]) -> Self {
        let mut ms: Vec<f64> = rows.iter().map(|r| r.duration_ms).collect();
        Self {
            calls: rows.len(),
            errors: rows
                .iter()
                .filter(|r| !r.ok && r.error_code.as_deref() != Some(CANCELLED))
                .count(),
            rate_limited: rows
                .iter()
                .filter(|r| r.error_code.as_deref() == Some(error_code::RATE_LIMITED))
                .count(),
            p50_ms: percentile(&mut ms, 50.0),
            p95_ms: percentile(&mut ms, 95.0),
        }
    }

    pub fn error_rate(&self) -> f64 {
        if self.calls == 0 {
            0.0
        } else {
            self.errors as f64 / self.calls as f64
        }
    }
}

/// The nearest-rank percentile of `values` (sorted in place); 0 when empty.
pub fn percentile(values: &mut [f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    let rank = ((p / 100.0) * values.len() as f64).ceil() as usize;
    values[rank.clamp(1, values.len()) - 1]
}

/// What one tick measured.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Measures {
    pub kinds: Vec<(TraceKind, KindStats)>,
    /// Documents waiting to be read or being read.
    pub ingest_backlog: Option<u64>,
    pub rss_bytes: Option<u64>,
    pub http_5xx: u64,
}

/// The trace rows of the window ending at `now`, per watched kind.
pub async fn measure_traces(
    store: &Store,
    cfg: &AlertConfig,
    now: DateTime<Utc>,
) -> anyhow::Result<Vec<(TraceKind, KindStats)>> {
    let from = now - window(cfg);
    let mut out = Vec::with_capacity(WATCHED_KINDS.len());
    for kind in WATCHED_KINDS {
        let rows = trace::read_rows(store, Some(kind), from, now, None).await?;
        out.push((kind, KindStats::from_rows(&rows)));
    }
    Ok(out)
}

/// Documents whose status is `indexing` (waiting in the queue or being read).
pub async fn ingest_backlog(store: &Store) -> anyhow::Result<u64> {
    let rows: Vec<Value> = store
        .query_rows(
            "SELECT count() AS n FROM document WHERE doc.status.state = 'indexing' GROUP ALL;",
            vec![],
        )
        .await?;
    Ok(rows
        .first()
        .and_then(|r| r.get("n"))
        .and_then(Value::as_u64)
        .unwrap_or(0))
}

fn window(cfg: &AlertConfig) -> chrono::Duration {
    chrono::Duration::minutes(i64::from(cfg.window_minutes.max(1)))
}

/// The thresholds `m` crosses. Per-kind alerts need `min_calls` rows in the window, so
/// an idle service raises nothing.
pub fn evaluate(cfg: &AlertConfig, rss_max: Option<u64>, m: &Measures) -> Vec<Breach> {
    let mut out = Vec::new();
    let min_calls = cfg.min_calls as usize;
    for (kind, s) in &m.kinds {
        if s.calls < min_calls.max(1) {
            continue;
        }
        let rate = s.error_rate();
        if rate > cfg.error_rate_max {
            out.push(Breach {
                name: ERROR_RATE,
                kind: Some(*kind),
                value: rate,
                threshold: cfg.error_rate_max,
            });
        }
        let p95_max = match kind {
            TraceKind::ModelCall => Some(cfg.p95_ms.model_call),
            TraceKind::Retrieval => Some(cfg.p95_ms.retrieval),
            TraceKind::ToolCall => Some(cfg.p95_ms.tool_call),
            TraceKind::Ingest => Some(cfg.p95_ms.ingest),
            _ => None,
        };
        if let Some(max) = p95_max
            && s.p95_ms > max
        {
            out.push(Breach {
                name: P95_MS,
                kind: Some(*kind),
                value: s.p95_ms,
                threshold: max,
            });
        }
        if *kind == TraceKind::ModelCall {
            let share = s.rate_limited as f64 / s.calls as f64;
            if share > cfg.rate_limited_share_max {
                out.push(Breach {
                    name: RATE_LIMITED_SHARE,
                    kind: Some(*kind),
                    value: share,
                    threshold: cfg.rate_limited_share_max,
                });
            }
        }
    }
    if let Some(n) = m.ingest_backlog
        && n > u64::from(cfg.ingest_backlog_max)
    {
        out.push(Breach {
            name: INGEST_BACKLOG,
            kind: None,
            value: n as f64,
            threshold: f64::from(cfg.ingest_backlog_max),
        });
    }
    if let (Some(rss), Some(max)) = (m.rss_bytes, rss_max)
        && rss > max
    {
        out.push(Breach {
            name: RSS_BYTES,
            kind: None,
            value: rss as f64,
            threshold: max as f64,
        });
    }
    if m.http_5xx > u64::from(cfg.http_5xx_per_window_max) {
        out.push(Breach {
            name: HTTP_5XX,
            kind: None,
            value: m.http_5xx as f64,
            threshold: f64::from(cfg.http_5xx_per_window_max),
        });
    }
    out
}

/// The memory alert's threshold: `[alerts] rss_bytes_max`, else the systemd unit's
/// `MemoryMax` (this process's cgroup `memory.max`) less 10 %, else none.
pub fn rss_max(cfg: &AlertConfig) -> Option<u64> {
    cfg.rss_bytes_max
        .or_else(|| memory_max().map(|m| m - m / 10))
}

/// This process's cgroup v2 `memory.max`, when it has a limit (Linux).
#[cfg(target_os = "linux")]
pub fn memory_max() -> Option<u64> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    memory_max_from(&cgroup, |path| std::fs::read_to_string(path).ok())
}

#[cfg(not(target_os = "linux"))]
pub fn memory_max() -> Option<u64> {
    None
}

/// `memory.max` of the cgroup `/proc/self/cgroup` names (its `0::<path>` line), read
/// through `read`; `None` for `max` (no limit) or anything unreadable.
pub fn memory_max_from(cgroup: &str, read: impl Fn(&str) -> Option<String>) -> Option<u64> {
    let path = cgroup
        .lines()
        .find_map(|l| l.strip_prefix("0::"))?
        .trim()
        .trim_end_matches('/');
    let text = read(&format!("/sys/fs/cgroup{path}/memory.max"))?;
    text.trim().parse().ok()
}

/// One tick at `now`: measures, compares, raises and clears, and publishes each rise
/// and clear (journald, a trace row, the webhook). Returns the events.
pub async fn tick(state: &AppState, now: DateTime<Utc>) -> anyhow::Result<Vec<AlertEvent>> {
    let cfg = &state.config.alerts;
    // Rows recorded before the tick count.
    state.tracer.flush(Duration::from_secs(2)).await;
    let kinds = measure_traces(&state.store, cfg, now).await?;
    let backlog = match ingest_backlog(&state.store).await {
        Ok(n) => Some(n),
        Err(e) => {
            tracing::warn!("alerts: couldn't count the ingest backlog: {e:#}");
            None
        }
    };
    let counter = state.http_errors.load(std::sync::atomic::Ordering::Relaxed);
    let events = {
        let mut alerts = state.alerts.locked();
        let http_5xx = alerts.http_5xx_in_window(now, window(cfg), counter);
        let m = Measures {
            kinds,
            ingest_backlog: backlog,
            rss_bytes: crate::version::resident_bytes(),
            http_5xx,
        };
        let breaches = evaluate(cfg, rss_max(cfg), &m);
        let clear_after = chrono::Duration::minutes(i64::from(cfg.clear_after_minutes));
        alerts.apply(breaches, now, clear_after)
    };
    publish(
        &events,
        &state.tracer,
        &state.http,
        cfg.webhook_url.as_deref(),
        &state.config.environment,
    );
    Ok(events)
}

/// Logs each event at `warn` (journald), records its trace row and posts it to the
/// webhook in the background, so a slow webhook never holds the tick.
pub fn publish(
    events: &[AlertEvent],
    tracer: &Tracer,
    http: &reqwest::Client,
    webhook: Option<&str>,
    environment: &str,
) {
    for e in events {
        let a = &e.alert;
        let kind = a.kind.map(|k| k.as_str()).unwrap_or("-");
        if e.raised {
            tracing::warn!(
                name = %a.name, kind, value = a.value, threshold = a.threshold,
                "alert raised name={} kind={kind} value={} threshold={}",
                a.name, a.value, a.threshold
            );
        } else {
            tracing::warn!(
                name = %a.name, kind, value = a.value, threshold = a.threshold,
                "alert cleared name={} kind={kind} value={} threshold={}",
                a.name, a.value, a.threshold
            );
        }
        tracer.record(alert_row(e));
        if let Some(url) = webhook.filter(|u| !u.trim().is_empty()) {
            let req = http
                .post(url.trim())
                .timeout(WEBHOOK_TIMEOUT)
                .json(&webhook_body(e, environment));
            let name = a.name.clone();
            tokio::spawn(async move {
                match req.send().await {
                    Ok(res) if res.status().is_success() => {}
                    Ok(res) => tracing::warn!(
                        "alert webhook answered {} for {name}",
                        res.status().as_u16()
                    ),
                    Err(e) => tracing::warn!("alert webhook failed for {name}: {e}"),
                }
            });
        }
    }
}

/// The trace row of a rise or clear (see the module docs).
pub fn alert_row(e: &AlertEvent) -> TraceRow {
    let mut row = TraceRow::new(TraceKind::Alert, trace::new_row_id(), e.at);
    row.trace_id = ALERT_TRACE_ID.into();
    row.ok = !e.raised;
    row.error_code = Some(e.alert.name.clone());
    row.duration_ms = e.alert.value;
    row.tool = e.alert.kind.map(|k| k.as_str().to_string());
    row
}

/// What the webhook receives: the `AlertEvent` (so it parses as one), the deploy it
/// came from, and a `text` line for chat-style receivers (Slack, ntfy).
pub fn webhook_body(e: &AlertEvent, environment: &str) -> Value {
    let a = &e.alert;
    let what = if e.raised { "raised" } else { "cleared" };
    let kind = a
        .kind
        .map(|k| format!(" ({})", k.as_str()))
        .unwrap_or_default();
    json!({
        "alert": a,
        "raised": e.raised,
        "at": e.at,
        "environment": environment,
        "text": format!(
            "Margin {environment}: alert {what}: {}{kind} = {} (threshold {})",
            a.name,
            show(a.value),
            show(a.threshold)
        ),
    })
}

fn show(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{v:.0}")
    } else {
        format!("{v:.3}")
    }
}

/// The alert job: ticks every `ALERT_TICK_SECS` until shutdown. Started by
/// `trace::start`.
pub async fn run(state: AppState) {
    let period = Duration::from_secs(ALERT_TICK_SECS);
    let mut tick_at = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    tick_at.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut shutdown = state.shutdown.clone();
    {
        // The 5xx baseline: answers before the first tick count from here.
        let counter = state.http_errors.load(std::sync::atomic::Ordering::Relaxed);
        let window = window(&state.config.alerts);
        state
            .alerts
            .locked()
            .http_5xx_in_window(Utc::now(), window, counter);
    }
    loop {
        tokio::select! {
            _ = tick_at.tick() => {}
            _ = shutdown.changed() => return,
        }
        if let Err(e) = tick(&state, Utc::now()).await {
            tracing::warn!("the alert job's tick failed: {e:#}");
        }
    }
}

/// The alerts active now.
pub fn active(state: &AppState) -> Vec<Alert> {
    state.alerts.locked().active.clone()
}

/// `GET routes::ALERTS`: the active alerts and the last rises and clears, newest first.
/// Admins only, like the traces they come from (L1).
pub async fn get_alerts(
    State(state): State<AppState>,
    _admin: crate::principal::RequireAdmin,
) -> AppResult<Json<AlertList>> {
    let alerts = state.alerts.locked();
    Ok(Json(AlertList {
        active: alerts.active.clone(),
        recent: alerts.recent.iter().rev().cloned().collect(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_760_000_000 + s, 0).unwrap()
    }

    fn minutes(m: i64) -> chrono::Duration {
        chrono::Duration::minutes(m)
    }

    #[test]
    fn summary_counts_and_dates_the_oldest() {
        assert_eq!(Alerts::default().summary(), (0, None));
        let alert = |since| Alert {
            name: "error_rate".into(),
            kind: None,
            value: 1.0,
            threshold: 0.2,
            since,
            last_seen: at(100),
        };
        let a = Alerts {
            active: vec![alert(at(30)), alert(at(10))],
            ..Default::default()
        };
        assert_eq!(a.summary(), (2, Some(at(10))));
    }

    #[test]
    fn percentiles_by_nearest_rank() {
        let mut v: Vec<f64> = (1..=20).map(|i| i as f64 * 10.0).collect();
        assert_eq!(percentile(&mut v, 95.0), 190.0);
        assert_eq!(percentile(&mut v, 50.0), 100.0);
        assert_eq!(percentile(&mut [], 95.0), 0.0);
        assert_eq!(percentile(&mut [7.0], 95.0), 7.0);
    }

    fn rows(kind: TraceKind, n: usize, failed: usize, code: &str, ms: f64) -> Vec<TraceRow> {
        (0..n)
            .map(|i| {
                let mut r = TraceRow::new(kind, trace::new_row_id(), at(i as i64));
                r.duration_ms = ms;
                if i < failed {
                    r.ok = false;
                    r.error_code = Some(code.into());
                }
                r
            })
            .collect()
    }

    #[test]
    fn stats_leave_stops_out_of_errors() {
        let mut r = rows(TraceKind::ModelCall, 10, 4, "rate_limited", 5.0);
        r[9].ok = false;
        r[9].error_code = Some("cancelled".into());
        let s = KindStats::from_rows(&r);
        assert_eq!((s.calls, s.errors, s.rate_limited), (10, 4, 4));
        assert!((s.error_rate() - 0.4).abs() < 1e-12);
    }

    fn measures(kinds: Vec<(TraceKind, KindStats)>) -> Measures {
        Measures {
            kinds,
            ..Default::default()
        }
    }

    #[test]
    fn thresholds_and_an_idle_service() {
        let cfg = AlertConfig::default();
        // Four failed calls: under min_calls, nothing.
        let idle = measures(vec![(
            TraceKind::ModelCall,
            KindStats::from_rows(&rows(TraceKind::ModelCall, 4, 4, "server_error", 1.0)),
        )]);
        assert!(evaluate(&cfg, None, &idle).is_empty());

        let calls = KindStats::from_rows(&rows(TraceKind::ModelCall, 20, 20, "rate_limited", 1.0));
        let slow = KindStats::from_rows(&rows(TraceKind::Retrieval, 10, 0, "", 900.0));
        let m = Measures {
            kinds: vec![(TraceKind::ModelCall, calls), (TraceKind::Retrieval, slow)],
            ingest_backlog: Some(51),
            rss_bytes: Some(2_000),
            http_5xx: 6,
        };
        let got = evaluate(&cfg, Some(1_000), &m);
        let names: Vec<(&str, Option<TraceKind>)> = got.iter().map(|b| (b.name, b.kind)).collect();
        assert_eq!(
            names,
            [
                (ERROR_RATE, Some(TraceKind::ModelCall)),
                (RATE_LIMITED_SHARE, Some(TraceKind::ModelCall)),
                (P95_MS, Some(TraceKind::Retrieval)),
                (INGEST_BACKLOG, None),
                (RSS_BYTES, None),
                (HTTP_5XX, None),
            ]
        );
        assert_eq!(got[0].value, 1.0);
        assert_eq!(got[0].threshold, 0.2);
        assert_eq!(got[2].value, 900.0);
        assert_eq!(got[2].threshold, 400.0);
        // No memory limit known: no memory alert.
        assert!(!evaluate(&cfg, None, &m).iter().any(|b| b.name == RSS_BYTES));
        // At the threshold is not over it.
        let edge = Measures {
            ingest_backlog: Some(50),
            http_5xx: 5,
            ..Default::default()
        };
        assert!(evaluate(&cfg, None, &edge).is_empty());
    }

    fn breach(name: &'static str, value: f64) -> Breach {
        Breach {
            name,
            kind: Some(TraceKind::ModelCall),
            value,
            threshold: 0.2,
        }
    }

    #[test]
    fn rise_stay_and_clear_after_quiet_minutes() {
        let mut a = Alerts::default();
        let clear = minutes(30);
        let ev = a.apply(vec![breach(ERROR_RATE, 0.5)], at(0), clear);
        assert_eq!(ev.len(), 1);
        assert!(ev[0].raised);
        assert_eq!(a.summary(), (1, Some(at(0))));
        // Seen again: no event, value and last_seen move.
        let ev = a.apply(vec![breach(ERROR_RATE, 0.7)], at(60), clear);
        assert!(ev.is_empty());
        assert_eq!(a.active[0].value, 0.7);
        assert_eq!((a.active[0].since, a.active[0].last_seen), (at(0), at(60)));
        // Quiet, but not for 30 minutes yet.
        assert!(a.apply(vec![], at(60 + 29 * 60), clear).is_empty());
        assert_eq!(a.active.len(), 1);
        // 30 minutes after it was last seen it clears.
        let ev = a.apply(vec![], at(60 + 30 * 60), clear);
        assert_eq!(ev.len(), 1);
        assert!(!ev[0].raised);
        assert_eq!(ev[0].alert.value, 0.7);
        assert!(a.active.is_empty());
        assert_eq!(a.recent.len(), 2);
        // The same name on another kind is another alert.
        let other = Breach {
            kind: Some(TraceKind::Retrieval),
            ..breach(ERROR_RATE, 0.3)
        };
        let ev = a.apply(vec![breach(ERROR_RATE, 0.5), other], at(9_000), clear);
        assert_eq!(ev.len(), 2);
        // The history keeps the last RECENT_MAX.
        for i in 0..RECENT_MAX as i64 {
            a.apply(vec![], at(20_000 + i * 4_000), clear);
            a.apply(
                vec![breach(HTTP_5XX, 9.0)],
                at(20_000 + i * 4_000 + 1),
                chrono::Duration::zero(),
            );
        }
        assert_eq!(a.recent.len(), RECENT_MAX);
    }

    #[test]
    fn http_5xx_counts_the_window() {
        let mut a = Alerts::default();
        let w = minutes(15);
        // No sample old enough: everything since start.
        assert_eq!(a.http_5xx_in_window(at(0), w, 3), 3);
        assert_eq!(a.http_5xx_in_window(at(60), w, 5), 5);
        // 16 minutes on: the sample at 60 s is the baseline.
        assert_eq!(a.http_5xx_in_window(at(60 + 16 * 60), w, 9), 4);
        assert_eq!(a.http_5xx_in_window(at(60 + 40 * 60), w, 9), 0);
        assert!(a.http_samples.len() <= 3);
    }

    #[test]
    fn memory_max_from_the_cgroup() {
        let cg = "0::/system.slice/margin-service.service\n";
        let read = |limit: &'static str| {
            move |p: &str| {
                assert_eq!(
                    p,
                    "/sys/fs/cgroup/system.slice/margin-service.service/memory.max"
                );
                Some(limit.to_string())
            }
        };
        assert_eq!(
            memory_max_from(cg, read("3221225472\n")),
            Some(3_221_225_472)
        );
        assert_eq!(memory_max_from(cg, read("max\n")), None);
        assert_eq!(memory_max_from("", read("1")), None);
        let cfg = AlertConfig {
            rss_bytes_max: Some(7),
            ..Default::default()
        };
        assert_eq!(rss_max(&cfg), Some(7));
    }

    #[tokio::test]
    async fn measures_read_the_window_and_the_backlog() {
        let s = Store::memory().await.unwrap();
        let now = at(3_600);
        let mut old = rows(TraceKind::ModelCall, 3, 3, "server_error", 1.0);
        for r in &mut old {
            r.at = now - minutes(20);
        }
        let mut recent = rows(TraceKind::ModelCall, 6, 2, "server_error", 10.0);
        for r in &mut recent {
            r.at = now - minutes(5);
        }
        trace::write_rows(&s, &old).await.unwrap();
        trace::write_rows(&s, &recent).await.unwrap();
        let kinds = measure_traces(&s, &AlertConfig::default(), now)
            .await
            .unwrap();
        let calls = &kinds
            .iter()
            .find(|(k, _)| *k == TraceKind::ModelCall)
            .unwrap()
            .1;
        assert_eq!((calls.calls, calls.errors), (6, 2));
        assert_eq!(calls.p95_ms, 10.0);
        assert!(
            kinds
                .iter()
                .all(|(k, s)| *k == TraceKind::ModelCall || s.calls == 0)
        );

        assert_eq!(ingest_backlog(&s).await.unwrap(), 0);
        for (id, state) in [("d1", "indexing"), ("d2", "indexing"), ("d3", "ready")] {
            s.query_rows::<Value>(
                "CREATE type::record('document', $id) CONTENT { doc: { status: { state: $state } } } RETURN NONE;",
                vec![("id", json!(id)), ("state", json!(state))],
            )
            .await
            .unwrap();
        }
        assert_eq!(ingest_backlog(&s).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn publish_records_rows_and_posts_the_webhook() {
        use std::sync::{Arc, Mutex};
        let got: Arc<Mutex<Vec<Value>>> = Arc::default();
        let sink = got.clone();
        let app = axum::Router::new().route(
            "/hook",
            axum::routing::post(move |Json(v): Json<Value>| {
                let sink = sink.clone();
                async move {
                    sink.lock().unwrap().push(v);
                    "ok"
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let tracer = Tracer::new();
        let mut rx = tracer.take_receiver().unwrap();
        let mut a = Alerts::default();
        let mut events = a.apply(vec![breach(ERROR_RATE, 0.5)], at(0), minutes(30));
        events.extend(a.apply(vec![], at(31 * 60), minutes(30)));
        publish(
            &events,
            &tracer,
            &reqwest::Client::new(),
            Some(&format!("http://{addr}/hook")),
            "dev",
        );
        let rise = rx.recv().await.unwrap();
        assert_eq!(rise.kind, TraceKind::Alert);
        assert_eq!(
            (rise.ok, rise.error_code.as_deref(), rise.duration_ms),
            (false, Some(ERROR_RATE), 0.5)
        );
        assert_eq!(rise.tool.as_deref(), Some("model_call"));
        assert_eq!(rise.trace_id, ALERT_TRACE_ID);
        let clear = rx.recv().await.unwrap();
        assert!(clear.ok);
        for _ in 0..200 {
            if got.lock().unwrap().len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let posts = got.lock().unwrap().clone();
        assert_eq!(posts.len(), 2);
        let mut parsed: Vec<AlertEvent> = posts
            .iter()
            .map(|p| serde_json::from_value(p.clone()).unwrap())
            .collect();
        parsed.sort_by_key(|e| e.at);
        assert_eq!(parsed, events);
        let texts: Vec<&str> = posts.iter().map(|p| p["text"].as_str().unwrap()).collect();
        assert!(
            texts.contains(
                &"Margin dev: alert raised: error_rate (model_call) = 0.500 (threshold 0.200)"
            ),
            "{texts:?}"
        );
        // A webhook that isn't there is a log line, not an error.
        publish(
            &events,
            &tracer,
            &reqwest::Client::new(),
            Some("http://127.0.0.1:9/nothing"),
            "dev",
        );
    }
}
