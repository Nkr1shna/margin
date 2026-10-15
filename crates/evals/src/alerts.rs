//! `margin-eval alerts --service-url …` (`docs/phase5-brief.md` D6): the service's
//! active alerts (`GET /api/alerts`) and the last day's rises and clears (its `alert`
//! trace rows).

/// Prints the active alerts and the last day's history.
pub async fn run(_http: &reqwest::Client, _url: &str, _token: &str) -> anyhow::Result<()> {
    unimplemented!("P5, 5-ops")
}
