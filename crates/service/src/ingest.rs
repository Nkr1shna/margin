//! One ingestion pipeline for every origin (`docs/agent-rag-architecture.md` §2.1,
//! §2.7): extract → chunk → hash → diff against the stored passages (kept, new, gone)
//! → embed only what is new, pipelined → `insert_many` (≤ `db::INSERT_MANY_MAX` rows per
//! statement, the write lock per statement) → bump the document's generation →
//! `Event::DocumentChanged`. Also the one-pass migration of the old `chunk`/`media`
//! rows at first start (owner Q11).
//!
//! Every passage carries its document's `access` paths (copied here at ingest), so
//! retrieval can pre-filter by access inside the index query.
//!
//! R1-store fills the bodies; the signatures and the registered names are the seam.

use std::sync::{Arc, Mutex};

use margin_embed::{Chunk, Extracted, SourceKind};
use margin_shared::Id;
use margin_shared::context::{Document, Origin};
use tokio::sync::mpsc;

use crate::db::PassageRow;
use crate::pipeline::{Chunker, ChunkerCfg, Registry, StageCtor};
use crate::state::AppState;

/// The migration that moves sources from `chunk`/`media` to `passage`, by its name in
/// `SettingsRow.migrations`.
pub const PASSAGES_MIGRATION: &str = "passages-v1";

/// Something to (re)index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestJob {
    pub document_id: Id,
}

/// What an ingest did, for its trace row and the progress events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IngestReport {
    pub chunks_new: u32,
    pub chunks_kept: u32,
    pub chunks_deleted: u32,
    pub embed_ms: u64,
}

/// The stored passages of a document against freshly cut chunks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Diff {
    /// Stored rows whose `chunk_hash` and `embedder` match a new chunk: kept as they are.
    pub kept: Vec<PassageRow>,
    /// Chunks to embed.
    pub new: Vec<Chunk>,
    /// Stored rows no chunk matches any more.
    pub gone: Vec<PassageRow>,
}

/// Documents waiting to be indexed, one at a time (chat files ahead of folder files).
pub struct IngestQueue {
    tx: mpsc::UnboundedSender<IngestJob>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<IngestJob>>>,
}

impl IngestQueue {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            tx,
            rx: Mutex::new(Some(rx)),
        }
    }

    /// Queues a document.
    pub fn push(&self, _job: IngestJob) {
        let _ = &self.tx;
        unimplemented!("R1, store: IngestQueue::push")
    }

    /// The receiving end, for the worker; `None` once taken.
    pub fn take_receiver(&self) -> Option<mpsc::UnboundedReceiver<IngestJob>> {
        self.rx.lock().ok()?.take()
    }
}

impl Default for IngestQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Creates (or finds) the document for `origin`.
pub async fn upsert_document(
    _state: &AppState,
    _origin: Origin,
    _name: &str,
    _kind: SourceKind,
    _actor: &str,
) -> anyhow::Result<Document> {
    unimplemented!("R1, store: upsert_document")
}

/// Indexes one document: everything in the module doc. Cancel-safe: a document deleted
/// meanwhile stops its ingest.
pub async fn ingest(_state: &AppState, _document_id: &str) -> anyhow::Result<IngestReport> {
    unimplemented!("R1, store: ingest")
}

/// Compares stored passages with new chunks by `chunk_hash` and embedder version.
pub fn diff(_stored: Vec<PassageRow>, _chunks: Vec<Chunk>, _embedder: &str) -> Diff {
    unimplemented!("R1, store: diff")
}

/// Chunks `doc` with the pipeline's chunker.
pub fn chunk_with(
    _chunker: &dyn Chunker,
    _doc: &Extracted,
    _kind: SourceKind,
    _tok: &dyn Fn(&str) -> usize,
) -> Vec<Chunk> {
    unimplemented!("R1, store: chunk_with")
}

/// The worker that drains `IngestQueue`.
pub async fn run_worker(_state: AppState) {
    unimplemented!("R1, store: run_worker")
}

/// Re-ingests every Ready source into `passage` rows in one pass, oldest first, then
/// drops `chunk`/`media` and records `PASSAGES_MIGRATION` (§2.2 "Migration").
pub async fn migrate(_state: &AppState) -> anyhow::Result<()> {
    unimplemented!("R1, store: migrate")
}

/// Registers this file's stages: the default chunker name now.
pub fn register(reg: &mut Registry) {
    reg.register(
        "sections-v2",
        StageCtor::Chunker(Arc::new(|_, _| Ok(Arc::new(SectionsV2)))),
    );
}

/// The section-aware chunker of `margin_embed` (§2.1), registered here.
pub struct SectionsV2;

impl Chunker for SectionsV2 {
    fn name(&self) -> &'static str {
        "sections-v2"
    }
    fn chunk(
        &self,
        _doc: &Extracted,
        _kind: SourceKind,
        _tok: &dyn Fn(&str) -> usize,
        _cfg: &ChunkerCfg,
    ) -> Vec<Chunk> {
        unimplemented!("R1, embed: sections-v2 chunker")
    }
}
