//! `margin-eval compare A B` (§3.4, §3.5): per-metric deltas between two runs of one
//! suite, each with a paired bootstrap 95 % interval over the items both runs scored,
//! because the question is whether variant B helped on the same queries. Runs made with
//! different embedders are refused unless `--allow-embedder-change` is passed, so a win
//! is never a side effect of fresh vectors; runs whose models came from different
//! providers (`EvalRun.providers`) are refused unless `--allow-provider-change` is
//! passed, so a win is never a side effect of switching OpenAI for Claude
//! (`docs/agent-providers-brief.md` §4, P-evals).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::metrics::{Interval, mean, paired_bootstrap};
use crate::store::{EvalResult, EvalRun};

/// Bootstrap resamples.
pub const BOOTSTRAP_ITERS: usize = 10_000;
const SEED: u64 = 0x6d61_7267_696e;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricDelta {
    pub metric: String,
    /// Items both runs scored.
    pub n: usize,
    pub mean_a: f64,
    pub mean_b: f64,
    /// Of `b − a`.
    pub interval: Interval,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comparison {
    pub a: String,
    pub b: String,
    pub suite: String,
    pub deltas: Vec<MetricDelta>,
    /// Items only one run scored, per metric.
    pub unpaired: BTreeMap<String, usize>,
    pub warnings: Vec<String>,
}

/// What `compare` lets differ between two runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Allow {
    /// `--allow-embedder-change`.
    pub embedder_change: bool,
    /// `--allow-provider-change`.
    pub provider_change: bool,
    /// `--allow-prompt-change` (R2): runs whose `prompt_versions` differ.
    pub prompt_change: bool,
}

/// A provider's name as trace rows and `EvalRun.providers` spell it: "openai",
/// "chatgpt", "claude" (`margin_shared::Provider::as_str`). The runner's models call
/// Claude "anthropic" (the billing name `prices.toml` uses): the same provider.
pub fn provider_name(p: &str) -> String {
    let p = p.trim().to_ascii_lowercase();
    match p.as_str() {
        "anthropic" | "claude" => "claude".into(),
        _ => p,
    }
}

