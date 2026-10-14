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

/// The real pipeline over the recorded corpus against the committed baseline.
///
/// Activates (remove the `ignore`) once all of these are on the branch:
/// - R1-embed: the `sections-v2` chunker (`crates/service/src/ingest.rs` `SectionsV2`
///   calling `margin_embed`'s section chunker);
/// - R1-retrieve: the `hybrid-rrf` retriever, `rrf` fusion and `mmr-v1` packer
///   (`retrieval.rs`), `retrieval::pack_budget`, and `HybridRrf::search` resolving
///   `Scope::Topic { id }` / `Scope::Workspace { id }` as `id IN scope_ids` and
///   `Scope::Sources { ids }` as `document_id IN ids` inside the index query (the eval
///   index files the corpus under topic `eval-corpus` and the folder under workspace
///   `eval-folder`, with no topic or workspace records behind them);
/// - R1-ai: the `lesson-v2` `PromptSet` adapter in `pipeline.rs` (`LessonV2`);
/// - the owner's first run on the service Mac: `margin-eval record-embeddings`
///   (writes `evals/recorded/{embeddings.bin,extracted.json}`), `margin-eval
///   record-generations` (`evals/recorded/generations/lesson-v2/…`) and `margin-eval
///   run --suite retrieval --variant baseline --write-baseline` (`evals/baseline.json`),
///   all committed.
#[tokio::test]
#[ignore = "needs R1-embed's sections-v2, R1-retrieve's hybrid-rrf/rrf/mmr-v1 and pack_budget, R1-ai's lesson-v2 prompts, and the owner's recorded embeddings, generations and baseline (see the doc comment)"]
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
