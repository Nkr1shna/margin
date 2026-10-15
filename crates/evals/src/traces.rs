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

use crate::metrics::percentile;
use crate::{ModelPrice, Prices, Provider};

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

/// Model-call numbers for one route, provider and model (or a provider's, or all calls).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CallStats {
    /// "chat", "write", "small"; empty on a per-provider or total line.
    pub route: String,
    /// "openai", "chatgpt", "claude" (`margin_shared::Provider::as_str`), "?" when
    /// neither the row nor `prices.toml` says; empty on the total line.
    pub provider: String,
    /// Empty on a per-provider or total line.
    pub model: String,
    pub calls: u64,
    pub input: u64,
    pub cached: u64,
    /// Explicit prompt-cache writes (Claude's `cache_creation_input_tokens`).
    pub cache_write: u64,
    pub output: u64,
    pub web_searches: u64,
    pub cost_usd: f64,
    /// Calls with no price (model missing from `prices.toml` and no `cost_usd`).
    pub unpriced: u64,
    /// ChatGPT calls: plan-billed, never priced (D14); their tokens still count.
    pub plan_billed: u64,
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
    /// By "route / provider / model".
    pub calls: BTreeMap<String, CallStats>,
    /// By provider: the report's cost-by-provider table.
    #[serde(default)]
    pub providers: BTreeMap<String, CallStats>,
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
    /// Margin's own time per agent step, `duration_ms − provider_ms` over the model-call
    /// rows that carry `provider_ms` (`docs/phase5-brief.md` D8).
    #[serde(default)]
    pub step_overhead: Option<Overhead>,
    /// The alert rows' rises and clears, oldest first (`docs/phase5-brief.md` D6).
    #[serde(default)]
    pub alerts: Vec<AlertLine>,
}

/// Margin's share of the model calls that recorded the provider's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Overhead {
    pub steps: usize,
    pub mean_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
}

impl Overhead {
    /// `None` when no row carries `provider_ms`.
    pub fn of(rows: &[TraceRow]) -> Option<Self> {
        let ms: Vec<f64> = rows
            .iter()
            .filter(|r| r.kind == TraceKind::ModelCall)
            // Job calls (lessons, scenes, the planner: the Write route) carry the
            // provider's share too, but aren't agent steps.
            .filter(|r| r.route != Some(margin_shared::pipeline::Route::Write))
            .filter_map(|r| r.provider_ms.map(|p| (r.duration_ms - p).max(0.0)))
            .collect();
        (!ms.is_empty()).then(|| Overhead {
            steps: ms.len(),
            mean_ms: crate::metrics::mean(&ms),
            p95_ms: percentile(&ms, 95.0),
            max_ms: ms.iter().copied().fold(0.0, f64::max),
        })
    }
}

/// One alert row: a rise or a clear.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertLine {
    pub at: DateTime<Utc>,
    /// The alert's name (the row's `error_code`).
    pub name: String,
    /// The trace kind it is about (the row's `tool`), when it is about one.
    #[serde(default)]
    pub kind: Option<String>,
    pub raised: bool,
    /// The value that crossed the threshold (the row's `duration_ms`).
    pub value: f64,
}

impl AlertLine {
    /// The rise or clear an `alert` row records; `None` for any other row.
    pub fn of(r: &TraceRow) -> Option<Self> {
        (r.kind == TraceKind::Alert).then(|| AlertLine {
            at: r.at,
            name: r.error_code.clone().unwrap_or_else(|| "?".into()),
            kind: r.tool.clone(),
            raised: !r.ok,
            value: r.duration_ms,
        })
    }
}

/// The model's prices: its own row, else the longest row id it starts with followed by
/// `-` (a dated snapshot of a listed model, as a provider may answer with), the
/// service's `trace::Prices::get` rule. `None` for a model not listed.
pub fn model_price<'a>(prices: &'a Prices, model: &str) -> Option<&'a ModelPrice> {
    if let Some(p) = prices.models.get(model) {
        return Some(p);
    }
    prices
        .models
        .iter()
        .filter(|(id, _)| {
            model
                .strip_prefix(id.as_str())
                .is_some_and(|rest| rest.starts_with('-'))
        })
        .max_by_key(|(id, _)| id.len())
        .map(|(_, p)| p)
}

/// The provider name (`margin_shared::Provider::as_str`) a price row bills under.
pub fn price_provider(p: Provider) -> &'static str {
    match p {
        Provider::Openai => margin_shared::Provider::OpenAi.as_str(),
        Provider::Anthropic => margin_shared::Provider::Claude.as_str(),
        Provider::Chatgpt => margin_shared::Provider::ChatGpt.as_str(),
    }
}

