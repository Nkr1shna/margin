//! CI regression gates (`docs/agent-rag-architecture.md` §3.3): the committed baseline
//! (`evals/baseline.json`, written by `margin-eval run --suite retrieval --variant
//! baseline --write-baseline` and committed on purpose with the change that improves
//! it) against a fresh run, with the allowances of `evals/gates.toml`.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::Gates;
use crate::retrieval::{KindMetrics, RetrievalReport};

/// `evals/baseline.json`.
pub const BASELINE_FILE: &str = "baseline.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    /// The variant it was measured with (`evals/variants/<name>.toml`).
    pub variant: String,
    pub pipeline_hash: String,
    pub embedder_version: String,
    pub corpus_hash: String,
    /// RFC 3339.
    pub recorded_at: String,
    pub retrieval: RetrievalBaseline,
    /// Lexical support (points) of the recorded generations' citations, when recorded.
    #[serde(default)]
    pub citation_lexical_support: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrievalBaseline {
    pub overall: KindMetrics,
    pub per_kind: BTreeMap<String, KindMetrics>,
    pub negative_fp_rate: f64,
    pub latency_p95_ms: f64,
}

impl Baseline {
    pub fn from_report(
        variant: &str,
        corpus_hash: &str,
        recorded_at: chrono::DateTime<chrono::Utc>,
        report: &RetrievalReport,
    ) -> Self {
        Self {
            variant: variant.to_string(),
            pipeline_hash: report.pipeline_hash.clone(),
            embedder_version: report.embedder_version.clone(),
            corpus_hash: corpus_hash.to_string(),
            recorded_at: recorded_at.to_rfc3339(),
            retrieval: RetrievalBaseline {
                overall: report.overall,
                per_kind: report.per_kind.clone(),
                negative_fp_rate: report.negative_fp_rate,
                latency_p95_ms: report.latency_p95_ms,
            },
            citation_lexical_support: None,
        }
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "couldn't read {} (the owner's first `margin-eval run --suite retrieval \
                 --variant baseline --write-baseline` writes it)",
                path.display()
            )
        })?;
        serde_json::from_str(&text).with_context(|| format!("bad {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        std::fs::write(path, serde_json::to_string_pretty(self)? + "\n")
            .with_context(|| format!("couldn't write {}", path.display()))
    }
}

/// Every way `report` breaks the retrieval gates against `baseline`; empty = pass.
pub fn check_retrieval(gates: &Gates, baseline: &Baseline, report: &RetrievalReport) -> Vec<String> {
    let g = &gates.retrieval;
    let b = &baseline.retrieval;
    let mut out = Vec::new();
    if baseline.embedder_version != report.embedder_version {
        out.push(format!(
            "the baseline was measured with embedder {}, this run with {}: re-record and \
             re-baseline together",
            baseline.embedder_version, report.embedder_version
        ));
    }
    let mut drop = |what: &str, base: f64, now: f64, max: f64| {
        if base - now > max + 1e-9 {
            out.push(format!(
                "{what} dropped {:.2} points ({base:.2} → {now:.2}; at most {max} allowed)",
                base - now
            ));
        }
    };
    drop(
        "recall@5",
        b.overall.recall_at_5,
        report.overall.recall_at_5,
        g.recall_at_5_max_drop,
    );
    drop(
        "nDCG@10",
        b.overall.ndcg_at_10,
        report.overall.ndcg_at_10,
        g.ndcg_at_10_max_drop,
    );
    for (kind, base) in &b.per_kind {
        let now = report.per_kind.get(kind).copied().unwrap_or_default();
        drop(
            &format!("{kind} recall@5"),
            base.recall_at_5,
            now.recall_at_5,
            g.per_kind_max_drop,
        );
        drop(
            &format!("{kind} nDCG@10"),
            base.ndcg_at_10,
            now.ndcg_at_10,
            g.per_kind_max_drop,
        );
    }
    if report.negative_fp_rate - b.negative_fp_rate > g.negative_fp_max_rise + 1e-9 {
        out.push(format!(
            "negatives' false-positive rate rose {:.2} points ({:.2} → {:.2}; at most {} allowed)",
            report.negative_fp_rate - b.negative_fp_rate,
            b.negative_fp_rate,
            report.negative_fp_rate,
            g.negative_fp_max_rise
        ));
    }
    if b.latency_p95_ms > 0.0 && report.latency_p95_ms > b.latency_p95_ms * g.p95_latency_max_ratio {
        out.push(format!(
            "p95 retrieval latency {:.1} ms is over {}× the baseline's {:.1} ms",
            report.latency_p95_ms, g.p95_latency_max_ratio, b.latency_p95_ms
        ));
    }
    out
}

