//! Margin's eval suite (`docs/agent-rag-architecture.md` §3). CI runs this crate's tests
//! on fakes and recorded fixtures only; the `margin-eval` binary is the opt-in runner
//! the owner runs by hand with real models and the real embedder, against its own data
//! folder, never the running service's database.

use std::collections::BTreeMap;

use serde::Deserialize;

pub mod access;
pub mod agent;
pub mod ai_judge;
pub mod alerts;
pub mod ci;
pub mod citations;
pub mod cli;
pub mod compare;
pub mod corpus;
pub mod courses;
pub mod gates;
pub mod golden;
pub mod harvest;
pub mod index;
pub mod injection;
pub mod judge;
pub mod live;
pub mod metrics;
pub mod model;
pub mod packing;
pub mod perf;
pub mod prefix;
pub mod recorded;
pub mod report;
pub mod retrieval;
pub mod runner;
pub mod small;
pub mod store;
pub mod synth;
pub mod traces;
pub mod unfiltered;

/// `evals/gates.toml`: how far a change may move the metrics before CI fails.
#[derive(Debug, Clone, Deserialize)]
pub struct Gates {
    pub retrieval: RetrievalGates,
    pub citations: CitationGates,
    pub packing: PackingGates,
    /// R2: the recorded reranks of the model-backed rerankers.
    #[serde(default)]
    pub rerank: RerankGates,
    /// R2: the prompt lock (`evals/prompts.lock`).
    #[serde(default)]
    pub prompts: PromptGates,
    /// L2: the leakage gate (`evals/access.toml`).
    #[serde(default)]
    pub access: AccessGates,
}

/// `[access]` (L2, `docs/l2-brief.md` D8): over every golden query × every principal of
/// `evals/access.toml`, by the Rust oracle.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(default)]
pub struct AccessGates {
    /// Hits a principal can't read.
    pub leaks_max: u32,
    /// By-id reads (`read_passages`, `document_outline`) that returned something for a
    /// (principal, document) pair the oracle forbids.
    pub by_id_leaks_max: u32,
    /// The owner's ranked ids equal the run without the conjuncts, to the id.
    pub owner_rank_identical: bool,
    /// For a principal who reads every relevant document of a query, the relevant hits
    /// keep the owner's relative order (the filter removes, never reorders).
    pub rank_consistent: bool,
    /// The owner's p95 over the unfiltered run's, same process.
    pub p95_latency_max_ratio: f64,
}

impl Default for AccessGates {
    fn default() -> Self {
        Self {
            leaks_max: 0,
            by_id_leaks_max: 0,
            owner_rank_identical: true,
            rank_consistent: true,
            p95_latency_max_ratio: 1.5,
        }
    }
}

/// `[rerank]`: on the recorded reranks over the golden set.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(default)]
pub struct RerankGates {
    /// Points (0–100): the mean share of the top 10 places a reranker may change. One
    /// that reorders more than this on the golden set is a bug, not a win.
    pub moved_at_10_max: f64,
    /// Points: searches the reranker applies to but left in the fused order (its
    /// fallback on a timeout, a refusal or bad JSON).
    pub fallback_rate_max: f64,
}

impl Default for RerankGates {
    fn default() -> Self {
        Self {
            moved_at_10_max: 60.0,
            fallback_rate_max: 0.0,
        }
    }
}

/// `[prompts]`.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(default)]
pub struct PromptGates {
    /// Whether a constant block changed under an unchanged version fails CI (else the
    /// lock test only prints what differs).
    pub lock: bool,
}

impl Default for PromptGates {
    fn default() -> Self {
        Self { lock: true }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RetrievalGates {
    /// Points (0–100) recall@5 may drop overall.
    pub recall_at_5_max_drop: f64,
    pub ndcg_at_10_max_drop: f64,
    /// Points any one query kind may drop.
    pub per_kind_max_drop: f64,
    /// Points the negatives' false-positive rate may rise.
    pub negative_fp_max_rise: f64,
    /// p95 latency may be at most this multiple of the baseline's.
    pub p95_latency_max_ratio: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CitationGates {
    /// Share (0–100) of `[^n]` markers that must resolve.
    pub resolution_min: f64,
    pub lexical_support_max_drop: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PackingGates {
    pub tool_output_max_bytes: usize,
}

/// `evals/prices.toml`: USD per million tokens, by model id.
#[derive(Debug, Clone, Deserialize)]
pub struct Prices {
    /// When the table was last checked against the providers' pricing pages.
    pub checked: String,
    pub models: BTreeMap<String, ModelPrice>,
}

/// One model's prices (frozen in the providers phase's first commit: P-evals reads
/// them, the service's `trace::ModelPrice` mirrors them).
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ModelPrice {
    pub provider: Provider,
    pub input: f64,
    #[serde(default)]
    pub cached_input: f64,
    #[serde(default)]
    pub cache_write: f64,
    pub output: f64,
    /// USD per 1 000 hosted web searches.
    #[serde(default)]
    pub web_search: f64,
    /// A higher tier for prompts over `over` input tokens (Claude Haiku 5.5).
    #[serde(default)]
    pub long_context: Option<LongContextPrice>,
}

/// The prices that apply when a call's input passes `over` tokens.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct LongContextPrice {
    pub over: u64,
    pub input: f64,
    pub output: f64,
}

/// Who bills a model. `Chatgpt` groups ChatGPT-plan models apart in reports (they are
/// never priced: plan-billed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Openai,
    Anthropic,
    Chatgpt,
}
