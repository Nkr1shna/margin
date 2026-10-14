//! Reading the service's trace rows from outside the app (§2.11, §3.2 "Efficiency"):
//! over `GET /api/traces` from a running service with a device token, or from an
//! exported JSONL file, and the efficiency numbers a report shows (tokens, cache-hit
//! share and cost by route and model; retrieval latency; ingest throughput), so the
//! numbers the service recorded are the ones the report shows.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;
use chrono::{DateTime, Utc};
use margin_shared::context::{TraceKind, TraceRow};
use margin_shared::routes::TRACES;
use serde::{Deserialize, Serialize};

use crate::Prices;
use crate::metrics::percentile;

/// Which rows to fetch.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TraceFilter {
    pub kind: Option<TraceKind>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub trace_id: Option<String>,
}

impl TraceFilter {
    fn query(&self) -> Vec<(&'static str, String)> {
        let mut q = Vec::new();
        if let Some(k) = self.kind {
            q.push(("kind", k.as_str().to_string()));
        }
        if let Some(f) = self.from {
            q.push(("from", f.to_rfc3339()));
        }
        if let Some(t) = self.to {
            q.push(("to", t.to_rfc3339()));
        }
        if let Some(t) = &self.trace_id {
            q.push(("trace_id", t.clone()));
        }
        q
    }

    fn keeps(&self, r: &TraceRow) -> bool {
        self.kind.is_none_or(|k| r.kind == k)
            && self.from.is_none_or(|f| r.at >= f)
            && self.to.is_none_or(|t| r.at <= t)
            && self.trace_id.as_deref().is_none_or(|t| r.trace_id == t)
    }
}

/// Parses JSON lines of trace rows.
pub fn parse_jsonl(text: &str) -> anyhow::Result<Vec<TraceRow>> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| serde_json::from_str(l).with_context(|| format!("trace line {}", i + 1)))
        .collect()
}

/// Rows from an exported JSONL file, filtered.
pub fn read_file(path: &Path, filter: &TraceFilter) -> anyhow::Result<Vec<TraceRow>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("couldn't read {}", path.display()))?;
    Ok(parse_jsonl(&text)?
        .into_iter()
        .filter(|r| filter.keeps(r))
        .collect())
}

/// Rows from a running service: `GET {base}/api/traces` with the device token.
pub async fn fetch(
    http: &reqwest::Client,
    base_url: &str,
    token: &str,
    filter: &TraceFilter,
) -> anyhow::Result<Vec<TraceRow>> {
    let url = format!("{}{TRACES}", base_url.trim_end_matches('/'));
    let resp = http
        .get(&url)
        .bearer_auth(token)
        .query(&filter.query())
        .send()
        .await
        .with_context(|| format!("couldn't reach {url}"))?;
    let status = resp.status();
    let body = resp.text().await?;
    anyhow::ensure!(
        status.is_success(),
        "{url} answered {status}: {}",
        body.chars().take(200).collect::<String>()
    );
    parse_jsonl(&body)
}

/// Writes rows as JSON lines (`margin-eval export`).
pub fn write_jsonl(path: &Path, rows: &[TraceRow]) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut out = String::new();
    for r in rows {
        out.push_str(&serde_json::to_string(r)?);
        out.push('\n');
    }
    std::fs::write(path, out).with_context(|| format!("couldn't write {}", path.display()))
}

/// Model-call numbers for one route and model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CallStats {
    pub calls: u64,
    pub input: u64,
    pub cached: u64,
    pub output: u64,
    pub cost_usd: f64,
    /// Calls with no price (model missing from `prices.toml` and no `cost_usd`).
    pub unpriced: u64,
    pub mean_ms: f64,
}

