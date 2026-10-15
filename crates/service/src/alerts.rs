//! Alerts from the traces (`docs/phase5-brief.md` D6): every `ALERT_TICK_SECS` the
//! alert job reads the last `[alerts] window_minutes` of trace rows per kind, computes
//! the error rate, p50/p95 durations, the rate-limited share, the ingest backlog,
//! memory and the HTTP 5xx count, and compares them with `Config.alerts`. A breach
//! raises an `Alert` (kept here, written as a `TraceKind::Alert` row, logged at
//! `warn` to journald, posted to the optional webhook); it clears after
//! `clear_after_minutes` without one.

use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
use margin_shared::context::{Alert, AlertEvent, AlertList};

use crate::error::AppResult;
use crate::state::AppState;

/// The alerts kept in `App.alerts`: those active now and the last rises and clears.
#[derive(Debug, Clone, Default)]
pub struct Alerts {
    pub active: Vec<Alert>,
    /// Newest last; at most `RECENT_MAX`.
    pub recent: Vec<AlertEvent>,
}

/// Rises and clears `GET /api/alerts` lists.
pub const RECENT_MAX: usize = 50;

impl Alerts {
    /// What `GET /api/version` shows: how many are active and since when the oldest.
    pub fn summary(&self) -> (u32, Option<DateTime<Utc>>) {
        (
            u32::try_from(self.active.len()).unwrap_or(u32::MAX),
            self.active.iter().map(|a| a.since).min(),
        )
    }
}

/// The alert job: ticks every `ALERT_TICK_SECS` until shutdown.
pub async fn run(_state: AppState) {
    unimplemented!("P5, 5-ops")
}

/// The alerts active now.
pub fn active(_state: &AppState) -> Vec<Alert> {
    unimplemented!("P5, 5-ops")
}

/// `GET routes::ALERTS`.
pub async fn get_alerts(State(_state): State<AppState>) -> AppResult<Json<AlertList>> {
    unimplemented!("P5, 5-ops")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_counts_and_dates_the_oldest() {
        assert_eq!(Alerts::default().summary(), (0, None));
        let at = |s: i64| DateTime::from_timestamp(1_760_000_000 + s, 0).unwrap();
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
            recent: Vec::new(),
        };
        assert_eq!(a.summary(), (2, Some(at(10))));
    }
}
