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
pub fn check_retrieval(
    gates: &Gates,
    baseline: &Baseline,
    report: &RetrievalReport,
) -> Vec<String> {
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
    if b.latency_p95_ms > 0.0 && report.latency_p95_ms > b.latency_p95_ms * g.p95_latency_max_ratio
    {
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

// ---------------------------------------------------------------------------
// The prompt lock (R2, `docs/r2-brief.md` D4.2)
// ---------------------------------------------------------------------------

/// `evals/prompts.lock`.
pub const PROMPT_LOCK_FILE: &str = "prompts.lock";

/// One job's line in the lock: the version and the hash of its constant block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedPrompt {
    pub version: String,
    pub constant_hash: String,
}

/// `evals/prompts.lock`: one table per job.
pub type PromptLock = BTreeMap<String, LockedPrompt>;

/// Every prompt with a hand-bumped version: `margin_ai`'s catalog plus the agent's
/// system prompt (`AGENT_PROMPT_VERSION`) and its condense call
/// (`SUMMARY_PROMPT_VERSION`). The chat title versions itself by hash
/// (`trace::prompt_version` in the service), so it needs no lock.
pub fn locked_catalog() -> Vec<margin_shared::context::PromptInfo> {
    let mut catalog = margin_ai::prompts::catalog();
    catalog.push(margin_ai::prompts::prompt_info(
        "agent",
        margin_agent::AGENT_PROMPT_VERSION,
        margin_agent::system_prompt(),
    ));
    catalog.push(margin_ai::prompts::prompt_info(
        "condense",
        margin_agent::SUMMARY_PROMPT_VERSION,
        margin_agent::SUMMARY_INSTRUCTIONS,
    ));
    catalog
}

/// The lock's text for `catalog` (what `margin-eval prompts --write-lock` writes).
pub fn prompt_lock_toml(catalog: &[margin_shared::context::PromptInfo]) -> String {
    let lock: PromptLock = catalog
        .iter()
        .map(|p| {
            (
                p.job.clone(),
                LockedPrompt {
                    version: p.version.clone(),
                    constant_hash: p.constant_hash.clone(),
                },
            )
        })
        .collect();
    format!(
        "# Every prompt's version and the FNV-1a 64 hash of its constant block \
         (docs/r2-brief.md D4.2).\n# A changed constant block needs a new version; \
         rewrite this file with `margin-eval prompts --write-lock`.\n\n{}",
        toml::to_string(&lock).expect("a lock is TOML")
    )
}

/// What differs between the lock and the catalog, one line each; empty when they agree.
/// The same version with another hash is the failure the lock exists for: a constant
/// block changed without a version bump.
pub fn check_prompt_lock(
    lock: &PromptLock,
    catalog: &[margin_shared::context::PromptInfo],
) -> Vec<String> {
    let mut out = Vec::new();
    for p in catalog {
        match lock.get(&p.job) {
            None => out.push(format!(
                "the {} prompt ({}) isn't in evals/prompts.lock; add it with \
                 `margin-eval prompts --write-lock`",
                p.job, p.version
            )),
            Some(l) if l.version == p.version && l.constant_hash != p.constant_hash => {
                out.push(format!(
                    "the {} prompt's constant block changed under {}; bump its version and \
                     update evals/prompts.lock (`margin-eval prompts --write-lock`)",
                    p.job, p.version
                ))
            }
            Some(l) if l.version != p.version => out.push(format!(
                "the {} prompt is {} but evals/prompts.lock says {}; update the lock \
                 (`margin-eval prompts --write-lock`)",
                p.job, p.version, l.version
            )),
            Some(_) => {}
        }
    }
    for job in lock.keys() {
        if !catalog.iter().any(|p| &p.job == job) {
            out.push(format!(
                "evals/prompts.lock names a {job} prompt the catalog doesn't have"
            ));
        }
    }
    out
}

/// `margin-eval prompts [--write-lock]`: every locked prompt's job, version and
/// constant-block hash beside what `prompts.lock` says (read from `paths.out`, else
/// `paths.evals`), and whether they agree. With `write_lock` the lock under `paths.out`
/// is rewritten from the catalog first (do it with the version bump, in the same
/// change). Returns the text and whether the lock now agrees.
pub fn prompts_command(
    paths: &crate::runner::Paths,
    write_lock: bool,
) -> anyhow::Result<(String, bool)> {
    let catalog = locked_catalog();
    let out = paths.out.join(PROMPT_LOCK_FILE);
    let mut text = String::new();
    if write_lock {
        std::fs::write(&out, prompt_lock_toml(&catalog))
            .with_context(|| format!("couldn't write {}", out.display()))?;
        text.push_str(&format!("wrote {}\n\n", out.display()));
    }
    let path = if out.is_file() {
        out
    } else {
        paths.evals.join(PROMPT_LOCK_FILE)
    };
    let lock: PromptLock = match std::fs::read_to_string(&path) {
        Ok(t) => toml::from_str(&t).with_context(|| format!("bad {}", path.display()))?,
        Err(_) => PromptLock::new(),
    };
    text.push_str(&format!(
        "{:<14} {:<16} {:<18} {:>9}  lock\n",
        "job", "version", "constant hash", "chars"
    ));
    for p in &catalog {
        let state = match lock.get(&p.job) {
            None => "missing".to_string(),
            Some(l) if l.version != p.version => format!("lock says {}", l.version),
            Some(l) if l.constant_hash != p.constant_hash => {
                "CHANGED under this version".to_string()
            }
            Some(_) => "ok".to_string(),
        };
        text.push_str(&format!(
            "{:<14} {:<16} {:<18} {:>9}  {state}\n",
            p.job, p.version, p.constant_hash, p.constant_chars
        ));
    }
    let violations = check_prompt_lock(&lock, &catalog);
    for v in &violations {
        text.push_str(&format!("\n{v}"));
    }
    if !violations.is_empty() {
        text.push('\n');
    }
    Ok((text, violations.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::golden::evals_dir;
    use crate::retrieval::IndexSummary;

    #[test]
    fn the_prompts_command_reads_and_writes_the_lock() {
        let out = std::env::temp_dir().join(format!(
            "margin-evals-prompts-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        std::fs::create_dir_all(&out).unwrap();
        let paths = crate::runner::Paths {
            out: out.clone(),
            ..crate::runner::Paths::same(&evals_dir())
        };
        // A lock with the lesson prompt changed under its version.
        let mut lock: PromptLock = toml::from_str(&prompt_lock_toml(&locked_catalog())).unwrap();
        lock.get_mut("lesson").unwrap().constant_hash = "0000000000000000".into();
        std::fs::write(out.join(PROMPT_LOCK_FILE), toml::to_string(&lock).unwrap()).unwrap();
        let (text, ok) = prompts_command(&paths, false).unwrap();
        assert!(!ok);
        assert!(text.contains("CHANGED under this version"), "{text}");
        assert!(text.contains("bump its version"), "{text}");
        // --write-lock rewrites it from the catalog.
        let (text, ok) = prompts_command(&paths, true).unwrap();
        assert!(ok, "{text}");
        assert!(text.starts_with("wrote "), "{text}");
        assert!(text.contains("rerank-v1"), "{text}");
        let written: PromptLock =
            toml::from_str(&std::fs::read_to_string(out.join(PROMPT_LOCK_FILE)).unwrap()).unwrap();
        assert!(check_prompt_lock(&written, &locked_catalog()).is_empty());
        std::fs::remove_dir_all(&out).unwrap();
    }

    #[test]
    fn the_prompt_lock_catches_a_changed_constant_under_one_version() {
        let catalog = locked_catalog();
        let lock: PromptLock = toml::from_str(&prompt_lock_toml(&catalog)).unwrap();
        assert!(check_prompt_lock(&lock, &catalog).is_empty());
        let mut changed = catalog.clone();
        changed[0].constant_hash = "0000000000000000".into();
        let v = check_prompt_lock(&lock, &changed);
        assert_eq!(v.len(), 1);
        assert!(v[0].contains("bump its version"), "{v:?}");
        let mut bumped = catalog.clone();
        bumped[0].version = "lesson-v9".into();
        bumped[0].constant_hash = "0000000000000000".into();
        assert!(check_prompt_lock(&lock, &bumped)[0].contains("update the lock"));
        assert_eq!(check_prompt_lock(&lock, &catalog[1..]).len(), 1);
        assert!(catalog.iter().any(|p| p.job == "agent"));
    }

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
            stages: Default::default(),
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
