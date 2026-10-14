//! The CI regression gate (`docs/agent-rag-architecture.md` §3.3, §6.4 e2e 7):
//! `cargo test -p margin-evals`. Recorded fixtures and in-memory SurrealDB with the
//! service's real indexes only; no model API, no Hugging Face, no Core ML model.

use margin_evals::Gates;
use margin_evals::ci::run_gates;
use margin_evals::corpus::{EXTRACTED_FILE, Extraction};
use margin_evals::gates::{BASELINE_FILE, Baseline};
use margin_evals::golden::evals_dir;
use margin_evals::recorded::{EMBEDDINGS_FILE, RecordedVectors};
use margin_evals::runner::{Env, Paths};

/// The real pipeline over the recorded corpus against the committed baseline (§6.4
/// e2e 7). Everything it runs is on the branch (`sections-v2`, `hybrid-rrf`/`rrf`/
/// `mmr-v1`, `pack_budget`, the `lesson-v2` prompts); what is missing are the owner's
/// recordings, made by hand on the service Mac and committed: `margin-eval
/// record-embeddings` (`evals/recorded/{embeddings.bin,extracted.json}`), `margin-eval
/// record-generations` (`evals/recorded/generations/lesson-v2/…`) and `margin-eval run
/// --suite retrieval --variant baseline --write-baseline` (`evals/baseline.json`).
/// Remove the `ignore` in the commit that adds them.
#[tokio::test]
#[ignore = "waits for the owner's recorded embeddings and baseline (margin-eval record-embeddings, record-generations, run --write-baseline on the service Mac); see the doc comment"]
async fn ci_gates_on_the_recorded_corpus() {
    let evals = evals_dir();
    let vectors = RecordedVectors::open(&evals.join(EMBEDDINGS_FILE)).unwrap();
    let baseline = Baseline::load(&evals.join(BASELINE_FILE)).unwrap();
    let gates: Gates =
        toml::from_str(&std::fs::read_to_string(evals.join("gates.toml")).unwrap()).unwrap();
    let env = Env {
        paths: Paths::same(&evals),
        vectors: &vectors,
        extraction: Extraction::load(&evals.join(EXTRACTED_FILE)).unwrap(),
        register: &|_| {},
        pack_budget: &margin_service::retrieval::pack_budget,
    };
    let outcome = run_gates(&env, &gates, &baseline, true).await.unwrap();
    assert!(
        outcome.violations.is_empty(),
        "the eval gates failed:\n{}",
        outcome.violations.join("\n")
    );
}
