//! Hybrid retrieval over `passage` rows (`docs/agent-rag-architecture.md` §2.2, §2.3):
//! query planning, the dense (HNSW) and lexical (BM25) legs, fusion, packing to a
//! budget, and the retrieval cache. One code path for lessons, the agent's `search`
//! tool family and the context API (`context_api.rs`).
//!
//! R1-retrieve fills the bodies; the signatures and the registered names are the seam.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use margin_shared::Id;
use margin_shared::context::{Modality, Scope};
use margin_shared::pipeline::Budget;

use crate::engines::BoxFuture;
use crate::pipeline::{
    FusionCfg, Hit, PackBudget, Packed, Packer, Query, Registry, Retriever, StageCtor,
};
use crate::state::AppState;

/// The query strings one job retrieves with (§2.3 step 1): the lesson title and
/// summary, the summary alone, the planner's sub-questions, the learner's prompt for
/// lesson 1; for the agent, its query plus an optional expansion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryPlan {
    pub queries: Vec<String>,
}

/// What a job asks retrieval for.
#[derive(Debug, Clone)]
pub struct Request {
    pub plan: QueryPlan,
    pub scope: Scope,
    /// The caller's access paths (pre-filter in the index query; `None` = no filter).
    pub access: Option<Vec<String>>,
    /// Empty means every modality.
    pub modalities: Vec<Modality>,
    /// Most passages, whatever the budget.
    pub k: usize,
    pub budget: Budget,
    /// The context window of the route's model, in tokens, which `budget` is a share of.
    pub window_tokens: usize,
    /// Correlates the retrieval's trace row with its turn or job.
    pub trace_id: String,
}

/// Retrieves, fuses, reranks and packs for `req` with the current pipeline, through the
/// cache, and records a `retrieval` trace row.
pub async fn retrieve(_state: &AppState, _req: &Request) -> anyhow::Result<Vec<Packed>> {
    unimplemented!("R1, retrieve: retrieve")
}

/// The ids a scope resolves to inside the project (topic → its sources and the chats
/// filed under it, chat → its files and folders, …). Unknown ids are an error the
/// context API answers with 400 "bad_scope".
pub async fn resolve_scope(_state: &AppState, _scope: &Scope) -> anyhow::Result<Vec<Id>> {
    unimplemented!("R1, retrieve: resolve_scope")
}

/// `PackBudget` for `budget` against a model window of `window_tokens`.
pub fn pack_budget(_budget: &Budget, _window_tokens: usize, _k: usize) -> PackBudget {
    unimplemented!("R1, retrieve: pack_budget")
}

/// Retrieval results by `hash(query strings, scope, embedder, index generation)`, an
/// in-memory LRU of `RETRIEVAL_CACHE_ENTRIES`; a document's ingest bumps its generation,
/// which invalidates every entry that read it.
pub struct RetrievalCache {
    capacity: usize,
    entries: Mutex<HashMap<String, Arc<Vec<Packed>>>>,
}

/// Entries the retrieval cache keeps.
pub const RETRIEVAL_CACHE_ENTRIES: usize = 256;

impl RetrievalCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn get(&self, _key: &str) -> Option<Arc<Vec<Packed>>> {
        let _ = &self.entries;
        unimplemented!("R1, retrieve: RetrievalCache::get")
    }

    pub fn insert(&self, _key: String, _packed: Arc<Vec<Packed>>) {
        unimplemented!("R1, retrieve: RetrievalCache::insert")
    }

    /// Drops every entry that read `document_id`.
    pub fn invalidate(&self, _document_id: &str) {
        unimplemented!("R1, retrieve: RetrievalCache::invalidate")
    }
}

/// Registers this file's stages: the default names now, the rest as R1-retrieve builds
/// them ("dense", "bm25", "linear").
pub fn register(reg: &mut Registry) {
    reg.register(
        "hybrid-rrf",
        StageCtor::Retriever(Arc::new(|_, deps| {
            Ok(Arc::new(HybridRrf {
                _store: deps.store.clone(),
            }))
        })),
    );
    reg.register("rrf", StageCtor::Fusion(Arc::new(|_, _| Ok(Arc::new(Rrf)))));
    reg.register(
        "mmr-v1",
        StageCtor::Packer(Arc::new(|_, _| Ok(Arc::new(MmrV1)))),
    );
}

/// Dense + BM25 legs fused with `search::rrf` in one SurrealQL statement (§2.2).
pub struct HybridRrf {
    _store: crate::db::Store,
}

impl Retriever for HybridRrf {
    fn name(&self) -> &'static str {
        "hybrid-rrf"
    }
    fn search<'a>(&'a self, _q: &'a Query) -> BoxFuture<'a, anyhow::Result<Vec<Hit>>> {
        unimplemented!("R1, retrieve: hybrid-rrf")
    }
}

/// Reciprocal rank fusion across query strings (max score kept per passage).
pub struct Rrf;

impl crate::pipeline::Fusion for Rrf {
    fn name(&self) -> &'static str {
        "rrf"
    }
    fn fuse(&self, _legs: Vec<Vec<Hit>>, _cfg: &FusionCfg) -> Vec<Hit> {
        unimplemented!("R1, retrieve: rrf fusion")
    }
}

/// Unit dedup, adjacent merge, MMR with a per-document cap, the score percentile and
/// the token budget (§2.3 step 4).
pub struct MmrV1;

impl Packer for MmrV1 {
    fn name(&self) -> &'static str {
        "mmr-v1"
    }
    fn pack(&self, _hits: Vec<Hit>, _budget: &PackBudget) -> Vec<Packed> {
        unimplemented!("R1, retrieve: mmr-v1 packer")
    }
}
