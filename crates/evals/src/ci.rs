//! The CI gates of §3.3 in one pass, as `tests/gates.rs` runs them: retrieval on the
//! recorded embeddings with the real indexes against `evals/baseline.json`; packing on
//! every golden query at a small and a large model window; tool-output size; citations
//! on the recorded generations; the prompt-prefix identity of the variant's prompt set
//! over every golden lesson. Returns every violation, so one CI failure lists them all.

use margin_shared::routes::RETRIEVAL_K_FUSED;

use crate::Gates;
use crate::citations::CitationReport;
use crate::corpus::corpus_hash;
use crate::courses::{GENERATIONS_DIR, check_prefixes, golden_prompts, load_generations};
use crate::gates::{Baseline, check_citations, check_retrieval};
use crate::index::scope_of;
use crate::packing::{check_packing, tool_output_bytes};
use crate::retrieval::RetrievalReport;
use crate::runner::Env;

/// The model windows packing is checked at: a small one where the budget binds, and a
/// current model's.
pub const GATE_WINDOWS: [usize; 2] = [8_000, 200_000];

#[derive(Debug)]
pub struct GateOutcome {
    pub retrieval: RetrievalReport,
    pub citations: Option<CitationReport>,
    pub violations: Vec<String>,
}

pub async fn run_gates(
    env: &Env<'_>,
    gates: &Gates,
    baseline: &Baseline,
    require_generations: bool,
) -> anyhow::Result<GateOutcome> {
    let mut v = Vec::new();
    let corpus = corpus_hash(&env.paths.corpus())?;
    if corpus != baseline.corpus_hash {
        v.push("the corpus changed since the baseline: re-record the embeddings and re-baseline in the same change".into());
    }
    let (cfg, _) = env.paths.variant(&baseline.variant)?;
    if cfg.hash() != baseline.pipeline_hash {
        v.push(format!(
            "variant {} is now pipeline {}, the baseline was measured on {}: re-baseline on purpose with the change",
            baseline.variant,
            cfg.hash(),
            baseline.pipeline_hash
        ));
    }
    let index = env.index(&cfg).await?;

    // Retrieval.
    let queries = env.paths.retrieval_golden()?;
    let report = crate::retrieval::run(&index, &queries).await?;
    v.extend(check_retrieval(gates, baseline, &report));

    // Packing and tool output, per query.
    for q in &queries {
        let ranked = index
            .search(std::slice::from_ref(&q.query), &scope_of(q.scope))
            .await?;
        let bytes = tool_output_bytes(&ranked.hits);
        if bytes > gates.packing.tool_output_max_bytes {
            v.push(format!(
                "{}: a search returning its top hits is {bytes} bytes, over {}",
                q.id, gates.packing.tool_output_max_bytes
            ));
        }
        for w in GATE_WINDOWS {
            let budget = (env.pack_budget)(&cfg.budget, w, RETRIEVAL_K_FUSED);
            let packed = index.pack(ranked.hits.clone(), &budget);
            for p in check_packing(&packed, &budget) {
                v.push(format!("{} (window {w}): {p}", q.id));
            }
        }
    }

    // Citations on recorded generations of this prompt version.
    let gens = load_generations(&env.paths.evals.join(GENERATIONS_DIR).join(&cfg.prompts))?;
    let citations = if gens.is_empty() {
        if require_generations {
            v.push(format!(
                "no recorded generations for {} (run `margin-eval record-generations`)",
                cfg.prompts
            ));
        }
        None
    } else {
        let mut total = CitationReport::default();
        for g in &gens {
            total.add(&g.citations());
        }
        v.extend(check_citations(
            gates,
            baseline,
            total.resolution_pct(),
            total.lexical_support_pct(),
        ));
        Some(total)
    };

    // Prompt prefixes.
    let (prompts, unique) = golden_prompts(index.pipeline.prompts.as_ref(), &env.paths.courses()?);
    for p in check_prefixes(&prompts, &unique) {
        v.push(format!("prompt {}: {p}", cfg.prompts));
    }

    Ok(GateOutcome {
        retrieval: report,
        citations,
        violations: v,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use margin_service::pipeline::{
        Diversity, Hit, PackBudget, PlanInput, Prompt, PromptSet, Registry, StageCtor,
    };
    use margin_shared::pipeline::Budget;

    use super::*;
    use crate::corpus::{Extraction, documents};
    use crate::golden::evals_dir;
    use crate::index::testing::register_stand_ins;
    use crate::recorded::FakeVectors;
    use crate::runner::{Paths, run_retrieval};
    use crate::store::EvalStore;

    struct LongPrompts;
    impl PromptSet for LongPrompts {
        fn version(&self) -> &'static str {
            "lesson-v2"
        }
        fn lesson(&self, ctx: &margin_ai::LessonContext) -> Prompt {
            Prompt {
                version: "lesson-v2".into(),
                constant: "Teach one lesson well. ".repeat(250),
                variable: ctx.outline[ctx.index].title.clone(),
                cache_key: "lesson:lesson-v2".into(),
            }
        }
        fn plan(&self, _: &PlanInput) -> Prompt {
            unreachable!()
        }
        fn rerank(&self, _: &str, _: &[Hit]) -> Prompt {
            unreachable!()
        }
    }

    fn register(reg: &mut Registry) {
        register_stand_ins(reg);
        reg.register(
            "lesson-v2",
            StageCtor::Prompts(Arc::new(|_, _| Ok(Arc::new(LongPrompts)))),
        );
    }

    fn budget(_: &Budget, window: usize, k: usize) -> PackBudget {
        PackBudget {
            tokens: window,
            images_max: 8,
            min_score_pct: 25,
            passages_min: 0,
            k: k.min(8),
            diversity: Diversity {
                lambda: 0.7,
                per_document_max: Some(8),
            },
        }
    }

    #[tokio::test]
    async fn the_gate_harness_passes_itself_and_catches_a_regression() {
        let out = std::env::temp_dir().join(format!("margin-evals-ci-{}", std::process::id()));
        std::fs::create_dir_all(&out).unwrap();
        let paths = Paths {
            out: out.clone(),
            ..Paths::same(&evals_dir())
        };
        let docs = documents(&paths.corpus()).unwrap();
        static V: FakeVectors = FakeVectors;
        let env = Env {
            extraction: Extraction::fixture(&paths.corpus(), &docs).unwrap(),
            paths,
            vectors: &V,
            register: &register,
            pack_budget: &budget,
        };
        let store = EvalStore::memory().await.unwrap();
        run_retrieval(&env, "baseline", &store, true).await.unwrap();
        let gates: Gates =
            toml::from_str(&std::fs::read_to_string(evals_dir().join("gates.toml")).unwrap())
                .unwrap();
        let mut base = Baseline::load(&out.join(crate::gates::BASELINE_FILE)).unwrap();
        base.retrieval.latency_p95_ms = 1e9;
        let o = run_gates(&env, &gates, &base, false).await.unwrap();
        // The stand-in packer takes k hits and the fake search returns 8 of up to
        // 1 100-character passages; only the tool-size rule may object.
        let others: Vec<&String> = o
            .violations
            .iter()
            .filter(|v| !v.contains("bytes, over"))
            .collect();
        assert!(others.is_empty(), "{others:?}");
        assert!(o.citations.is_none());
        // A baseline that claims better retrieval fails the gate, naming the metric.
        base.retrieval.overall.recall_at_5 += 5.0;
        base.pipeline_hash = "other".into();
        let o = run_gates(&env, &gates, &base, true).await.unwrap();
        let all = o.violations.join("\n");
        assert!(all.contains("recall@5 dropped"), "{all}");
        assert!(all.contains("re-baseline on purpose"), "{all}");
        assert!(all.contains("no recorded generations"), "{all}");
        std::fs::remove_dir_all(&out).unwrap();
    }
}
