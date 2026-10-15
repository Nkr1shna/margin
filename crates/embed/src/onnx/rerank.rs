//! A cross-encoder reranker on ONNX Runtime, CPU execution provider (R2,
//! `docs/r2-brief.md` D5): `ms-marco-MiniLM-L6-v2` (default) or `bge-reranker-base`,
//! int8. `score` tokenises `[CLS] query [SEP] passage [SEP]` pairs (truncated to
//! `routes::RERANK_PAIR_TOKENS`, the query kept whole), runs them in batches of 8 and
//! returns one logit per pair. The session sits in an `IdleSlot` like the vision encoder
//! and is dropped after `ENCODER_IDLE`. R2-xenc builds it.

use crate::{EmbedError, OnnxOptions, Reranker, RerankerFiles, RerankerModel};

/// Pairs per run of the graph.
pub const RERANK_BATCH: usize = 8;

/// A loaded cross-encoder.
pub struct OnnxReranker {
    model: RerankerModel,
}

impl OnnxReranker {
    /// Opens the model's tokenizer and graph on the CPU EP (`PreferCPU`, the intra-op
    /// threads of `opts`). A missing download is an error naming Settings → Sources.
    pub fn load(files: &RerankerFiles, opts: &OnnxOptions) -> Result<Self, EmbedError> {
        let _ = (files, opts);
        unimplemented!("R2-xenc, OnnxReranker::load")
    }

    pub fn model(&self) -> RerankerModel {
        self.model
    }
}

impl Reranker for OnnxReranker {
    fn name(&self) -> &str {
        self.model.name()
    }

    fn score(&self, query: &str, passages: &[String]) -> Result<Vec<f32>, EmbedError> {
        let _ = (query, passages);
        unimplemented!("R2-xenc, OnnxReranker::score")
    }
}
