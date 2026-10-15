//! Query rewriting (R2, `docs/r2-brief.md` D3): `"small-expand-v1"` adds up to
//! `routes::REWRITE_MAX_ADDED` phrasings to a thin plan on the `Small` route, with a
//! `RewriteCache` so a repeated agent search pays once. The prompt sees the query and
//! document names only. A failure or timeout leaves the plan as it was. Not the default
//! (`"none"`). R2-retrieve builds this file; `register` is called from
//! `Registry::standard` once it does.

use std::sync::Arc;
use std::time::Duration;

use margin_shared::pipeline::{RetrievalPurpose, StageCfg};

use crate::engines::BoxFuture;
use crate::pipeline::{Registry, RewriteRequest, Rewriter, SmallModels, StageDeps};
use crate::retrieval::QueryPlan;

/// The stage name.
pub const SMALL_EXPAND_V1: &str = "small-expand-v1";
/// Entries the rewrite cache keeps.
pub const REWRITE_CACHE_ENTRIES: usize = 1_024;

/// `"small-expand-v1"`'s params (`StageCfg.params`).
#[derive(Debug, Clone, PartialEq)]
pub struct ExpandParams {
    /// `Agent`/`Context` queries with fewer words are rewritten
    /// (`routes::REWRITE_MIN_WORDS`).
    pub min_words: usize,
    /// `routes::REWRITE_MAX_ADDED`.
    pub max_added: usize,
    /// Default every purpose; `Lesson`/`Plan` only when the plan has fewer than 3
    /// strings.
    pub apply_to: Vec<RetrievalPurpose>,
    /// Default 10 s.
    pub timeout: Duration,
}

impl ExpandParams {
    pub fn from_cfg(cfg: &StageCfg) -> anyhow::Result<Self> {
        let _ = cfg;
        unimplemented!("R2-retrieve, ExpandParams::from_cfg")
    }
}

/// LRU of rewrites keyed by `hash(rewriter cfg, prompt version, purpose, query,
/// scope_names)`.
#[derive(Default)]
pub struct RewriteCache {}

impl RewriteCache {
    pub fn get(&self, key: &str) -> Option<Vec<String>> {
        let _ = key;
        unimplemented!("R2-retrieve, RewriteCache::get")
    }
    pub fn insert(&self, key: String, added: Vec<String>) {
        let _ = (key, added);
        unimplemented!("R2-retrieve, RewriteCache::insert")
    }
}

/// The rewriter on a provider's small model.
pub struct SmallExpand {
    pub params: ExpandParams,
    pub models: Arc<dyn SmallModels>,
    pub cache: Arc<RewriteCache>,
    pub deps: StageDeps,
}

impl Rewriter for SmallExpand {
    fn name(&self) -> &'static str {
        SMALL_EXPAND_V1
    }
    fn rewrite<'a>(
        &'a self,
        req: &'a RewriteRequest<'a>,
        plan: QueryPlan,
    ) -> BoxFuture<'a, anyhow::Result<QueryPlan>> {
        let _ = (req, plan);
        unimplemented!("R2-retrieve, small-expand-v1")
    }
}

/// Registers `"small-expand-v1"`; its constructor fails without `deps.models`.
pub fn register(reg: &mut Registry) {
    let _ = reg;
    unimplemented!("R2-retrieve, rewrite::register")
}