/// A model-call row's provider: the row's own (what the service recorded), else the
/// provider `prices.toml` lists the model under, else "?".
pub fn row_provider(prices: &Prices, provider: Option<&str>, model: &str) -> String {
    match provider.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => crate::compare::provider_name(p),
        None => model_price(prices, model)
            .map(|p| price_provider(p.provider).to_string())
            .unwrap_or_else(|| "?".into()),
    }
}

/// USD of a call's usage at `prices`, by the rule of `docs/agent-providers-brief.md`
/// D14 (the service's `trace::cost_usd`, so a re-priced row matches one priced when it
/// was written): fresh input (`input − cached − cache_write`), cached reads and cache
/// writes at their own prices, output (reasoning included) per million; past a model's
/// `long_context.over` input tokens its tier's input and output prices; plus
/// `web_search` per 1 000 hosted searches. `None` for a model the table doesn't list
/// (never a guess) and always for ChatGPT calls (plan-billed: no dollar price).
pub fn price(
    prices: &Prices,
    provider: &str,
    model: &str,
    u: &margin_shared::context::TraceUsage,
) -> Option<f64> {
    if provider == margin_shared::Provider::ChatGpt.as_str() {
        return None;
    }
    let p = model_price(prices, model)?;
    if p.provider == Provider::Chatgpt {
        return None;
    }
    let (input, output) = match p.long_context {
        Some(tier) if u.input > tier.over => (tier.input, tier.output),
        _ => (p.input, p.output),
    };
    let fresh = u
        .input
        .saturating_sub(u.cached)
        .saturating_sub(u.cache_write);
    let micro = fresh as f64 * input
        + u.cached as f64 * p.cached_input
        + u.cache_write as f64 * p.cache_write
        + u.output as f64 * output;
    Some(micro / 1e6 + u.web_searches as f64 * p.web_search / 1e3)
}

