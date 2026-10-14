//! Margin's eval suite (`docs/agent-rag-architecture.md` §3). CI runs this crate's tests
//! on fakes and recorded fixtures only; the `margin-eval` binary is the opt-in runner
//! the owner runs by hand with real models and the real embedder, against its own data
//! folder, never the running service's database.

use std::collections::BTreeMap;

use serde::Deserialize;

pub mod agent;
pub mod ai_judge;
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
pub mod judge;
pub mod live;
pub mod metrics;
pub mod model;
pub mod packing;
pub mod recorded;
pub mod report;
pub mod retrieval;
pub mod runner;
pub mod store;
pub mod synth;
pub mod traces;

/// `evals/gates.toml`: how far a change may move the metrics before CI fails.
#[derive(Debug, Clone, Deserialize)]
pub struct Gates {
    pub retrieval: RetrievalGates,
    pub citations: CitationGates,
    pub packing: PackingGates,
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

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ModelPrice {
    pub provider: Provider,
    pub input: f64,
    #[serde(default)]
    pub cached_input: f64,
    #[serde(default)]
    pub cache_write: f64,
    pub output: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Openai,
    Anthropic,
}
