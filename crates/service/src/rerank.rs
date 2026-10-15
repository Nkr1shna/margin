//! Rerankers (R2, `docs/r2-brief.md` D1, D2, D5): `"listwise-small"`, a listwise grade
//! of the fused candidates on the `Small` route for lessons and the planner, and
//! `"onnx-crossencoder"`, a local cross-encoder (`margin_embed::Reranker`) on the CPU.
//! Neither is the default (`PipelineConfig::default()` keeps `"none"`); the eval decides.
//!
//! Every failure (no model, a timeout, a refusal, bad JSON, a missing download at call
//! time) returns the fused order unchanged; `retrieve` traces it as `rerank_failed`.
//! R2-retrieve builds this file; `register` is called from `Registry::standard` once it
//! does.

use std::sync::Arc;
use std::time::Duration;

use margin_shared::pipeline::{RetrievalPurpose, StageCfg};

use crate::engines::BoxFuture;
use crate::pipeline::{Hit, Registry, RerankRequest, Reranker, SmallModels, StageDeps};

/// The stage names.
pub const LISTWISE_SMALL: &str = "listwise-small";
pub const ONNX_CROSSENCODER: &str = "onnx-crossencoder";

/// `"listwise-small"`'s params (`StageCfg.params`).
#[derive(Debug, Clone, PartialEq)]
pub struct ListwiseParams {
    /// Purposes it reranks; others pass through untouched with no call. Default
    /// `["lesson", "plan"]`.
    pub apply_to: Vec<RetrievalPurpose>,
    /// Default `routes::RERANK_MAX_CANDIDATES`.
    pub max_candidates: usize,
    /// Drop grade-0 hits (never below `Budget.passages_min`). Default true.
    pub drop_grade_0: bool,
    /// Default 20 s.
    pub timeout: Duration,
}

/// `"onnx-crossencoder"`'s params.
#[derive(Debug, Clone, PartialEq)]
pub struct CrossEncoderParams {
    /// `margin_embed::RerankerModel::name`; default `ms-marco-minilm-l6-v2`.
    pub model: String,
    /// Default every purpose.
    pub apply_to: Vec<RetrievalPurpose>,
    pub max_candidates: usize,
}

impl ListwiseParams {
    pub fn from_cfg(cfg: &StageCfg) -> anyhow::Result<Self> {
        let _ = cfg;
        unimplemented!("R2-retrieve, ListwiseParams::from_cfg")
    }
}

impl CrossEncoderParams {
    pub fn from_cfg(cfg: &StageCfg) -> anyhow::Result<Self> {
        let _ = cfg;
        unimplemented!("R2-retrieve, CrossEncoderParams::from_cfg")
    }
}

/// The listwise reranker on a provider's small model.
pub struct ListwiseSmall {
    pub params: ListwiseParams,
    pub models: Arc<dyn SmallModels>,
    pub deps: StageDeps,
}

impl Reranker for ListwiseSmall {
    fn name(&self) -> &'static str {
        LISTWISE_SMALL
    }
    fn rerank<'a>(
        &'a self,
        req: &'a RerankRequest<'a>,
        hits: Vec<Hit>,
    ) -> BoxFuture<'a, anyhow::Result<Vec<Hit>>> {
        let _ = (req, hits);
        unimplemented!("R2-retrieve, listwise-small")
    }
}

/// The cross-encoder over `margin_embed::Reranker` (the ONNX model, or `FakeReranker`
/// in tests), run on a blocking thread.
pub struct CrossEncoder {
    pub params: CrossEncoderParams,
    pub model: Arc<dyn margin_embed::Reranker>,
}

impl Reranker for CrossEncoder {
    fn name(&self) -> &'static str {
        ONNX_CROSSENCODER
    }
    fn rerank<'a>(
        &'a self,
        req: &'a RerankRequest<'a>,
        hits: Vec<Hit>,
    ) -> BoxFuture<'a, anyhow::Result<Vec<Hit>>> {
        let _ = (req, hits);
        unimplemented!("R2-retrieve, onnx-crossencoder")
    }
}

/// D1's score rewrite: each hit in `scored` (by passage id) gets `rerank = Some(score)`
/// and `score = score + fused score min-max scaled into [0, 1)`; hits not in `scored`
/// keep their fused score and follow; the list comes back sorted by `score`
/// descending.
pub fn rescore(hits: Vec<Hit>, scored: &[(String, f32)]) -> Vec<Hit> {
    let _ = (hits, scored);
    unimplemented!("R2-retrieve, rescore")
}

/// Registers `"listwise-small"` and `"onnx-crossencoder"`. A constructor without
/// `deps.models` (or without the reranker download) fails with a message Settings
/// shows.
pub fn register(reg: &mut Registry) {
    let _ = reg;
    unimplemented!("R2-retrieve, rerank::register")
}