impl CallStats {
    /// Points: the share of input tokens served from the prompt cache.
    pub fn cache_share(&self) -> f64 {
        if self.input == 0 {
            0.0
        } else {
            self.cached as f64 * 100.0 / self.input as f64
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Efficiency {
    pub rows: usize,
    pub traces: usize,
    /// By "route / model".
    pub calls: BTreeMap<String, CallStats>,
    pub total: CallStats,
    pub retrievals: usize,
    pub retrieval_p50_ms: f64,
    pub retrieval_p95_ms: f64,
    pub retrieval_cache_hits: usize,
    pub ingests: usize,
    pub chunks_new: u64,
    pub chunks_kept: u64,
    /// Embedded chunks per second of embedding time.
    pub embed_chunks_per_s: f64,
    pub tool_calls: usize,
    pub tool_output_max_bytes: u64,
}

/// USD of a call's usage at `prices`; `None` when the model isn't priced.
pub fn price(prices: &Prices, model: &str, u: &margin_shared::context::TraceUsage) -> Option<f64> {
    let p = prices.models.get(model)?;
    let uncached = u.input.saturating_sub(u.cached) as f64;
    Some(
        (uncached * p.input
            + u.cached as f64 * p.cached_input
            + u.cache_write as f64 * p.cache_write
            + u.output as f64 * p.output)
            / 1e6,
    )
}

/// The efficiency numbers of a set of trace rows. A row's own `cost_usd` (priced when
/// it was written) wins over today's prices.
pub fn efficiency(rows: &[TraceRow], prices: &Prices) -> Efficiency {
    let mut e = Efficiency {
        rows: rows.len(),
        traces: rows
            .iter()
            .map(|r| r.trace_id.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len(),
        ..Default::default()
    };
    let mut ms: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut retrieval_ms = Vec::new();
    let mut embed_ms = 0.0;
    for r in rows {
        match r.kind {
            TraceKind::ModelCall => {
                let model = r.model.clone().unwrap_or_else(|| "?".into());
                let key = format!("{} / {model}", r.route.map(|x| x.as_str()).unwrap_or("?"));
                let u = r.usage.unwrap_or_default();
                let cost = r.cost_usd.or_else(|| price(prices, &model, &u));
                for s in [e.calls.entry(key.clone()).or_default(), &mut e.total] {
                    s.calls += 1;
                    s.input += u.input;
                    s.cached += u.cached;
                    s.output += u.output;
                    match cost {
                        Some(c) => s.cost_usd += c,
                        None => s.unpriced += 1,
                    }
                }
                ms.entry(key).or_default().push(r.duration_ms);
                ms.entry(String::new()).or_default().push(r.duration_ms);
            }
            TraceKind::Retrieval => {
                e.retrievals += 1;
                retrieval_ms.push(r.duration_ms);
                if r.cache_hit == Some(true) {
                    e.retrieval_cache_hits += 1;
                }
            }
            TraceKind::Ingest => {
                e.ingests += 1;
                e.chunks_new += r.chunks_new.unwrap_or(0) as u64;
                e.chunks_kept += r.chunks_kept.unwrap_or(0) as u64;
                embed_ms += r.embed_ms.unwrap_or(0.0);
            }
            TraceKind::ToolCall => {
                e.tool_calls += 1;
                e.tool_output_max_bytes = e.tool_output_max_bytes.max(r.output_bytes.unwrap_or(0));
            }
            TraceKind::JobStep | TraceKind::Turn => {}
        }
    }
    for (k, v) in &ms {
        let m = crate::metrics::mean(v);
        if k.is_empty() {
            e.total.mean_ms = m;
        } else if let Some(s) = e.calls.get_mut(k) {
            s.mean_ms = m;
        }
    }
    e.retrieval_p50_ms = percentile(&retrieval_ms, 50.0);
    e.retrieval_p95_ms = percentile(&retrieval_ms, 95.0);
    e.embed_chunks_per_s = if embed_ms > 0.0 {
        e.chunks_new as f64 * 1000.0 / embed_ms
    } else {
        0.0
    };
    e
}

#[cfg(test)]
mod tests {
    use super::*;
    use margin_shared::context::TraceUsage;
    use margin_shared::pipeline::Route;

    fn row(kind: TraceKind, trace: &str, at: i64) -> TraceRow {
        let mut r: TraceRow = serde_json::from_value(serde_json::json!({
            "id": format!("u{at}"),
            "kind": kind.as_str(),
            "at": DateTime::from_timestamp(at, 0).unwrap(),
        }))
        .unwrap();
        r.trace_id = trace.into();
        r
    }

    fn prices() -> Prices {
        toml::from_str(
            &std::fs::read_to_string(crate::golden::evals_dir().join("prices.toml")).unwrap(),
        )
        .unwrap()
    }

    fn sample() -> Vec<TraceRow> {
        let mut a = row(TraceKind::ModelCall, "t1", 10);
        a.route = Some(Route::Write);
        a.model = Some("claude-sonnet-5-5".into());
        a.usage = Some(TraceUsage {
            input: 1_000_000,
            cached: 500_000,
            output: 100_000,
            ..Default::default()
        });
        a.duration_ms = 100.0;
        let mut b = a.clone();
        b.id = "u11".into();
        b.cost_usd = Some(9.0);
        b.duration_ms = 300.0;
        let mut c = row(TraceKind::ModelCall, "t2", 12);
        c.route = Some(Route::Small);
        c.model = Some("unknown-model".into());
        c.usage = Some(TraceUsage {
            input: 10,
            ..Default::default()
        });
        let mut r1 = row(TraceKind::Retrieval, "t1", 13);
        r1.duration_ms = 40.0;
        let mut r2 = row(TraceKind::Retrieval, "t2", 14);
        r2.duration_ms = 80.0;
        r2.cache_hit = Some(true);
        let mut i = row(TraceKind::Ingest, "t3", 15);
        i.chunks_new = Some(20);
        i.chunks_kept = Some(5);
        i.embed_ms = Some(2_000.0);
        let mut t = row(TraceKind::ToolCall, "t1", 16);
        t.output_bytes = Some(9_000);
        vec![a, b, c, r1, r2, i, t]
    }

    #[test]
    fn efficiency_by_hand() {
        let e = efficiency(&sample(), &prices());
        assert_eq!((e.rows, e.traces), (7, 3));
        let w = &e.calls["write / claude-sonnet-5-5"];
        // Priced: 0.5M uncached × $2 + 0.5M cached × $0.20 + 0.1M out × $10 = $2.10;
        // the second row's own cost_usd ($9) wins.
        assert!((w.cost_usd - 11.1).abs() < 1e-9, "{}", w.cost_usd);
        assert_eq!(w.calls, 2);
        assert_eq!(w.cache_share(), 50.0);
        assert_eq!(w.mean_ms, 200.0);
        assert_eq!(e.calls["small / unknown-model"].unpriced, 1);
        assert_eq!(e.total.unpriced, 1);
        assert_eq!(e.total.calls, 3);
        assert_eq!((e.retrievals, e.retrieval_cache_hits), (2, 1));
        assert_eq!(e.retrieval_p95_ms, 80.0);
        assert_eq!(e.embed_chunks_per_s, 10.0);
        assert_eq!(e.tool_output_max_bytes, 9_000);
    }

    #[test]
    fn jsonl_round_trip_and_filters() {
        let dir = std::env::temp_dir().join(format!("margin-evals-traces-{}", std::process::id()));
        let path = dir.join("t.jsonl");
        write_jsonl(&path, &sample()).unwrap();
        let all = read_file(&path, &TraceFilter::default()).unwrap();
        assert_eq!(all, sample());
        let f = TraceFilter {
            kind: Some(TraceKind::Retrieval),
            from: DateTime::from_timestamp(14, 0),
            ..Default::default()
        };
        let got = read_file(&path, &f).unwrap();
        assert_eq!(got.len(), 1);
        let f = TraceFilter {
            trace_id: Some("t1".into()),
            ..Default::default()
        };
        assert_eq!(read_file(&path, &f).unwrap().len(), 4);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn fetches_over_http_with_the_device_token() {
        use axum::extract::Query;
        use axum::http::HeaderMap;
        let body: String = sample()
            .iter()
            .map(|r| serde_json::to_string(r).unwrap() + "\n")
            .collect();
        let app = axum::Router::new().route(
            TRACES,
            axum::routing::get(
                move |h: HeaderMap, Query(q): Query<BTreeMap<String, String>>| {
                    let body = body.clone();
                    async move {
                        if h.get("authorization").and_then(|v| v.to_str().ok())
                            != Some("Bearer tok")
                        {
                            return (axum::http::StatusCode::UNAUTHORIZED, "no".to_string());
                        }
                        assert_eq!(q.get("kind").map(String::as_str), Some("model_call"));
                        (axum::http::StatusCode::OK, body)
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let base = format!("http://{addr}");
        let f = TraceFilter {
            kind: Some(TraceKind::ModelCall),
            ..Default::default()
        };
        let http = reqwest::Client::new();
        let rows = fetch(&http, &base, "tok", &f).await.unwrap();
        assert_eq!(
            rows.len(),
            7,
            "the service filters; the runner reads what it sends"
        );
        let err = fetch(&http, &base, "bad", &f)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("401"), "{err}");
    }
}