/// The sorted, deduplicated provider names of model ids like "anthropic:claude-opus-5-5"
/// or of bare provider names.
pub fn providers_of<'a>(ids: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = ids
        .into_iter()
        .map(|id| provider_name(id.split_once(':').map_or(id, |(p, _)| p)))
        .filter(|p| !p.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Compares the per-item results of two runs.
pub fn compare(
    a: &EvalRun,
    ra: &[EvalResult],
    b: &EvalRun,
    rb: &[EvalResult],
    allow: Allow,
) -> anyhow::Result<Comparison> {
    anyhow::ensure!(
        a.suite == b.suite,
        "{} is a {} run and {} a {} run: compare runs of one suite",
        a.id,
        a.suite,
        b.id,
        b.suite
    );
    anyhow::ensure!(
        allow.embedder_change || a.embedder_version == b.embedder_version,
        "{} used embedder {} and {} used {}: a difference could come from the vectors \
         alone; re-run both with one recording, or pass --allow-embedder-change",
        a.id,
        a.embedder_version,
        b.id,
        b.embedder_version
    );
    let mut warnings = Vec::new();
    let pa = providers_of(a.providers.iter().map(String::as_str));
    let pb = providers_of(b.providers.iter().map(String::as_str));
    let shown = |p: &[String]| p.join(" + ");
    if !pa.is_empty() && !pb.is_empty() && pa != pb {
        anyhow::ensure!(
            allow.provider_change,
            "{} ran on {} and {} on {}: a difference could come from the provider alone; \
             re-run both on one provider, or pass --allow-provider-change",
            a.id,
            shown(&pa),
            b.id,
            shown(&pb)
        );
        warnings.push(format!(
            "the provider changed ({} → {}): the deltas include the provider's effect",
            shown(&pa),
            shown(&pb)
        ));
    } else if pa.is_empty() != pb.is_empty() {
        let (none, some, p) = if pa.is_empty() {
            (a, b, &pb)
        } else {
            (b, a, &pa)
        };
        warnings.push(format!(
            "{} has no recorded provider ({} ran on {}): it may have run on another",
            none.id,
            some.id,
            shown(p)
        ));
    }
    if a.corpus_hash != b.corpus_hash {
        warnings.push("the runs are over different corpora".to_string());
    }
    for r in [a, b] {
        if !r.valid {
            warnings.push(format!("{} is invalid: a judge's canaries failed", r.id));
        }
    }
    let index = |rows: &[EvalResult]| {
        let mut m: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
        for r in rows.iter().filter(|r| r.item != "_all") {
            m.entry(r.metric.clone())
                .or_default()
                .insert(r.item.clone(), r.value);
        }
        m
    };
    let (ia, ib) = (index(ra), index(rb));
    let mut deltas = Vec::new();
    let mut unpaired = BTreeMap::new();
    for (metric, items_a) in &ia {
        let Some(items_b) = ib.get(metric) else {
            unpaired.insert(metric.clone(), items_a.len());
            continue;
        };
        let (mut xa, mut xb) = (Vec::new(), Vec::new());
        for (item, va) in items_a {
            if let Some(vb) = items_b.get(item) {
                xa.push(*va);
                xb.push(*vb);
            }
        }
        let lone = items_a.len() + items_b.len() - 2 * xa.len();
        if lone > 0 {
            unpaired.insert(metric.clone(), lone);
        }
        deltas.push(MetricDelta {
            metric: metric.clone(),
            n: xa.len(),
            mean_a: mean(&xa),
            mean_b: mean(&xb),
            interval: paired_bootstrap(&xa, &xb, BOOTSTRAP_ITERS, SEED),
        });
    }
    for (metric, items) in &ib {
        if !ia.contains_key(metric) {
            unpaired.insert(metric.clone(), items.len());
        }
    }
    Ok(Comparison {
        a: a.id.clone(),
        b: b.id.clone(),
        suite: a.suite.clone(),
        deltas,
        unpaired,
        warnings,
    })
}

impl Comparison {
    /// A Markdown table.
    pub fn markdown(&self) -> String {
        let mut s = format!(
            "## {} vs {} ({})\n\n| Metric | n | A | B | B − A | 95 % interval | |\n|---|---:|---:|---:|---:|---|---|\n",
            self.a, self.b, self.suite
        );
        for d in &self.deltas {
            s.push_str(&format!(
                "| {} | {} | {:.4} | {:.4} | {:+.4} | [{:+.4}, {:+.4}] | {} |\n",
                d.metric,
                d.n,
                d.mean_a,
                d.mean_b,
                d.interval.mean,
                d.interval.lo,
                d.interval.hi,
                if !d.interval.significant() {
                    "noise"
                } else if d.interval.mean > 0.0 {
                    "B better"
                } else {
                    "B worse"
                }
            ));
        }
        for w in &self.warnings {
            s.push_str(&format!("\n**Warning:** {w}\n"));
        }
        for (m, n) in &self.unpaired {
            s.push_str(&format!(
                "\n{n} {m} item(s) scored by only one run, left out.\n"
            ));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn run(id: &str, emb: &str) -> EvalRun {
        EvalRun {
            id: id.into(),
            suite: "retrieval".into(),
            variant: id.into(),
            pipeline_hash: id.into(),
            embedder_version: emb.into(),
            corpus_hash: "c".into(),
            key: id.into(),
            started_at: Utc::now(),
            finished_at: None,
            judges: vec![],
            providers: vec![],
            prompt_versions: Vec::new(),
            valid: true,
            notes: vec![],
            summary: serde_json::Value::Null,
        }
    }

    const STRICT: Allow = Allow {
        embedder_change: false,
        provider_change: false,
        prompt_change: false,
    };

    fn rows(run: &str, vals: &[(&str, f64)]) -> Vec<EvalResult> {
        vals.iter()
            .map(|(item, v)| EvalResult {
                run_id: run.into(),
                at: DateTime::<Utc>::UNIX_EPOCH,
                item: (*item).into(),
                group: String::new(),
                metric: "recall_at_5".into(),
                value: *v,
                judge: None,
            })
            .collect()
    }

    #[test]
    fn deltas_pair_by_item() {
        let a = run("a", "v");
        let b = run("b", "v");
        let ra = rows("a", &[("q1", 0.0), ("q2", 0.5), ("q3", 1.0), ("q9", 1.0)]);
        let rb = rows("b", &[("q1", 1.0), ("q2", 1.0), ("q3", 1.0), ("_all", 0.9)]);
        let c = compare(&a, &ra, &b, &rb, STRICT).unwrap();
        let d = &c.deltas[0];
        assert_eq!(d.n, 3);
        assert!((d.interval.mean - 0.5).abs() < 1e-9);
        assert_eq!(c.unpaired["recall_at_5"], 1);
        let md = c.markdown();
        assert!(md.contains("| recall_at_5 | 3 |"), "{md}");
        assert!(md.contains("left out"));
    }

    #[test]
    fn refuses_across_embedders_and_suites() {
        let a = run("a", "v1");
        let b = run("b", "v2");
        let err = compare(&a, &[], &b, &[], STRICT).unwrap_err().to_string();
        assert!(err.contains("--allow-embedder-change"), "{err}");
        let embedder = Allow {
            embedder_change: true,
            ..STRICT
        };
        assert!(compare(&a, &[], &b, &[], embedder).is_ok());
        let mut c = run("c", "v1");
        c.suite = "courses".into();
        assert!(compare(&a, &[], &c, &[], embedder).is_err());
        let mut d = run("d", "v1");
        d.valid = false;
        d.corpus_hash = "other".into();
        let cmp = compare(&a, &[], &d, &[], STRICT).unwrap();
        assert_eq!(cmp.warnings.len(), 2);
    }

    #[test]
    fn refuses_across_providers_unless_allowed() {
        let mut a = run("a", "v");
        a.providers = vec!["openai".into()];
        let mut b = run("b", "v");
        b.providers = vec!["claude".into()];
        let ra = rows("a", &[("q1", 0.0), ("q2", 1.0)]);
        let rb = rows("b", &[("q1", 1.0), ("q2", 1.0)]);
        let err = compare(&a, &ra, &b, &rb, STRICT).unwrap_err().to_string();
        assert!(err.contains("--allow-provider-change"), "{err}");
        assert!(err.contains("a ran on openai and b on claude"), "{err}");
        // The embedder flag doesn't let a provider change through.
        let embedder = Allow {
            embedder_change: true,
            ..STRICT
        };
        assert!(compare(&a, &ra, &b, &rb, embedder).is_err());
        let allowed = Allow {
            provider_change: true,
            ..STRICT
        };
        let c = compare(&a, &ra, &b, &rb, allowed).unwrap();
        assert_eq!(c.deltas[0].n, 2);
        assert!(
            c.warnings[0].contains("openai → claude"),
            "{:?}",
            c.warnings
        );
        assert!(c.markdown().contains("**Warning:** the provider changed"));
        // ChatGPT is its own provider, even on model ids OpenAI also serves.
        b.providers = vec!["chatgpt".into()];
        assert!(compare(&a, &ra, &b, &rb, STRICT).is_err());
        // "anthropic" (the runner's name) and "claude" (the traces') are one provider.
        a.providers = vec!["anthropic".into()];
        b.providers = vec!["claude".into()];
        let same = compare(&a, &ra, &b, &rb, STRICT).unwrap();
        assert!(same.warnings.is_empty(), "{:?}", same.warnings);
        // The same mix of providers on both sides compares.
        a.providers = vec!["claude".into(), "openai".into()];
        b.providers = vec!["openai".into(), "anthropic".into()];
        assert!(compare(&a, &ra, &b, &rb, STRICT).is_ok());
        // A run recorded before providers were compares, with a warning.
        b.providers.clear();
        let old = compare(&a, &ra, &b, &rb, STRICT).unwrap();
        assert!(
            old.warnings[0].contains("b has no recorded provider"),
            "{:?}",
            old.warnings
        );
        // Two runs with no model under test (retrieval) compare as before.
        a.providers.clear();
        assert!(
            compare(&a, &ra, &b, &rb, STRICT)
                .unwrap()
                .warnings
                .is_empty()
        );
    }

    #[test]
    fn provider_names() {
        assert_eq!(
            providers_of([
                "anthropic:claude-opus-5-5",
                "openai:gpt-6-luna",
                "claude",
                "openai"
            ]),
            ["claude", "openai"]
        );
        assert_eq!(provider_name(" ChatGPT"), "chatgpt");
        assert!(providers_of([""]).is_empty());
    }
}