/// The efficiency numbers of a set of trace rows. A row's own `cost_usd` (priced when
/// it was written) wins over today's prices; ChatGPT rows are plan-billed, never priced.
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
    let mut provider_ms: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut retrieval_ms = Vec::new();
    let mut embed_ms = 0.0;
    for r in rows {
        match r.kind {
            TraceKind::ModelCall => {
                let model = r.model.clone().unwrap_or_else(|| "?".into());
                let route = r.route.map(|x| x.as_str()).unwrap_or("?").to_string();
                let provider = row_provider(prices, r.provider.as_deref(), &model);
                let key = format!("{route} / {provider} / {model}");
                let u = r.usage.unwrap_or_default();
                let plan_billed = provider == margin_shared::Provider::ChatGpt.as_str();
                let cost = if plan_billed {
                    None
                } else {
                    r.cost_usd.or_else(|| price(prices, &provider, &model, &u))
                };
                let line = e.calls.entry(key.clone()).or_insert_with(|| CallStats {
                    route: route.clone(),
                    provider: provider.clone(),
                    model: model.clone(),
                    ..Default::default()
                });
                let by_provider =
                    e.providers
                        .entry(provider.clone())
                        .or_insert_with(|| CallStats {
                            provider: provider.clone(),
                            ..Default::default()
                        });
                for s in [line, by_provider, &mut e.total] {
                    s.calls += 1;
                    s.input += u.input;
                    s.cached += u.cached;
                    s.cache_write += u.cache_write;
                    s.output += u.output;
                    s.web_searches += u.web_searches;
                    match cost {
                        Some(c) => s.cost_usd += c,
                        None if plan_billed => s.plan_billed += 1,
                        None => s.unpriced += 1,
                    }
                }
                ms.entry(key).or_default().push(r.duration_ms);
                provider_ms.entry(provider).or_default().push(r.duration_ms);
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
            TraceKind::Alert => e.alerts.extend(AlertLine::of(r)),
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
    for (p, v) in &provider_ms {
        if let Some(s) = e.providers.get_mut(p) {
            s.mean_ms = crate::metrics::mean(v);
        }
    }
    e.step_overhead = Overhead::of(rows);
    e.alerts.sort_by_key(|a| a.at);
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
        // The rows carry no provider: `prices.toml` lists Sonnet under Anthropic.
        let w = &e.calls["write / claude / claude-sonnet-5-5"];
        // Priced: 0.5M uncached × $2 + 0.5M cached × $0.20 + 0.1M out × $10 = $2.10;
        // the second row's own cost_usd ($9) wins.
        assert!((w.cost_usd - 11.1).abs() < 1e-9, "{}", w.cost_usd);
        assert_eq!(w.calls, 2);
        assert_eq!(w.cache_share(), 50.0);
        assert_eq!(w.mean_ms, 200.0);
        assert_eq!((w.route.as_str(), w.provider.as_str()), ("write", "claude"));
        assert_eq!(e.calls["small / ? / unknown-model"].unpriced, 1);
        assert_eq!(e.providers["claude"].calls, 2);
        assert_eq!(e.providers["?"].unpriced, 1);
        assert_eq!(e.total.unpriced, 1);
        assert_eq!(e.total.calls, 3);
        assert_eq!((e.retrievals, e.retrieval_cache_hits), (2, 1));
        assert_eq!(e.retrieval_p95_ms, 80.0);
        assert_eq!(e.embed_chunks_per_s, 10.0);
        assert_eq!(e.tool_output_max_bytes, 9_000);
    }

    fn call(provider: Option<&str>, route: Route, model: &str, u: TraceUsage) -> TraceRow {
        let mut r = row(TraceKind::ModelCall, "t9", 20);
        r.provider = provider.map(String::from);
        r.route = Some(route);
        r.model = Some(model.into());
        r.usage = Some(u);
        r.duration_ms = 10.0;
        r
    }

    /// D14's pricing per provider, by hand.
    #[test]
    fn prices_per_provider() {
        let p = prices();
        // Claude: fresh input excludes cache reads and writes, which have their own
        // prices; web searches at $10 per 1 000.
        let u = TraceUsage {
            input: 300_000,
            cached: 100_000,
            cache_write: 100_000,
            output: 10_000,
            web_searches: 3,
            ..Default::default()
        };
        // 0.1M × $4 + 0.1M × $0.20 + 0.1M × $5 + 0.01M × $20 + 3 × $0.01 = $1.15.
        let c = price(&p, "claude", "claude-opus-5-5", &u).unwrap();
        assert!((c - 1.15).abs() < 1e-9, "{c}");
        // A dated snapshot the provider answered with is priced as its model.
        assert_eq!(price(&p, "claude", "claude-opus-5-5-20261001", &u), Some(c));
        // Haiku 5.5 past 100k input tokens: the long-context tier.
        let long = TraceUsage {
            input: 200_000,
            output: 1_000_000,
            ..Default::default()
        };
        let h = price(&p, "claude", "claude-haiku-5-5", &long).unwrap();
        assert!((h - (0.2 * 0.50 + 2.50)).abs() < 1e-9, "{h}");
        let short = TraceUsage {
            input: 100_000,
            ..Default::default()
        };
        let h = price(&p, "claude", "claude-haiku-5-5", &short).unwrap();
        assert!((h - 0.01).abs() < 1e-9, "{h}");
        // ChatGPT: plan-billed, never priced, even on a model OpenAI prices.
        assert_eq!(price(&p, "chatgpt", "gpt-6.1-sol", &u), None);
        assert!(price(&p, "openai", "gpt-6.1-sol", &u).is_some());
        // Unknown models are never guessed.
        assert_eq!(price(&p, "openai", "gpt-unknown", &u), None);
    }

    /// The evals' pricing is the service's: a row re-priced here matches one priced
    /// when it was written.
    #[test]
    fn prices_match_the_service() {
        let ours = prices();
        let text = std::fs::read_to_string(crate::golden::evals_dir().join("prices.toml")).unwrap();
        let theirs = margin_service::trace::Prices::parse(&text).unwrap();
        let usages = [
            TraceUsage {
                input: 300_000,
                cached: 100_000,
                cache_write: 100_000,
                output: 10_000,
                web_searches: 3,
                ..Default::default()
            },
            TraceUsage {
                input: 150_000,
                cached: 20_000,
                output: 2_000,
                ..Default::default()
            },
        ];
        for model in ours.models.keys() {
            for provider in ["openai", "claude", "chatgpt"] {
                for u in &usages {
                    let a = price(&ours, provider, model, u);
                    let b = margin_service::trace::cost_usd(&theirs, Some(provider), model, u);
                    match (a, b) {
                        (Some(a), Some(b)) => assert!((a - b).abs() < 1e-12, "{model}"),
                        (a, b) => assert_eq!(a, b, "{provider} {model}"),
                    }
                }
            }
        }
    }

    #[test]
    fn efficiency_by_provider() {
        let u = |input, cached, cache_write, web_searches| TraceUsage {
            input,
            cached,
            cache_write,
            output: 1_000,
            web_searches,
            ..Default::default()
        };
        let rows = vec![
            call(
                Some("claude"),
                Route::Chat,
                "claude-opus-5-5",
                u(10_000, 6_000, 2_000, 2),
            ),
            call(
                Some("claude"),
                Route::Small,
                "claude-haiku-5-5",
                u(1_000, 0, 0, 0),
            ),
            call(
                Some("openai"),
                Route::Chat,
                "gpt-6.1-sol",
                u(10_000, 5_000, 0, 1),
            ),
            // The same model on the ChatGPT plan: its own line, tokens counted, no
            // dollars.
            call(
                Some("chatgpt"),
                Route::Chat,
                "gpt-6.1-sol",
                u(10_000, 0, 0, 0),
            ),
        ];
        let e = efficiency(&rows, &prices());
        assert_eq!(
            e.calls.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "chat / chatgpt / gpt-6.1-sol",
                "chat / claude / claude-opus-5-5",
                "chat / openai / gpt-6.1-sol",
                "small / claude / claude-haiku-5-5",
            ]
        );
        let claude = &e.providers["claude"];
        assert_eq!(
            (
                claude.calls,
                claude.input,
                claude.cache_write,
                claude.web_searches
            ),
            (2, 11_000, 2_000, 2)
        );
        let opus = 2_000.0 * 4.0 + 6_000.0 * 0.2 + 2_000.0 * 5.0 + 1_000.0 * 20.0;
        let haiku = 1_000.0 * 0.1 + 1_000.0 * 0.5;
        let want = (opus + haiku) / 1e6 + 2.0 * 0.01;
        assert!(
            (claude.cost_usd - want).abs() < 1e-12,
            "{}",
            claude.cost_usd
        );
        let chatgpt = &e.providers["chatgpt"];
        assert_eq!(
            (
                chatgpt.calls,
                chatgpt.input,
                chatgpt.plan_billed,
                chatgpt.unpriced
            ),
            (1, 10_000, 1, 0)
        );
        assert_eq!(chatgpt.cost_usd, 0.0);
        assert!(e.providers["openai"].cost_usd > 0.0);
        assert_eq!(e.total.calls, 4);
        assert_eq!(e.total.plan_billed, 1);
        assert_eq!(e.total.unpriced, 0);
        assert_eq!(e.providers["openai"].mean_ms, 10.0);
        // A recorded cost_usd on a ChatGPT row is still not dollars.
        let mut odd = rows[3].clone();
        odd.cost_usd = Some(1.0);
        let e = efficiency(&[odd], &prices());
        assert_eq!(e.total.cost_usd, 0.0);
        assert_eq!(e.total.plan_billed, 1);
    }

    #[test]
    fn overhead_and_alert_lines() {
        let mut steps = Vec::new();
        for (i, (d, p)) in [(1_000.0, 900.0), (500.0, 480.0), (300.0, 310.0)]
            .into_iter()
            .enumerate()
        {
            let mut r = row(TraceKind::ModelCall, "t", 30 + i as i64);
            r.duration_ms = d;
            r.provider_ms = Some(p);
            steps.push(r);
        }
        // A job call without the provider's share doesn't count.
        let mut job = row(TraceKind::ModelCall, "j", 40);
        job.duration_ms = 9_999.0;
        steps.push(job);
        let mut clear = row(TraceKind::Alert, "alerts", 51);
        clear.error_code = Some("error_rate".into());
        clear.tool = Some("model_call".into());
        clear.duration_ms = 0.5;
        let mut rise = clear.clone();
        rise.id = "u50".into();
        rise.at = DateTime::from_timestamp(50, 0).unwrap();
        rise.ok = false;
        steps.push(clear);
        steps.push(rise);
        let e = efficiency(&steps, &prices());
        let o = e.step_overhead.unwrap();
        assert_eq!(o.steps, 3);
        // 100, 20 and 0 (a provider share past the duration counts as none).
        assert!((o.mean_ms - 40.0).abs() < 1e-9);
        assert_eq!((o.p95_ms, o.max_ms), (100.0, 100.0));
        assert_eq!(
            e.alerts
                .iter()
                .map(|a| (a.raised, a.name.as_str(), a.kind.as_deref()))
                .collect::<Vec<_>>(),
            [
                (true, "error_rate", Some("model_call")),
                (false, "error_rate", Some("model_call"))
            ]
        );
        assert_eq!(efficiency(&sample(), &prices()).step_overhead, None);
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