/// The citation gate on recorded generations: every marker resolves, and lexical
/// support hasn't dropped more than allowed.
pub fn check_citations(
    gates: &Gates,
    baseline: &Baseline,
    resolution_pct: f64,
    lexical_support_pct: f64,
) -> Vec<String> {
    let mut out = Vec::new();
    if resolution_pct + 1e-9 < gates.citations.resolution_min {
        out.push(format!(
            "{resolution_pct:.2} % of citation markers resolve; {} % required",
            gates.citations.resolution_min
        ));
    }
    if let Some(base) = baseline.citation_lexical_support
        && base - lexical_support_pct > gates.citations.lexical_support_max_drop + 1e-9
    {
        out.push(format!(
            "citation lexical support dropped {:.2} points ({base:.2} → {lexical_support_pct:.2})",
            base - lexical_support_pct
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::golden::evals_dir;
    use crate::retrieval::IndexSummary;

    fn gates() -> Gates {
        toml::from_str(&std::fs::read_to_string(evals_dir().join("gates.toml")).unwrap()).unwrap()
    }

    fn km(r5: f64, n10: f64) -> KindMetrics {
        KindMetrics {
            queries: 10,
            recall_at_5: r5,
            recall_at_10: r5,
            mrr: r5,
            ndcg_at_10: n10,
        }
    }

    fn report(overall: KindMetrics, semantic: KindMetrics, fp: f64, p95: f64) -> RetrievalReport {
        RetrievalReport {
            embedder_version: "v".into(),
            pipeline_hash: "h".into(),
            index: IndexSummary::default(),
            overall,
            per_kind: [("semantic".to_string(), semantic)].into(),
            negative_fp_rate: fp,
            fp_threshold: 0.5,
            picture_hit_rate: 0.0,
            dedup_rate: 0.0,
            latency_p50_ms: 1.0,
            latency_p95_ms: p95,
            queries: vec![],
        }
    }

    #[test]
    fn the_gates_hold_and_break_where_they_should() {
        let g = gates();
        let base_report = report(km(80.0, 70.0), km(75.0, 65.0), 10.0, 5.0);
        let base = Baseline::from_report("baseline", "c", chrono::Utc::now(), &base_report);
        // Unchanged, and within every allowance.
        assert!(check_retrieval(&g, &base, &base_report).is_empty());
        let ok = report(km(79.5, 69.2), km(72.5, 62.5), 11.5, 9.0);
        assert_eq!(check_retrieval(&g, &base, &ok), Vec::<String>::new());
        // Each way of failing.
        let bad = report(km(78.0, 68.0), km(70.0, 60.0), 13.0, 11.0);
        let v = check_retrieval(&g, &base, &bad);
        let all = v.join("\n");
        for want in [
            "recall@5 dropped 2.00",
            "nDCG@10 dropped 2.00",
            "semantic recall@5",
            "semantic nDCG@10",
            "false-positive rate rose 3.00",
            "p95 retrieval latency",
        ] {
            assert!(all.contains(want), "{want} not in:\n{all}");
        }
        // A query kind that vanished counts as a drop to zero.
        let mut gone = base_report.clone();
        gone.per_kind.clear();
        assert!(!check_retrieval(&g, &base, &gone).is_empty());
        // Another embedder is never compared silently.
        let mut other = base_report.clone();
        other.embedder_version = "w".into();
        assert!(check_retrieval(&g, &base, &other)[0].contains("embedder"));
    }

    #[test]
    fn the_citation_gate() {
        let g = gates();
        let mut base = Baseline::from_report(
            "baseline",
            "c",
            chrono::Utc::now(),
            &report(km(1.0, 1.0), km(1.0, 1.0), 0.0, 1.0),
        );
        assert!(check_citations(&g, &base, 100.0, 50.0).is_empty());
        assert_eq!(check_citations(&g, &base, 99.0, 50.0).len(), 1);
        base.citation_lexical_support = Some(60.0);
        assert!(check_citations(&g, &base, 100.0, 58.5).is_empty());
        assert_eq!(check_citations(&g, &base, 100.0, 57.0).len(), 1);
    }

    #[test]
    fn a_baseline_round_trips() {
        let base = Baseline::from_report(
            "baseline",
            "c",
            chrono::Utc::now(),
            &report(km(1.0, 2.0), km(3.0, 4.0), 5.0, 6.0),
        );
        let dir = std::env::temp_dir().join(format!("margin-evals-base-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("baseline.json");
        base.save(&path).unwrap();
        assert_eq!(Baseline::load(&path).unwrap(), base);
        assert!(
            Baseline::load(&dir.join("missing.json"))
                .unwrap_err()
                .to_string()
                .contains("--write-baseline")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
