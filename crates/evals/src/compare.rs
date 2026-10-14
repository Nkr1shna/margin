//! `margin-eval compare A B` (§3.4, §3.5): per-metric deltas between two runs of one
//! suite, each with a paired bootstrap 95 % interval over the items both runs scored,
//! because the question is whether variant B helped on the same queries. Runs made with
//! different embedders are refused unless `--allow-embedder-change` is passed, so a win
//! is never a side effect of fresh vectors.

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

/// Compares the per-item results of two runs.
pub fn compare(
    a: &EvalRun,
    ra: &[EvalResult],
    b: &EvalRun,
    rb: &[EvalResult],
    allow_embedder_change: bool,
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
        allow_embedder_change || a.embedder_version == b.embedder_version,
        "{} used embedder {} and {} used {}: a difference could come from the vectors \
         alone; re-run both with one recording, or pass --allow-embedder-change",
        a.id,
        a.embedder_version,
        b.id,
        b.embedder_version
    );
    let mut warnings = Vec::new();
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
            s.push_str(&format!("\n{n} {m} item(s) scored by only one run, left out.\n"));
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
            valid: true,
            notes: vec![],
            summary: serde_json::Value::Null,
        }
    }

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
        let c = compare(&a, &ra, &b, &rb, false).unwrap();
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
        let err = compare(&a, &[], &b, &[], false).unwrap_err().to_string();
        assert!(err.contains("--allow-embedder-change"), "{err}");
        assert!(compare(&a, &[], &b, &[], true).is_ok());
        let mut c = run("c", "v1");
        c.suite = "courses".into();
        assert!(compare(&a, &[], &c, &[], true).is_err());
        let mut d = run("d", "v1");
        d.valid = false;
        d.corpus_hash = "other".into();
        let cmp = compare(&a, &[], &d, &[], false).unwrap();
        assert_eq!(cmp.warnings.len(), 2);
    }
}
