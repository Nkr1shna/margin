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
//! Documents and where their bytes are:
//! - an uploaded source (`Origin::Source`) or a file of an attached folder, which phase 4
//!   uploads as a source with a `workspace_id` (`Origin::FolderFile`): the document's id
//!   is the source's, and the file is the source's (`AppState::source_dir`);
//! - a folder file with no source row, or an agent's document (`Origin::Agent`, written
//!   by `ingest_text`): `document_dir(id)/file`;
//! - a lesson, note or chat (`Origin::Library`): its text, read from the store.
//!
//! A source's `SourceStatus` follows its document's: Processing while it is read, Ready
//! with its passage count, Failed with the reason.
//!
//! A passage's `chunk_hash` is the hash of what its vector was made from (the embedding
//! title and the text; a picture's or a sound's bytes and text), plus `-2`, `-3`, … for
//! a repeat inside one document. A stored passage whose hash and embedder version match
//! a new one is kept with its vector; only its place in the document (order, lines,
//! page, unit, scope, access) is rewritten when that moved.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::{DateTime, Utc};
use margin_embed::{
    CHUNK_TOKENS, Chunk, Embedder, Extracted, FLUIDAUDIOCLI, ImageTokens, Page, Purpose,
    SourceKind, Transcript, audio_windows, sample_ms,
};
use margin_shared::context::{
    ACCESS_COMPANY, ContextChanges, ContextIngest, Document, DocumentStatus, LibraryKind, Modality,
    Origin, TraceKind, TraceRow,
};
use margin_shared::{Chat, ChatItemKind, Event, Id, Note, Source, SourceStatus};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::chats::StoredChatItem;
use crate::db::{self, INSERT_MANY_MAX, PassageRow};
use crate::embedding;
use crate::library;
use crate::pipeline::{Chunker, ChunkerCfg, Registry, StageCtor};
use crate::sources;
use crate::state::{AppState, SETTINGS_ID, SOURCE_ORIGINAL, StoredLesson, new_id, sha256_hex};

/// The migration that moves sources from `chunk`/`media` to `passage`, by its name in
/// `SettingsRow.migrations`.
pub const PASSAGES_MIGRATION: &str = "passages-v1";

/// The embedding model and how its document inputs are prefixed
/// (`margin_embed::EMBEDDER_VERSION`: EmbeddingGemma 2 with `title: {section path or
/// document name} | text: …`). Stored on every passage; a passage embedded under
/// another version is embedded again at its next ingest.
pub const EMBEDDER_VERSION: &str = margin_embed::EMBEDDER_VERSION;

/// Who wrote a document when no agent did.
pub const ACTOR_OWNER: &str = "owner";

/// The folder (in the data folder) of documents that aren't sources.
pub const DOCUMENTS_DIR: &str = "documents";

/// Progress is saved and sent at most this often.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

/// What a source whose file is gone says after the migration couldn't read it again.
pub const FILE_GONE: &str = "Its file is missing on the service, so it couldn't be read \
     into the new search index. Read it again once the file is back, or upload it again.";

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

/// A deleted document, for the change feed (`changes_since`). Stored in
/// `db::DOCUMENT_DELETED` under the document's id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tombstone {
    pub id: Id,
    pub generation: u64,
    pub deleted_at: DateTime<Utc>,
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
    pub fn push(&self, job: IngestJob) {
        // Fails only once the worker is gone, at shutdown.
        let _ = self.tx.send(job);
    }

    /// Asks the worker to try the migration again (the embedding model just arrived).
    pub fn kick(&self) {
        self.push(IngestJob {
            document_id: Id::new(),
        });
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

// ---------------------------------------------------------------------------
// Documents
// ---------------------------------------------------------------------------

/// A stored document.
pub async fn document(state: &AppState, id: &str) -> anyhow::Result<Option<Document>> {
    state.store.get(db::DOCUMENT, id).await
}

/// Where a document that isn't a source keeps its file.
pub fn document_dir(state: &AppState, id: &str) -> PathBuf {
    state.config.data_dir.join(DOCUMENTS_DIR).join(id)
}

/// "source", "folder", "library" or "agent": `PassageRow.origin`.
pub fn origin_name(origin: &Origin) -> &'static str {
    match origin {
        Origin::Source { .. } => "source",
        Origin::FolderFile { .. } => "folder",
        Origin::Library { .. } => "library",
        Origin::Agent { .. } => "agent",
    }
}

/// `agent:<name>` for an agent's document, else `owner`.
pub fn actor_for(origin: &Origin) -> String {
    match origin {
        Origin::Agent { name } => format!("agent:{name}"),
        _ => ACTOR_OWNER.to_string(),
    }
}

fn new_document(id: Id, project_id: Id, origin: Origin, name: &str, kind: SourceKind) -> Document {
    let now = Utc::now();
    Document {
        id,
        project_id,
        actor: actor_for(&origin),
        origin,
        name: name.to_string(),
        kind,
        status: DocumentStatus::Indexing { done: 0, total: 0 },
        generation: 0,
        bytes: 0,
        access: vec![ACCESS_COMPANY.to_string()],
        created_at: now,
        updated_at: now,
    }
}

/// The document of `origin`, found by its origin (and, for an agent's, its name).
async fn find_by_origin(
    state: &AppState,
    origin: &Origin,
    name: &str,
) -> anyhow::Result<Option<Document>> {
    if let Origin::Source { source_id } = origin {
        return document(state, source_id).await;
    }
    let mut sql = "SELECT VALUE doc FROM document WHERE doc.origin = $origin".to_string();
    let mut binds = vec![("origin", serde_json::to_value(origin)?)];
    if matches!(origin, Origin::Agent { .. }) {
        sql.push_str(" AND doc.name = $name");
        binds.push(("name", serde_json::json!(name)));
    }
    let found: Vec<Document> = state.store.query_rows(&sql, binds).await?;
    Ok(found.into_iter().next())
}

/// Creates (or finds) the document for `origin`.
pub async fn upsert_document(
    state: &AppState,
    origin: Origin,
    name: &str,
    kind: SourceKind,
    actor: &str,
) -> anyhow::Result<Document> {
    let project = library::default_project(state).await?;
    let _w = state.write_lock.lock().await;
    upsert_document_locked(state, &project.id, origin, name, kind, actor).await
}

/// `upsert_document` with the write lock held.
async fn upsert_document_locked(
    state: &AppState,
    project_id: &str,
    origin: Origin,
    name: &str,
    kind: SourceKind,
    actor: &str,
) -> anyhow::Result<Document> {
    let doc = match find_by_origin(state, &origin, name).await? {
        Some(mut doc) => {
            if doc.name != name || doc.kind != kind || doc.actor != actor || doc.origin != origin {
                doc.name = name.to_string();
                doc.kind = kind;
                doc.actor = actor.to_string();
                doc.origin = origin;
                doc.updated_at = Utc::now();
                state.store.put(db::DOCUMENT, &doc.id.clone(), &doc).await?;
            }
            doc
        }
        None => {
            let id = match &origin {
                Origin::Source { source_id } => source_id.clone(),
                _ => new_id(),
            };
            let mut doc = new_document(id, project_id.to_string(), origin, name, kind);
            doc.actor = actor.to_string();
            state.store.put(db::DOCUMENT, &doc.id.clone(), &doc).await?;
            doc
        }
    };
    Ok(doc)
}

/// The origin of a source's document: a folder file when it came from an attached
/// folder, else an uploaded source.
pub fn source_origin(source: &Source) -> Origin {
    match &source.workspace_id {
        Some(workspace_id) => Origin::FolderFile {
            workspace_id: workspace_id.clone(),
            path: source.name.clone(),
        },
        None => Origin::Source {
            source_id: source.id.clone(),
        },
    }
}

/// Creates or updates the document of a source (same id). Call with the write lock held.
pub async fn source_document_locked(
    state: &AppState,
    project_id: &str,
    source: &Source,
) -> anyhow::Result<Document> {
    let origin = source_origin(source);
    match document(state, &source.id).await? {
        Some(mut doc) => {
            if doc.name != source.name
                || doc.kind != source.kind
                || doc.origin != origin
                || doc.bytes != source.bytes
            {
                doc.name = source.name.clone();
                doc.kind = source.kind;
                doc.origin = origin;
                doc.bytes = source.bytes;
                doc.updated_at = Utc::now();
                state.store.put(db::DOCUMENT, &source.id, &doc).await?;
            }
            Ok(doc)
        }
        None => {
            let mut doc = new_document(
                source.id.clone(),
                project_id.to_string(),
                origin,
                &source.name,
                source.kind,
            );
            doc.bytes = source.bytes;
            doc.created_at = source.created_at;
            state.store.put(db::DOCUMENT, &source.id, &doc).await?;
            Ok(doc)
        }
    }
}

/// Makes sure a source has its document and queues it to be read. Sources that no
/// longer exist are skipped.
pub async fn queue_source(state: &AppState, source_id: &str) -> anyhow::Result<()> {
    let project = library::default_project(state).await?;
    {
        let _w = state.write_lock.lock().await;
        let Some(source) = state.store.get::<Source>(db::SOURCE, source_id).await? else {
            return Ok(());
        };
        source_document_locked(state, &project.id, &source).await?;
    }
    state.ingest_queue.push(IngestJob {
        document_id: source_id.to_string(),
    });
    Ok(())
}

/// Adds an agent's text document (`routes::CONTEXT_INGEST`): the text is kept as the
/// document's file, the document is created (or found again by its agent and name) with
/// `actor: agent:<name>` and queued. Only `Origin::Agent` documents come in as text.
pub async fn ingest_text(state: &AppState, req: &ContextIngest) -> anyhow::Result<Document> {
    anyhow::ensure!(
        matches!(req.origin, Origin::Agent { .. }),
        "only an agent's document can be added as text"
    );
    let name = req.name.trim();
    anyhow::ensure!(!name.is_empty(), "the document needs a name");
    let actor = actor_for(&req.origin);
    let project = library::default_project(state).await?;
    let doc = {
        let _w = state.write_lock.lock().await;
        let mut doc = upsert_document_locked(
            state,
            &project.id,
            req.origin.clone(),
            name,
            req.kind,
            &actor,
        )
        .await?;
        let dir = document_dir(state, &doc.id);
        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("couldn't create {}", dir.display()))?;
        tokio::fs::write(dir.join(sources::FILE_NAME), req.text.as_bytes())
            .await
            .with_context(|| format!("couldn't save {}", dir.display()))?;
        doc.bytes = req.text.len() as u64;
        doc.status = DocumentStatus::Indexing { done: 0, total: 0 };
        doc.updated_at = Utc::now();
        state.store.put(db::DOCUMENT, &doc.id.clone(), &doc).await?;
        doc
    };
    state.ingest_queue.push(IngestJob {
        document_id: doc.id.clone(),
    });
    Ok(doc)
}

/// Indexes a lesson, note or chat transcript as a document (`Origin::Library`), so what
/// is written in the GUI is context for agents. Queues it; returns the document.
pub async fn index_library(
    state: &AppState,
    kind: LibraryKind,
    id: &str,
) -> anyhow::Result<Document> {
    let name = match library_text(state, kind, id).await? {
        Some((name, _, _)) => name,
        None => anyhow::bail!("there's no {kind:?} {id} to index"),
    };
    let origin = Origin::Library {
        kind,
        id: id.to_string(),
    };
    let doc = upsert_document(state, origin, &name, SourceKind::Text, ACTOR_OWNER).await?;
    {
        let _w = state.write_lock.lock().await;
        if let Some(mut d) = document(state, &doc.id).await? {
            d.status = DocumentStatus::Indexing { done: 0, total: 0 };
            state.store.put(db::DOCUMENT, &d.id.clone(), &d).await?;
        }
    }
    state.ingest_queue.push(IngestJob {
        document_id: doc.id.clone(),
    });
    Ok(doc)
}

/// Indexes a library item again after it changed (a lesson written or edited, a note,
/// a chat turn finished), in the background: what the owner writes in the app is
/// context for agents (`docs/plan-dogfood.md` §1). Nothing for an item without text.
/// Unchanged passages keep their vectors (the ingest diff), so a re-index is cheap.
pub fn spawn_reindex(state: &AppState, kind: LibraryKind, id: &str) {
    let (state, id) = (state.clone(), id.to_string());
    tokio::spawn(async move {
        let result = async {
            match library_text(&state, kind, &id).await? {
                Some((_, text, _)) if !text.trim().is_empty() => {
                    index_library(&state, kind, &id).await.map(|_| ())
                }
                _ => Ok(()),
            }
        };
        if let Err(e) = result.await {
            tracing::warn!("couldn't index {kind:?} {id}: {e:#}");
        }
    });
}

/// A library item's name, text and scope ids; `None` when it is gone.
async fn library_text(
    state: &AppState,
    kind: LibraryKind,
    id: &str,
) -> anyhow::Result<Option<(String, String, Vec<String>)>> {
    Ok(match kind {
        LibraryKind::Lesson => state
            .store
            .get::<StoredLesson>(db::LESSON, id)
            .await?
            .map(|l| {
                let l = l.lesson;
                let text = format!("{}\n\n{}\n\n{}", l.title, l.summary, l.body_md);
                (l.title, text, vec![l.topic_id])
            }),
        LibraryKind::Note => state.store.get::<Note>(db::NOTE, id).await?.map(|n| {
            let mut text = format!("{}\n\n{}", n.title, n.body_md);
            if let Some(q) = &n.quote {
                text.push_str(&format!("\n\n{q}"));
            }
            (n.title, text, vec![n.topic_id])
        }),
        LibraryKind::Chat => match state.store.get::<Chat>(db::CHAT, id).await? {
            None => None,
            Some(chat) => {
                let mut items: Vec<StoredChatItem> =
                    state.store.list_where(db::CHAT_ITEM, "chat_id", id).await?;
                items.sort_by_key(|i| i.item.seq);
                let text = items
                    .iter()
                    .filter_map(|i| match &i.item.kind {
                        ChatItemKind::UserMessage { text, .. } => Some(format!("Owner: {text}")),
                        ChatItemKind::AssistantText { text } => Some(format!("Margin: {text}")),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n");
                Some((chat.title, text, vec![chat.id]))
            }
        },
    })
}

/// Deletes a document's passages and row and leaves a tombstone for the change feed.
/// Call with the write lock held. Nothing happens for a document that doesn't exist.
pub async fn delete_document_locked(state: &AppState, id: &str) -> anyhow::Result<()> {
    state.store.delete_document_passages(id).await?;
    if document(state, id).await?.is_some() {
        state.store.delete(db::DOCUMENT, id).await?;
        let tomb = Tombstone {
            id: id.to_string(),
            generation: state.store.next_generation().await?,
            deleted_at: Utc::now(),
        };
        state.store.put(db::DOCUMENT_DELETED, id, &tomb).await?;
        state.retrieval_cache.invalidate(id);
    }
    // A file kept for a document that isn't a source.
    sources::remove_dir(&document_dir(state, id)).await;
    Ok(())
}

/// Deletes the document of a library item, if it was indexed. Call with the write lock
/// held.
pub async fn delete_library_document_locked(
    state: &AppState,
    kind: LibraryKind,
    id: &str,
) -> anyhow::Result<()> {
    let origin = Origin::Library {
        kind,
        id: id.to_string(),
    };
    if let Some(doc) = find_by_origin(state, &origin, "").await? {
        delete_document_locked(state, &doc.id).await?;
    }
    Ok(())
}

/// Makes a document's passages searchable from `scope_id` too (a topic it was filed
/// under, a chat it was attached to): the generation moves, so caches see it. Call with
/// the write lock held; returns the event to emit, `None` when there's no document.
pub async fn add_scope_locked(
    state: &AppState,
    document_id: &str,
    scope_id: &str,
) -> anyhow::Result<Option<Event>> {
    rescope_locked(state, document_id, scope_id, true).await
}

/// The reverse of `add_scope_locked` (a chat no longer has the file).
pub async fn remove_scope_locked(
    state: &AppState,
    document_id: &str,
    scope_id: &str,
) -> anyhow::Result<Option<Event>> {
    rescope_locked(state, document_id, scope_id, false).await
}

async fn rescope_locked(
    state: &AppState,
    document_id: &str,
    scope_id: &str,
    add: bool,
) -> anyhow::Result<Option<Event>> {
    let Some(mut doc) = document(state, document_id).await? else {
        return Ok(None);
    };
    if add {
        state.store.add_passage_scope(document_id, scope_id).await?;
    } else {
        state
            .store
            .remove_passage_scope(document_id, scope_id)
            .await?;
    }
    doc.generation = state.store.next_generation().await?;
    doc.updated_at = Utc::now();
    state.store.put(db::DOCUMENT, document_id, &doc).await?;
    state.retrieval_cache.invalidate(document_id);
    Ok(Some(Event::DocumentChanged { document: doc }))
}

/// Every document whose generation passed `since`, and those deleted since then.
pub async fn changes_since(state: &AppState, since: u64) -> anyhow::Result<ContextChanges> {
    changes_page(state, since, usize::MAX).await
}

/// One page of the change feed (`routes::CONTEXT_CHANGES`): the first `limit` changes
/// (documents and deletions) after `since`, by generation; `more` when others follow,
/// and then `generation` is the page's last, so the next page starts after it.
pub async fn changes_page(
    state: &AppState,
    since: u64,
    limit: usize,
) -> anyhow::Result<ContextChanges> {
    // Writers bump the counter and then put the row in a separate statement, both under
    // the write lock. Reading under it too means no generation is handed out whose row
    // isn't written yet, so the cursor never passes a document the feed didn't list.
    let limit = limit.max(1);
    // One more than the page, to know whether more follow.
    let fetch = limit.saturating_add(1).min(i64::MAX as usize);
    let _w = state.write_lock.lock().await;
    let generation = state.store.generation().await?;
    let rows: Vec<serde_json::Value> = state
        .store
        .query_rows(
            "SELECT doc, doc.generation AS g FROM document WHERE doc.generation > $since \
             ORDER BY g LIMIT $n",
            vec![
                ("since", serde_json::json!(since)),
                ("n", serde_json::json!(fetch)),
            ],
        )
        .await?;
    let mut changed: Vec<Document> = rows
        .into_iter()
        .map(|mut r| serde_json::from_value(r["doc"].take()))
        .collect::<Result<_, _>>()?;
    changed.sort_by_key(|d| d.generation);
    let rows: Vec<serde_json::Value> = state
        .store
        .query_rows(
            "SELECT doc, doc.generation AS g FROM document_deleted WHERE doc.generation > $since \
             ORDER BY g LIMIT $n",
            vec![
                ("since", serde_json::json!(since)),
                ("n", serde_json::json!(fetch)),
            ],
        )
        .await?;
    let mut deleted: Vec<Tombstone> = rows
        .into_iter()
        .map(|mut r| serde_json::from_value(r["doc"].take()))
        .collect::<Result<_, _>>()?;
    deleted.sort_by_key(|t| t.generation);
    // Merged by generation, the page is the first `limit`.
    let mut gens: Vec<u64> = changed
        .iter()
        .map(|d| d.generation)
        .chain(deleted.iter().map(|t| t.generation))
        .collect();
    gens.sort_unstable();
    if gens.len() > limit {
        let last = gens[limit - 1];
        changed.retain(|d| d.generation <= last);
        deleted.retain(|t| t.generation <= last);
        return Ok(ContextChanges {
            generation: last,
            changed,
            deleted: deleted.into_iter().map(|t| t.id).collect(),
            more: true,
        });
    }
    // Never behind what it lists, nor behind what the caller has seen.
    let generation = changed
        .iter()
        .map(|d| d.generation)
        .chain(deleted.iter().map(|t| t.generation))
        .fold(generation.max(since), u64::max);
    Ok(ContextChanges {
        generation,
        changed,
        deleted: deleted.into_iter().map(|t| t.id).collect(),
        more: false,
    })
}

/// What may search a document's passages: the topic and chats of its source, its
/// folder. Library items and agents' documents have none (only `Scope::Library`).
async fn scope_ids(state: &AppState, doc: &Document) -> anyhow::Result<Vec<String>> {
    let mut ids = Vec::new();
    match &doc.origin {
        Origin::Source { .. } | Origin::FolderFile { .. } => {
            if let Origin::FolderFile { workspace_id, .. } = &doc.origin {
                ids.push(workspace_id.clone());
            }
            if let Some(source) = state.store.get::<Source>(db::SOURCE, &doc.id).await? {
                ids.extend(source.topic_id);
                ids.extend(source.workspace_id);
                let chats: Vec<Chat> = state.store.list(db::CHAT).await?;
                ids.extend(
                    chats
                        .into_iter()
                        .filter(|c| c.source_ids.contains(&doc.id))
                        .map(|c| c.id),
                );
            }
        }
        // Lessons, notes and chats are found through the whole library (the context
        // API's `Scope::Library`), not from a topic or a chat: a lesson isn't material
        // for another lesson of its course, nor a chat's transcript for that chat.
        Origin::Library { .. } => {}
        Origin::Agent { .. } => {}
    }
    let mut seen = HashSet::new();
    ids.retain(|id| seen.insert(id.clone()));
    Ok(ids)
}

// ---------------------------------------------------------------------------
// Chunking and diffing
// ---------------------------------------------------------------------------

/// Chunks `doc` with the pipeline's chunker.
pub fn chunk_with(
    chunker: &dyn Chunker,
    doc: &Extracted,
    kind: SourceKind,
    tok: &dyn Fn(&str) -> usize,
) -> Vec<Chunk> {
    let cfg = ChunkerCfg {
        max_tokens: CHUNK_TOKENS,
        params: Default::default(),
    };
    let mut chunks = chunker.chunk(doc, kind, tok, &cfg);
    fill_hashes(&mut chunks);
    chunks
}

/// Gives chunks from chunkers that don't set `hash` the hash of their section path and
/// text.
pub fn fill_hashes(chunks: &mut [Chunk]) {
    for c in chunks.iter_mut().filter(|c| c.hash.is_empty()) {
        c.hash = sha256_hex(&format!("{}\n{}", c.section_path, c.text));
    }
}

/// Compares stored passages with new chunks by `chunk_hash` and embedder version.
pub fn diff(stored: Vec<PassageRow>, chunks: Vec<Chunk>, embedder: &str) -> Diff {
    let (kept, new, gone) = split(stored, chunks, |c| c.hash.as_str(), embedder);
    Diff {
        kept: kept.into_iter().map(|(row, _)| row).collect(),
        new,
        gone,
    }
}

/// Pairs each new thing with the stored row of the same key (and embedder), if any:
/// (kept pairs, new, gone rows).
fn split<T>(
    stored: Vec<PassageRow>,
    new: Vec<T>,
    key: impl Fn(&T) -> &str,
    embedder: &str,
) -> (Vec<(PassageRow, T)>, Vec<T>, Vec<PassageRow>) {
    let mut gone = Vec::new();
    let mut by_hash: HashMap<String, PassageRow> = HashMap::new();
    for row in stored {
        if row.embedder == embedder && !by_hash.contains_key(&row.chunk_hash) {
            by_hash.insert(row.chunk_hash.clone(), row);
        } else {
            gone.push(row);
        }
    }
    let mut kept = Vec::new();
    let mut fresh = Vec::new();
    for item in new {
        match by_hash.remove(key(&item)) {
            Some(row) => kept.push((row, item)),
            None => fresh.push(item),
        }
    }
    let mut rest: Vec<PassageRow> = by_hash.into_values().collect();
    rest.sort_by_key(|r| r.ord);
    gone.extend(rest);
    (kept, fresh, gone)
}

/// Splits `gone` rows into those really gone and the ids of those a new item replaces
/// under the same id (same chunk hash, another embedder version): their vectors are
/// rewritten in place instead.
fn stale_rows(gone: Vec<PassageRow>, new: &[Item]) -> (Vec<PassageRow>, HashSet<String>) {
    let new_ids: HashSet<&str> = new.iter().map(|i| i.row.id.as_str()).collect();
    let mut revectored = HashSet::new();
    let mut really_gone = Vec::new();
    for row in gone {
        if new_ids.contains(row.id.as_str()) && revectored.insert(row.id.clone()) {
            continue;
        }
        really_gone.push(row);
    }
    (really_gone, revectored)
}

/// The chunker `sections-v2` (§2.1, `margin_embed::chunk_sections`): Markdown/reST/
/// HTML/LaTeX headings, section paths, line ranges, sentence overlap, atomic tables and
/// fences. Params: `target_tokens` (default `margin_embed::CHUNK_TARGET_TOKENS`); the
/// hard cap is the stage config's `max_tokens`.
pub struct SectionsV2;

/// Registers this file's stages: the default chunker name now.
pub fn register(reg: &mut Registry) {
    reg.register(
        "sections-v2",
        StageCtor::Chunker(Arc::new(|_, _| Ok(Arc::new(SectionsV2)))),
    );
}

impl Chunker for SectionsV2 {
    fn name(&self) -> &'static str {
        "sections-v2"
    }
    fn chunk(
        &self,
        doc: &Extracted,
        kind: SourceKind,
        tok: &dyn Fn(&str) -> usize,
        cfg: &ChunkerCfg,
    ) -> Vec<Chunk> {
        let target = cfg
            .params
            .get("target_tokens")
            .and_then(serde_json::Value::as_u64)
            .map_or(margin_embed::CHUNK_TARGET_TOKENS, |t| t as usize);
        let sections = margin_embed::SectionsCfg {
            max_tokens: cfg.max_tokens,
            target_tokens: target.clamp(1, cfg.max_tokens.max(1)),
        };
        let mut chunks = margin_embed::chunk_sections(doc, kind, tok, &sections);
        fill_hashes(&mut chunks);
        chunks
    }
}

// ---------------------------------------------------------------------------
// Preparing a document's passages
// ---------------------------------------------------------------------------

/// How a passage's vector is made.
#[derive(Debug, Clone)]
enum Input {
    /// The text with `title` in the document prompt.
    Text {
        title: String,
    },
    Image {
        path: PathBuf,
        tokens: ImageTokens,
    },
    /// Samples [from, to) of the document's sound.
    Audio {
        from: usize,
        to: usize,
    },
}

/// A passage before it has a vector.
#[derive(Debug, Clone)]
struct Item {
    row: PassageRow,
    input: Input,
}

/// Everything read from a document.
struct Prepared {
    items: Vec<Item>,
    pages: Option<u32>,
    warning: Option<String>,
    sound: Option<Arc<Vec<f32>>>,
}

/// Where a document's content comes from.
enum Content {
    File {
        path: PathBuf,
        media_dir: PathBuf,
        work_dir: PathBuf,
    },
    Text(String),
}

async fn content(state: &AppState, doc: &Document) -> anyhow::Result<Content> {
    if let Origin::Library { kind, id } = &doc.origin {
        return match library_text(state, *kind, id).await? {
            Some((_, text, _)) => Ok(Content::Text(text)),
            None => anyhow::bail!("It was deleted."),
        };
    }
    let is_source = state
        .store
        .get::<Source>(db::SOURCE, &doc.id)
        .await?
        .is_some();
    let (dir, media_dir) = if is_source {
        (state.source_dir(&doc.id), state.source_media_dir(&doc.id))
    } else {
        let dir = document_dir(state, &doc.id);
        let media = dir.join(crate::state::SOURCE_MEDIA_DIR);
        (dir, media)
    };
    if !dir.join(sources::FILE_NAME).is_file() {
        anyhow::bail!("{FILE_GONE}");
    }
    let path = sources::named_link(&dir, &doc.name)
        .await
        .map_err(|e| anyhow::anyhow!("couldn't prepare the file: {e}"))?;
    Ok(Content::File {
        path,
        media_dir,
        work_dir: dir,
    })
}

/// Reads, transcribes and chunks a document into passages without vectors.
async fn prepare(
    state: &AppState,
    doc: &Document,
    embedder: &Arc<dyn Embedder>,
    chunker: Arc<dyn Chunker>,
    chunker_cfg: ChunkerCfg,
) -> anyhow::Result<Prepared> {
    let mut kind = doc.kind;
    let mut transcript: Option<Transcript> = None;
    let mut no_transcript: Option<String> = None;
    let (extracted, original) = match content(state, doc).await? {
        Content::Text(text) => {
            kind = SourceKind::Text;
            let extracted = Extracted {
                pages: vec![Page { number: None, text }],
                ..Default::default()
            };
            (extracted, None)
        }
        Content::File {
            path,
            media_dir,
            work_dir,
        } => {
            // Pages and frames of a run the service stopped in the middle of.
            sources::remove_dir(&media_dir).await;
            let (tools, from, to) = (state.source_tools.clone(), path.clone(), media_dir);
            // PDF pages aren't rendered while reading; each page is drawn once here for its
            // picture passage (the encoder reads the picture) and kept in the media folder
            // for retrieval and citations.
            let extracted = tokio::task::spawn_blocking(move || {
                let mut doc = tools.extract_lazy(&from, kind, &to)?;
                if kind == SourceKind::Pdf && doc.pictures.is_empty() {
                    for n in doc
                        .pages
                        .iter()
                        .filter_map(|p| p.number)
                        .collect::<Vec<_>>()
                    {
                        let path = tools.pdf_page_picture(&from, n, &to)?;
                        doc.pictures.push(margin_embed::Picture {
                            path,
                            page: Some(n),
                            at_ms: None,
                            tokens: ImageTokens::High,
                        });
                    }
                }
                Ok::<_, margin_embed::EmbedError>(doc)
            })
            .await??;
            if let Some(sound) = &extracted.sound {
                let (tools, samples, work) =
                    (state.source_tools.clone(), sound.clone(), work_dir.clone());
                match tokio::task::spawn_blocking(move || tools.transcribe(&samples, &work)).await?
                {
                    Ok(Some(t)) => transcript = Some(t),
                    Ok(None) => {
                        no_transcript = Some(format!(
                            "{FLUIDAUDIOCLI} isn't installed on the service (Settings → \
                             Tools shows how to install it)"
                        ))
                    }
                    Err(e) => no_transcript = Some(e.to_string()),
                }
                if let Some(why) = &no_transcript {
                    tracing::warn!("document {}: no transcript: {why}", doc.id);
                }
            }
            // Kept for the app's whole-transcript view; a read without one drops an old one.
            let transcript_path = work_dir.join(sources::TRANSCRIPT_FILE);
            match &transcript {
                Some(t) => {
                    let json = serde_json::to_vec(t)?;
                    tokio::fs::write(&transcript_path, json)
                        .await
                        .with_context(|| format!("couldn't save {}", transcript_path.display()))?;
                }
                None => {
                    let _ = tokio::fs::remove_file(&transcript_path).await;
                }
            }
            (extracted, Some(path))
        }
    };
    let pages = extracted.pages.iter().filter_map(|p| p.number).max();
    let warning = no_transcript.map(|why| sources::transcript_warning(kind, &why));
    let sound = extracted.sound.clone().map(Arc::new);

    let (tools, counter, doc_c) = (state.source_tools.clone(), embedder.clone(), doc.clone());
    let sound_c = sound.clone();
    let items = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Item>> {
        let count = |t: &str| counter.count_tokens(t);
        let mut chunks = tools.chunk(&*chunker, &extracted, kind, &count, &chunker_cfg);
        if let Some(t) = &transcript {
            chunks.extend(t.chunks(&count, chunker_cfg.max_tokens));
        }
        fill_hashes(&mut chunks);
        items_of(
            &doc_c,
            kind,
            chunks,
            &extracted,
            original.as_deref(),
            sound_c.as_deref().map(|v| v.as_slice()),
            transcript.as_ref(),
        )
    })
    .await??;
    Ok(Prepared {
        items,
        pages,
        warning,
        sound,
    })
}

fn bytes_hash(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("couldn't read {}", path.display()))?;
    use sha2::Digest;
    Ok(hex::encode(sha2::Sha256::digest(&bytes)))
}

fn samples_hash(samples: &[f32]) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    for s in samples {
        h.update(s.to_le_bytes());
    }
    hex::encode(h.finalize())
}

/// The passages of a document: its text chunks, then its pictures and sound windows
/// (each with the words said meanwhile), with hashes, order, units and locators. Scope,
/// access and project are filled in by the caller. Blocking (reads pictures).
fn items_of(
    doc: &Document,
    kind: SourceKind,
    chunks: Vec<Chunk>,
    extracted: &Extracted,
    original: Option<&Path>,
    sound: Option<&[f32]>,
    transcript: Option<&Transcript>,
) -> anyhow::Result<Vec<Item>> {
    let id = &doc.id;
    let row = |modality: Modality, text: String| PassageRow {
        id: String::new(),
        project_id: doc.project_id.clone(),
        document_id: id.clone(),
        origin: origin_name(&doc.origin).to_string(),
        scope_ids: Vec::new(),
        access: doc.access.clone(),
        unit_id: String::new(),
        modality: modality.as_str().to_string(),
        ord: 0,
        page: None,
        start_line: None,
        end_line: None,
        start_ms: None,
        end_ms: None,
        section_path: String::new(),
        text,
        file: None,
        chunk_hash: String::new(),
        embedder: EMBEDDER_VERSION.to_string(),
        vector: Vec::new(),
    };
    let mut items = Vec::new();
    for c in chunks {
        if c.text.trim().is_empty() {
            continue;
        }
        // As `margin_embed::chunk_title` (R1-embed): the section path, else the name.
        let title = if c.section_path.trim().is_empty() {
            doc.name.trim().to_string()
        } else {
            c.section_path.clone()
        };
        let hash = sha256_hex(&format!("text\0{title}\0{}", c.text));
        let unit_id = match (kind, c.page, c.start_ms) {
            (SourceKind::Image, ..) => format!("{id}:image"),
            (_, Some(n), _) => format!("{id}:page:{n}"),
            (_, _, Some(ms)) => format!("{id}:said:{ms}"),
            _ => format!("{id}:text:{}", &hash[..16]),
        };
        let mut r = row(Modality::Text, c.text);
        r.page = c.page.map(i64::from);
        r.start_ms = c.start_ms.map(|v| v as i64);
        r.end_ms = c.end_ms.map(|v| v as i64);
        r.start_line = c.start_line.map(i64::from);
        r.end_line = c.end_line.map(i64::from);
        r.section_path = c.section_path;
        r.unit_id = unit_id;
        r.chunk_hash = hash;
        items.push(Item {
            row: r,
            input: Input::Text { title },
        });
    }

    let said = |start: u64, end: u64| transcript.map(|t| t.span(start, end)).unwrap_or_default();
    let page_text: HashMap<u32, String> = extracted
        .pages
        .iter()
        .filter_map(|p| Some((p.number?, p.text.trim().to_string())))
        .collect();
    let all_text = extracted
        .pages
        .iter()
        .map(|p| p.text.trim())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    // Frames stand for the stretch of video around them.
    let pictures = &extracted.pictures;
    let frame_stretch = pictures
        .windows(2)
        .find_map(|w| Some(w[1].at_ms? - w[0].at_ms?))
        .unwrap_or(margin_embed::FRAME_INTERVAL_MS);
    for p in pictures {
        let (modality, file, unit_id, text) = match (kind, p.page, p.at_ms) {
            (SourceKind::Image, ..) => (
                Modality::Picture,
                SOURCE_ORIGINAL.to_string(),
                format!("{id}:image"),
                all_text.clone(),
            ),
            (_, Some(n), _) => (
                Modality::Picture,
                margin_embed::page_file(n),
                format!("{id}:page:{n}"),
                page_text.get(&n).cloned().unwrap_or_default(),
            ),
            (_, _, Some(ms)) => (
                Modality::Frame,
                margin_embed::frame_file(ms),
                format!("{id}:frame:{ms}"),
                said(ms.saturating_sub(frame_stretch / 2), ms + frame_stretch / 2),
            ),
            _ => continue,
        };
        // An image document's picture is the uploaded file itself.
        let path = match (kind, original) {
            (SourceKind::Image, Some(original)) => original.to_path_buf(),
            _ => p.path.clone(),
        };
        let hash = sha256_hex(&format!(
            "{}\0{file}\0{text}\0{:?}\0{}",
            modality.as_str(),
            p.tokens,
            bytes_hash(&p.path)?
        ));
        let mut r = row(modality, text);
        r.page = p.page.map(i64::from);
        // A frame is cited at its moment (the viewer starts the video there).
        r.start_ms = p.at_ms.map(|v| v as i64);
        r.file = Some(file);
        r.unit_id = unit_id;
        r.chunk_hash = hash;
        items.push(Item {
            row: r,
            input: Input::Image {
                path,
                tokens: p.tokens,
            },
        });
    }
    if let Some(samples) = sound {
        for (start, window) in audio_windows(samples) {
            let (from, to) = (sample_ms(start), sample_ms(start + window.len()));
            let text = said(from, to);
            let hash = sha256_hex(&format!(
                "sound\0{from}\0{to}\0{text}\0{}",
                samples_hash(window)
            ));
            let mut r = row(Modality::Sound, text);
            r.start_ms = Some(from as i64);
            r.end_ms = Some(to as i64);
            r.unit_id = format!("{id}:sound:{from}");
            r.chunk_hash = hash;
            items.push(Item {
                row: r,
                input: Input::Audio {
                    from: start,
                    to: start + window.len(),
                },
            });
        }
    }

    // Repeats inside one document get their own hashes; order and ids follow.
    let mut seen: HashMap<String, u32> = HashMap::new();
    for (ord, item) in items.iter_mut().enumerate() {
        let n = seen.entry(item.row.chunk_hash.clone()).or_insert(0);
        *n += 1;
        if *n > 1 {
            item.row.chunk_hash = format!("{}-{n}", item.row.chunk_hash);
        }
        item.row.ord = ord as i64;
        item.row.id = format!("{id}:{}", item.row.chunk_hash);
    }
    Ok(items)
}

/// Whether a kept row's place in the document (or scope, access, …) moved.
fn moved(stored: &PassageRow, now: &PassageRow) -> bool {
    stored.project_id != now.project_id
        || stored.origin != now.origin
        || stored.scope_ids != now.scope_ids
        || stored.access != now.access
        || stored.unit_id != now.unit_id
        || stored.modality != now.modality
        || stored.ord != now.ord
        || stored.page != now.page
        || stored.start_line != now.start_line
        || stored.end_line != now.end_line
        || stored.start_ms != now.start_ms
        || stored.end_ms != now.end_ms
        || stored.section_path != now.section_path
        || stored.file != now.file
}

/// Vectors for one batch of items, consecutive items of one kind (and title, or picture
/// size) going to the model together. Blocking.
fn embed_batch(
    embedder: &dyn Embedder,
    batch: &[(Input, String)],
    sound: Option<&[f32]>,
) -> anyhow::Result<Vec<Vec<f32>>> {
    let same = |a: &Input, b: &Input| match (a, b) {
        (Input::Text { title: x }, Input::Text { title: y }) => x == y,
        (Input::Image { tokens: x, .. }, Input::Image { tokens: y, .. }) => x == y,
        (Input::Audio { .. }, Input::Audio { .. }) => true,
        _ => false,
    };
    let mut vectors = Vec::with_capacity(batch.len());
    let mut start = 0;
    while start < batch.len() {
        let n = batch[start..]
            .iter()
            .take_while(|(i, _)| same(i, &batch[start].0))
            .count();
        let group = &batch[start..start + n];
        start += n;
        let got = match &group[0].0 {
            Input::Text { title } => {
                let texts: Vec<String> = group.iter().map(|(_, t)| t.clone()).collect();
                embedder.embed(
                    &texts,
                    Purpose::Document {
                        title: Some(title.clone()),
                    },
                )?
            }
            Input::Image { tokens, .. } => {
                let paths: Vec<PathBuf> = group
                    .iter()
                    .filter_map(|(i, _)| match i {
                        Input::Image { path, .. } => Some(path.clone()),
                        _ => None,
                    })
                    .collect();
                embedder.embed_images(&paths, *tokens)?
            }
            Input::Audio { .. } => {
                let samples = sound.unwrap_or_default();
                let windows: Vec<Vec<f32>> = group
                    .iter()
                    .filter_map(|(i, _)| match i {
                        Input::Audio { from, to } => Some(
                            samples[(*from).min(samples.len())..(*to).min(samples.len())].to_vec(),
                        ),
                        _ => None,
                    })
                    .collect();
                embedder.embed_audio(&windows)?
            }
        };
        anyhow::ensure!(
            got.len() == group.len(),
            "The embedding model returned {} vectors for {} inputs.",
            got.len(),
            group.len()
        );
        vectors.extend(got);
    }
    Ok(vectors)
}

/// Vectors for every input, handed to `on_batch(index of the batch's first input,
/// vectors)` at most `batch` at a time, in order: runs of text through
/// `Embedder::embed_pipelined` (each with its own title), pictures and sound through
/// `embed_batch`. An error from `on_batch` stops the work. Blocking.
fn embed_all(
    embedder: &dyn Embedder,
    inputs: &[(Input, String)],
    sound: Option<&[f32]>,
    batch: usize,
    on_batch: &mut dyn FnMut(usize, Vec<Vec<f32>>) -> Result<(), margin_embed::EmbedError>,
) -> anyhow::Result<()> {
    let batch = batch.max(1);
    let is_text = |i: &Input| matches!(i, Input::Text { .. });
    let mut start = 0;
    while start < inputs.len() {
        let text = is_text(&inputs[start].0);
        let end = start
            + inputs[start..]
                .iter()
                .take_while(|(i, _)| is_text(i) == text)
                .count();
        if text {
            let texts: Vec<(String, Purpose)> = inputs[start..end]
                .iter()
                .map(|(input, t)| {
                    let title = match input {
                        Input::Text { title } => Some(title.as_str()),
                        _ => None,
                    };
                    (t.clone(), Purpose::document(title))
                })
                .collect();
            let mut n = 0;
            embedder.embed_pipelined(&texts, batch, &mut |first, vectors| {
                n += vectors.len();
                on_batch(start + first, vectors)
            })?;
            anyhow::ensure!(
                n == end - start,
                "The embedding model returned {n} vectors for {} inputs.",
                end - start
            );
        } else {
            for (k, group) in inputs[start..end].chunks(batch).enumerate() {
                let vectors = embed_batch(embedder, group, sound)?;
                on_batch(start + k * batch, vectors)?;
            }
        }
        start = end;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Ingest
// ---------------------------------------------------------------------------

/// Status changes of a document (and of its source, when it has one), saved and sent.
/// Returns false when the document is gone (its source, if any, is still updated).
async fn set_status(
    state: &AppState,
    id: &str,
    status: DocumentStatus,
    source_status: Option<SourceStatus>,
) -> anyhow::Result<bool> {
    let (doc, source) = {
        let _w = state.write_lock.lock().await;
        let doc = match document(state, id).await? {
            Some(mut doc) => {
                doc.status = status;
                doc.updated_at = Utc::now();
                state.store.put(db::DOCUMENT, id, &doc).await?;
                Some(doc)
            }
            None => None,
        };
        let source = match source_status {
            // A folder file replaced while this read ran: the new copy waits, Queued.
            Some(_) if was_replaced(state, id) => None,
            Some(st) => match state.store.get::<Source>(db::SOURCE, id).await? {
                Some(mut s) => {
                    s.status = st;
                    state.store.put(db::SOURCE, id, &s).await?;
                    Some(s)
                }
                None => None,
            },
            None => None,
        };
        (doc, source)
    };
    if let Some(s) = &source {
        state.emit_source(s);
        refresh_folder(state, s).await;
    }
    let Some(doc) = doc else {
        return Ok(false);
    };
    if !matches!(doc.status, DocumentStatus::Indexing { .. }) {
        state.emit(Event::DocumentChanged { document: doc });
    }
    Ok(true)
}

/// Whether the folder source `id` was replaced by a new upload while it was being read
/// (`sources::create_or_replace`): the read finishing then leaves its status alone.
fn was_replaced(state: &AppState, id: &str) -> bool {
    state
        .sources_replaced
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(id)
}

/// A folder source that is now read (or failed): its folder's counts change.
async fn refresh_folder(state: &AppState, source: &Source) {
    if let Some(ws) = &source.workspace_id
        && matches!(
            source.status,
            SourceStatus::Ready { .. } | SourceStatus::Failed { .. }
        )
    {
        crate::folders::refresh_sync_logged(state, ws).await;
    }
}

/// Marks a document (and its source) failed with `error`.
pub async fn fail(state: &AppState, id: &str, error: &str) -> anyhow::Result<()> {
    tracing::warn!("document {id} failed: {error}");
    set_status(
        state,
        id,
        DocumentStatus::Failed {
            error: error.to_string(),
        },
        Some(SourceStatus::Failed {
            error: error.to_string(),
        }),
    )
    .await?;
    Ok(())
}

/// Runs `f` under the write lock if the document still exists; `None` when it is gone
/// (so an ingest stops as soon as its document is deleted).
async fn while_exists<T>(
    state: &AppState,
    id: &str,
    f: impl AsyncFnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<Option<T>> {
    let _w = state.write_lock.lock().await;
    if document(state, id).await?.is_none() {
        return Ok(None);
    }
    f().await.map(Some)
}

/// Progress of a document being embedded, saved and sent at most every
/// `PROGRESS_INTERVAL`.
struct Progress {
    done: u32,
    total: u32,
    last: Instant,
}

impl Progress {
    /// Counts `n` more done. False when the document is gone.
    async fn add(&mut self, state: &AppState, id: &str, n: usize) -> anyhow::Result<bool> {
        self.done += n as u32;
        if self.done >= self.total || self.last.elapsed() < PROGRESS_INTERVAL {
            return Ok(true);
        }
        self.last = Instant::now();
        let (done, total) = (self.done, self.total);
        set_status(
            state,
            id,
            DocumentStatus::Indexing { done, total },
            Some(SourceStatus::Processing { done, total }),
        )
        .await
    }
}

/// Indexes one document: everything in the module doc. Cancel-safe: a document deleted
/// meanwhile stops its ingest. On failure the document (and its source) is marked
/// Failed with the reason, which is also returned.
pub async fn ingest(state: &AppState, document_id: &str) -> anyhow::Result<IngestReport> {
    let started = Instant::now();
    let at = Utc::now();
    let result = ingest_inner(state, document_id).await;
    let doc = document(state, document_id).await.ok().flatten();
    if let Err(e) = &result
        && doc.is_some()
    {
        let _ = fail(state, document_id, &e.to_string()).await;
    }
    let pipeline = state.pipeline.load();
    let mut row = TraceRow::new(TraceKind::Ingest, ulid::Ulid::new().to_string(), at);
    row.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
    row.ok = result.is_ok();
    row.error_code = result.as_ref().err().map(|_| "ingest_failed".to_string());
    row.trace_id = format!("ingest:{document_id}");
    row.document_id = Some(document_id.to_string());
    row.project_id = doc.as_ref().map(|d| d.project_id.clone());
    row.actor = doc.as_ref().map(|d| d.actor.clone());
    row.pipeline_hash = Some(pipeline.hash.clone());
    if let Ok(r) = &result {
        row.chunks_new = Some(r.chunks_new);
        row.chunks_kept = Some(r.chunks_kept);
        row.chunks_deleted = Some(r.chunks_deleted);
        row.embed_ms = Some(r.embed_ms as f64);
    }
    state.tracer.record(row);
    result
}

async fn ingest_inner(state: &AppState, id: &str) -> anyhow::Result<IngestReport> {
    let mut report = IngestReport::default();
    let Some(doc) = document(state, id).await? else {
        return Ok(report);
    };
    let Some(embedder) = embedding::embedder(state).await? else {
        anyhow::bail!("{}", margin_embed::EmbedError::NotDownloaded);
    };
    if !set_status(
        state,
        id,
        DocumentStatus::Indexing { done: 0, total: 0 },
        Some(SourceStatus::Processing { done: 0, total: 0 }),
    )
    .await?
    {
        return Ok(report);
    }
    let pipeline = state.pipeline.load_full();
    let chunker_cfg = ChunkerCfg {
        max_tokens: CHUNK_TOKENS,
        params: pipeline.config.chunker.params.clone(),
    };
    let prepared = prepare(
        state,
        &doc,
        &embedder,
        pipeline.chunker.clone(),
        chunker_cfg,
    )
    .await?;
    if prepared.items.is_empty() {
        let error = match doc.kind {
            SourceKind::Text | SourceKind::Code | SourceKind::Pdf => {
                "There's no text in this file."
            }
            _ => "There's no picture or sound in this file.",
        };
        anyhow::bail!("{error}");
    }
    // The document as it is now (access, project) and what may search it.
    let Some(doc) = document(state, id).await? else {
        return Ok(report);
    };
    let scope = scope_ids(state, &doc).await?;
    let mut items = prepared.items;
    for item in &mut items {
        item.row.scope_ids = scope.clone();
        item.row.access = doc.access.clone();
        item.row.project_id = doc.project_id.clone();
        // The backend that embeds them (`Embedder::version`), not the Core ML constant.
        item.row.embedder = embedder.version().to_string();
    }
    let passages = items.len() as u32;

    let stored = state.store.passages_of(id).await?;
    let (kept, new, gone) = split(
        stored,
        items,
        |i| i.row.chunk_hash.as_str(),
        embedder.version(),
    );
    // Rows of another embedder version whose chunk is unchanged (same hash, so same id)
    // get their vector rewritten in place (`Store::rewrite_vectors`) rather than being
    // deleted and inserted again: a re-embed never takes them out of the index.
    let (gone, revectored) = stale_rows(gone, &new);
    report.chunks_kept = kept.len() as u32;
    report.chunks_new = new.len() as u32;
    report.chunks_deleted = gone.len() as u32;
    let moved_rows: Vec<PassageRow> = kept
        .iter()
        .filter(|(stored, item)| moved(stored, &item.row))
        .map(|(stored, item)| PassageRow {
            id: stored.id.clone(),
            vector: Vec::new(),
            ..item.row.clone()
        })
        .collect();
    let total = new.len() as u32;
    if !set_status(
        state,
        id,
        DocumentStatus::Indexing { done: 0, total },
        Some(SourceStatus::Processing { done: 0, total }),
    )
    .await?
    {
        return Ok(report);
    }

    // What is gone, then what moved, a statement at a time.
    let gone_ids: Vec<String> = gone.into_iter().map(|r| r.id).collect();
    for ids in gone_ids.chunks(INSERT_MANY_MAX) {
        let store = &state.store;
        if while_exists(state, id, async || store.delete_passages(id, ids).await)
            .await?
            .is_none()
        {
            return Ok(report);
        }
    }
    for rows in moved_rows.chunks(INSERT_MANY_MAX) {
        let store = &state.store;
        if while_exists(state, id, async || store.update_passages(rows).await)
            .await?
            .is_none()
        {
            return Ok(report);
        }
    }

    // The new ones, embedded on a blocking thread and written as they come, so the model
    // works on the next batch while this one is written (`Embedder::embed_pipelined`
    // for text; pictures and sound a batch at a time).
    let inputs: Vec<(Input, String)> = new
        .iter()
        .map(|i| (i.input.clone(), i.row.text.clone()))
        .collect();
    let (tx, mut rx) = mpsc::channel::<(usize, Vec<Vec<f32>>)>(1);
    let (model, sound) = (embedder.clone(), prepared.sound.clone());
    let embedding = tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
        let started = Instant::now();
        let mut waited = Duration::ZERO;
        embed_all(
            &*model,
            &inputs,
            sound.as_deref().map(|s| s.as_slice()),
            INSERT_MANY_MAX,
            &mut |first, vectors| {
                let t = Instant::now();
                let sent = tx.blocking_send((first, vectors));
                waited += t.elapsed();
                sent.map_err(|_| margin_embed::EmbedError::Embed("the ingest stopped".into()))
            },
        )?;
        Ok(started.elapsed().saturating_sub(waited).as_millis() as u64)
    });
    let mut progress = Progress {
        done: 0,
        total,
        last: Instant::now(),
    };
    let mut written_count = 0usize;
    while let Some((first, vectors)) = rx.recv().await {
        anyhow::ensure!(
            first == written_count && first + vectors.len() <= new.len(),
            "The embedding model returned vectors out of order."
        );
        let rows: Vec<PassageRow> = new[first..first + vectors.len()]
            .iter()
            .zip(vectors)
            .map(|(item, vector)| PassageRow {
                vector,
                ..item.row.clone()
            })
            .collect();
        written_count += rows.len();
        let count = rows.len();
        let (rewrite, insert): (Vec<PassageRow>, Vec<PassageRow>) =
            rows.into_iter().partition(|r| revectored.contains(&r.id));
        let store = &state.store;
        let written = while_exists(state, id, async || {
            store.rewrite_vectors(&rewrite).await?;
            store.insert_many(db::PASSAGE, &insert).await
        })
        .await?;
        let rows = count;
        if written.is_none() || !progress.add(state, id, rows).await? {
            // The thread stops at its next batch, its channel being closed.
            drop(rx);
            let _ = embedding.await;
            return Ok(report);
        }
    }
    report.embed_ms = embedding.await??;
    anyhow::ensure!(
        written_count == new.len(),
        "The embedding model returned {written_count} vectors for {} inputs.",
        new.len()
    );

    // Done: a new generation when anything changed (or on the first read).
    let changed = report.chunks_new > 0 || report.chunks_deleted > 0 || !moved_rows.is_empty();
    let (doc, source) = {
        let _w = state.write_lock.lock().await;
        let Some(mut doc) = document(state, id).await? else {
            return Ok(report);
        };
        if changed || doc.generation == 0 {
            doc.generation = state.store.next_generation().await?;
        }
        doc.status = DocumentStatus::Ready { passages };
        doc.updated_at = Utc::now();
        state.store.put(db::DOCUMENT, id, &doc).await?;
        let source = match state.store.get::<Source>(db::SOURCE, id).await? {
            // Replaced meanwhile: the new copy is read next.
            Some(_) if was_replaced(state, id) => None,
            Some(mut s) => {
                s.status = SourceStatus::Ready {
                    chunks: passages,
                    pages: prepared.pages,
                    warning: prepared.warning.clone(),
                };
                state.store.put(db::SOURCE, id, &s).await?;
                Some(s)
            }
            None => None,
        };
        (doc, source)
    };
    if changed {
        state.retrieval_cache.invalidate(id);
    }
    if let Some(s) = &source {
        state.emit_source(s);
        refresh_folder(state, s).await;
    }
    state.emit(Event::DocumentChanged { document: doc });
    tracing::info!(
        "document {id} is indexed: {} new, {} kept, {} gone",
        report.chunks_new,
        report.chunks_kept,
        report.chunks_deleted
    );
    Ok(report)
}

// ---------------------------------------------------------------------------
// The worker and the migration
// ---------------------------------------------------------------------------

/// The worker that drains `IngestQueue`: first the migration (when not done yet), then
/// one document at a time, chat and topic files ahead of folder files.
pub async fn run_worker(state: AppState) {
    let Some(mut rx) = state.ingest_queue.take_receiver() else {
        return;
    };
    if let Err(e) = library::default_project(&state).await {
        tracing::error!("couldn't create the default project: {e:#}");
    }
    if let Err(e) = migrate(&state).await {
        tracing::error!("the move to the passage index failed: {e:#}");
    }
    if let Err(e) = reembed(&state).await {
        tracing::error!("the re-embed with the new embedder failed: {e:#}");
    }
    let mut pending: VecDeque<Id> = VecDeque::new();
    loop {
        if pending.is_empty() {
            match rx.recv().await {
                Some(job) => pending.push_back(job.document_id),
                None => return,
            }
        }
        while let Ok(job) = rx.try_recv() {
            if !pending.contains(&job.document_id) {
                pending.push_back(job.document_id);
            }
        }
        let id = match next_job(&state, &mut pending).await {
            Ok(Some(id)) => id,
            Ok(None) => continue,
            Err(e) => {
                tracing::error!("couldn't pick the next document to read: {e:#}");
                continue;
            }
        };
        if id.is_empty() {
            if let Err(e) = migrate(&state).await {
                tracing::error!("the move to the passage index failed: {e:#}");
            }
            if let Err(e) = reembed(&state).await {
                tracing::error!("the re-embed with the new embedder failed: {e:#}");
            }
            continue;
        }
        if let Err(e) = run_job(&state, &id).await {
            tracing::error!("document {id}: {e:#}");
        }
    }
}

/// The next job: a migration request first, then anything that isn't a folder file,
/// then folder files in order.
async fn next_job(state: &AppState, pending: &mut VecDeque<Id>) -> anyhow::Result<Option<Id>> {
    if let Some(i) = pending.iter().position(String::is_empty) {
        return Ok(pending.remove(i));
    }
    for i in 0..pending.len() {
        let is_folder = matches!(
            document(state, &pending[i]).await?,
            Some(Document {
                origin: Origin::FolderFile { .. },
                ..
            })
        );
        if !is_folder {
            return Ok(pending.remove(i));
        }
    }
    Ok(pending.pop_front())
}

/// Reads one queued document, if it still waits to be read and the model is there
/// (otherwise it stays queued; the finished download queues it again).
async fn run_job(state: &AppState, id: &str) -> anyhow::Result<()> {
    let Some(doc) = document(state, id).await? else {
        return Ok(());
    };
    // The read a replacement waited behind is over (the worker runs one at a time).
    state
        .sources_replaced
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(id);
    match state.store.get::<Source>(db::SOURCE, id).await? {
        Some(source) if source.status != SourceStatus::Queued => return Ok(()),
        Some(_) => {}
        None if !matches!(doc.status, DocumentStatus::Indexing { .. }) => return Ok(()),
        None => {}
    }
    match embedding::embedder(state).await {
        Ok(Some(_)) => {}
        Ok(None) => return Ok(()),
        Err(e) => return fail(state, id, &e.to_string()).await,
    }
    ingest(state, id).await.map(|_| ())
}

/// Queues every document that waits to be read and isn't a source (sources are queued
/// by `sources::enqueue_queued`), and asks for the migration again. Called once the
/// embedding model is downloaded.
pub async fn enqueue_waiting(state: &AppState) -> anyhow::Result<()> {
    let docs: Vec<Document> = state.store.list(db::DOCUMENT).await?;
    for d in docs {
        if matches!(d.status, DocumentStatus::Indexing { .. })
            && state
                .store
                .get::<Source>(db::SOURCE, &d.id)
                .await?
                .is_none()
        {
            state.ingest_queue.push(IngestJob { document_id: d.id });
        }
    }
    state.ingest_queue.kick();
    Ok(())
}

async fn migration_done(state: &AppState) -> anyhow::Result<bool> {
    Ok(state
        .settings_row()
        .await?
        .migrations
        .iter()
        .any(|m| m == PASSAGES_MIGRATION))
}

/// Re-ingests every Ready source into `passage` rows in one pass, oldest first, then
/// drops `chunk`/`media` and records `PASSAGES_MIGRATION` (§2.2 "Migration").
///
/// Needs the embedding model: without it the pass waits (sources keep their old rows,
/// which retrieval falls back to) and runs once the download finishes. A source whose
/// file is gone is marked Failed (`FILE_GONE`) and the owner can read it again.
pub async fn migrate(state: &AppState) -> anyhow::Result<()> {
    if migration_done(state).await? {
        return Ok(());
    }
    let mut sources: Vec<Source> = state.store.list(db::SOURCE).await?;
    sources.sort_by_key(|s| s.created_at);
    let mut todo = Vec::new();
    for s in sources {
        if matches!(s.status, SourceStatus::Ready { .. })
            && !state.store.has_passages(&s.id).await?
        {
            todo.push(s);
        }
    }
    if !todo.is_empty() {
        match embedding::embedder(state).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                tracing::info!(
                    "{} source(s) wait for the embedding model to move to the passage index",
                    todo.len()
                );
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
        tracing::info!("moving {} source(s) to the passage index", todo.len());
        let project = library::default_project(state).await?;
        for s in todo {
            {
                let _w = state.write_lock.lock().await;
                let Some(source) = state.store.get::<Source>(db::SOURCE, &s.id).await? else {
                    continue;
                };
                source_document_locked(state, &project.id, &source).await?;
            }
            if let Err(e) = ingest(state, &s.id).await {
                tracing::warn!("source {} couldn't be moved: {e:#}", s.id);
            }
            sources::delete_old_rows(state, &s.id).await?;
        }
    }
    state.store.remove_table(db::CHUNK).await?;
    state.store.remove_table(db::MEDIA).await?;
    let _w = state.write_lock.lock().await;
    let mut row = state.settings_row().await?;
    if !row.migrations.iter().any(|m| m == PASSAGES_MIGRATION) {
        row.migrations.push(PASSAGES_MIGRATION.to_string());
        state.store.put(db::SETTINGS, SETTINGS_ID, &row).await?;
    }
    tracing::info!("every source is in the passage index");
    Ok(())
}

/// The prefix of the `SettingsRow.migrations` entry that records which embedder version
/// the passages were last brought to: `embedder-<version>`. One entry at a time, so
/// switching back to an earlier backend runs the pass again.
pub const EMBEDDER_MIGRATION_PREFIX: &str = "embedder-";

/// The migration entry for `version`.
pub fn embedder_migration(version: &str) -> String {
    format!("{EMBEDDER_MIGRATION_PREFIX}{version}")
}

/// The one-pass re-embed when the embedder changes (`docs/plan-linux-service.md` §2.1,
/// "Switch = re-embed, one pass"): at the first start with another `embedder` than the
/// passages were made with, every Ready document holding passages of another version
/// is read again, oldest first. Unchanged chunks keep their rows and get new vectors
/// (`Store::rewrite_vectors`); text that changed (another OCR engine) is a new chunk.
/// Until a document is done, the dense leg skips its old vectors (`embedder =
/// $embedder`) and BM25 still finds it.
///
/// Needs the model: without it the pass waits and runs when the download finishes
/// (`IngestQueue::kick`). Recorded in `SettingsRow.migrations` as `embedder-<version>`.
pub async fn reembed(state: &AppState) -> anyhow::Result<()> {
    let configured = state.model_files.backend.embedder_version();
    let marker = embedder_migration(configured);
    if state.settings_row().await?.migrations.contains(&marker) {
        return Ok(());
    }
    if !state
        .store
        .documents_with_other_embedder(configured)
        .await?
        .is_empty()
    {
        let embedder = match embedding::embedder(state).await {
            Ok(Some(e)) => e,
            Ok(None) => {
                tracing::info!("the re-embed with {configured} waits for the embedding model");
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        // What the loaded model stamps (a test fake may differ from the backend's).
        let version = embedder.version().to_string();
        let stale = state.store.documents_with_other_embedder(&version).await?;
        let mut docs: Vec<Document> = Vec::new();
        for doc_id in &stale {
            if let Some(d) = document(state, doc_id).await?
                && matches!(d.status, DocumentStatus::Ready { .. })
            {
                docs.push(d);
            }
        }
        docs.sort_by_key(|d| d.created_at);
        let total = docs.len();
        if total > 0 {
            tracing::info!("re-embedding {total} document(s) with {version}");
        }
        for (i, d) in docs.iter().enumerate() {
            tracing::info!("re-indexing {} of {total} documents ({})", i + 1, d.name);
            if let Err(e) = ingest(state, &d.id).await {
                tracing::warn!("document {} couldn't be re-embedded: {e:#}", d.id);
            }
        }
        if total > 0 {
            tracing::info!("every document is embedded with {version}");
        }
    }
    let _w = state.write_lock.lock().await;
    let mut row = state.settings_row().await?;
    row.migrations
        .retain(|m| !m.starts_with(EMBEDDER_MIGRATION_PREFIX));
    row.migrations.push(marker);
    state.store.put(db::SETTINGS, SETTINGS_ID, &row).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_rows_are_rewritten_not_deleted() {
        let item = |id: &str| Item {
            row: PassageRow {
                id: id.into(),
                ..stored(id, "h", 0, EMBEDDER_VERSION)
            },
            input: Input::Text {
                title: String::new(),
            },
        };
        let gone = vec![
            stored("d:h1", "h1", 0, "older-embedder"),
            stored("d:h2", "h2", 1, "older-embedder"),
        ];
        let (really_gone, revectored) = stale_rows(gone, &[item("d:h1")]);
        assert_eq!(
            really_gone
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["d:h2"]
        );
        assert_eq!(revectored.into_iter().collect::<Vec<_>>(), ["d:h1"]);
        assert_eq!(
            embedder_migration("embeddinggemma-2-onnx-q8/title-prefix-v1"),
            "embedder-embeddinggemma-2-onnx-q8/title-prefix-v1"
        );
    }

    fn stored(id: &str, hash: &str, ord: i64, embedder: &str) -> PassageRow {
        PassageRow {
            id: id.into(),
            project_id: "p".into(),
            document_id: "d".into(),
            origin: "source".into(),
            scope_ids: Vec::new(),
            access: vec![ACCESS_COMPANY.into()],
            unit_id: id.into(),
            modality: "text".into(),
            ord,
            page: None,
            start_line: None,
            end_line: None,
            start_ms: None,
            end_ms: None,
            section_path: String::new(),
            text: id.into(),
            file: None,
            chunk_hash: hash.into(),
            embedder: embedder.into(),
            vector: vec![0.0; 4],
        }
    }

    fn chunk(text: &str) -> Chunk {
        let mut c = vec![Chunk::new(text, None)];
        fill_hashes(&mut c);
        c.pop().unwrap()
    }

    #[test]
    fn diff_keeps_matching_hashes_embeds_new_ones_and_drops_the_rest() {
        let a = chunk("alpha");
        let b = chunk("beta");
        let c = chunk("gamma");
        let rows = vec![
            stored("ra", &a.hash, 0, EMBEDDER_VERSION),
            stored("rb", &b.hash, 1, "older-embedder"),
            stored("rx", "gone-hash", 2, EMBEDDER_VERSION),
        ];
        let d = diff(
            rows,
            vec![a.clone(), b.clone(), c.clone()],
            EMBEDDER_VERSION,
        );
        assert_eq!(
            d.kept.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["ra"]
        );
        // Another embedder version is embedded again.
        assert_eq!(d.new, vec![b, c]);
        let mut gone: Vec<&str> = d.gone.iter().map(|r| r.id.as_str()).collect();
        gone.sort();
        assert_eq!(gone, ["rb", "rx"]);
        // Nothing stored: everything is new.
        let d = diff(Vec::new(), vec![a.clone()], EMBEDDER_VERSION);
        assert_eq!((d.kept.len(), d.new.len(), d.gone.len()), (0, 1, 0));
    }

    #[test]
    fn hashes_follow_the_text_and_the_section() {
        let mut chunks = vec![
            Chunk::new("same", None),
            Chunk {
                section_path: "Intro".into(),
                ..Chunk::new("same", None)
            },
            Chunk {
                hash: "set-by-the-chunker".into(),
                ..Chunk::new("x", None)
            },
        ];
        fill_hashes(&mut chunks);
        assert_ne!(chunks[0].hash, chunks[1].hash);
        assert_eq!(chunks[0].hash, chunk("same").hash);
        assert_eq!(chunks[2].hash, "set-by-the-chunker");
    }

    fn doc(kind: SourceKind) -> Document {
        new_document(
            "d1".into(),
            "p1".into(),
            Origin::Source {
                source_id: "d1".into(),
            },
            "notes.md",
            kind,
        )
    }

    #[test]
    fn items_carry_units_hashes_order_and_titles() {
        let d = doc(SourceKind::Text);
        let chunks = vec![
            Chunk {
                section_path: "Waves > Fourier".into(),
                start_line: Some(3),
                end_line: Some(5),
                ..Chunk::new("A sum of sines.", None)
            },
            Chunk::new("A sum of sines.", None),
            Chunk::new("A sum of sines.", None),
        ];
        let items = items_of(
            &d,
            SourceKind::Text,
            chunks,
            &Extracted::default(),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(
            items.iter().map(|i| i.row.ord).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        // The section path is the title; without one, the document's name.
        assert!(matches!(&items[0].input, Input::Text { title } if title == "Waves > Fourier"));
        assert!(matches!(&items[1].input, Input::Text { title } if title == "notes.md"));
        assert_eq!(items[0].row.start_line, Some(3));
        assert_eq!(items[0].row.section_path, "Waves > Fourier");
        // Same title and text: the repeat gets its own hash and id.
        assert_ne!(items[0].row.chunk_hash, items[1].row.chunk_hash);
        assert_eq!(
            items[2].row.chunk_hash,
            format!("{}-2", items[1].row.chunk_hash)
        );
        assert_eq!(items[2].row.id, format!("d1:{}", items[2].row.chunk_hash));
        assert!(items.iter().all(|i| i.row.embedder == EMBEDDER_VERSION
            && i.row.access == [ACCESS_COMPANY]
            && i.row.origin == "source"));
    }

    #[test]
    fn a_pdf_page_picture_shares_its_unit_and_text_with_the_page() {
        let dir = std::env::temp_dir().join(format!("margin-ingest-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let page = dir.join(margin_embed::page_file(2));
        std::fs::write(&page, b"png").unwrap();
        let extracted = Extracted {
            pages: vec![
                Page {
                    number: Some(1),
                    text: "one".into(),
                },
                Page {
                    number: Some(2),
                    text: " Second page text ".into(),
                },
            ],
            pictures: vec![margin_embed::Picture {
                path: page.clone(),
                page: Some(2),
                at_ms: None,
                tokens: ImageTokens::High,
            }],
            ..Default::default()
        };
        let d = doc(SourceKind::Pdf);
        let items = items_of(
            &d,
            SourceKind::Pdf,
            vec![Chunk::new("Second page text", Some(2))],
            &extracted,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].row.unit_id, "d1:page:2");
        assert_eq!(items[1].row.unit_id, "d1:page:2");
        assert_eq!(items[1].row.modality, "picture");
        assert_eq!(items[1].row.text, "Second page text");
        assert_eq!(items[1].row.file.as_deref(), Some("page-2.png"));
        // The picture's hash follows its bytes.
        let before = items[1].row.chunk_hash.clone();
        std::fs::write(&page, b"another png").unwrap();
        let again = items_of(
            &d,
            SourceKind::Pdf,
            Vec::new(),
            &extracted,
            None,
            None,
            None,
        )
        .unwrap();
        assert_ne!(again[0].row.chunk_hash, before);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn moved_rows_are_told_apart_from_unchanged_ones() {
        let a = stored("a", "h", 0, EMBEDDER_VERSION);
        assert!(!moved(&a, &a.clone()));
        let b = PassageRow {
            start_line: Some(9),
            ..a.clone()
        };
        assert!(moved(&a, &b));
        let c = PassageRow {
            access: vec!["company/engineering".into()],
            ..a.clone()
        };
        assert!(moved(&a, &c));
        // Text and vector aren't places: a change there is a new hash.
        let d = PassageRow {
            vector: vec![1.0],
            ..a.clone()
        };
        assert!(!moved(&a, &d));
    }

    #[test]
    fn embedding_batches_group_by_kind_and_title() {
        use std::sync::Mutex;
        #[derive(Default)]
        struct Counting(Mutex<Vec<String>>);
        impl Embedder for Counting {
            fn embed(
                &self,
                texts: &[String],
                purpose: Purpose,
            ) -> Result<Vec<Vec<f32>>, margin_embed::EmbedError> {
                let title = match purpose {
                    Purpose::Document { title } => title.unwrap_or_default(),
                    Purpose::Query => "query".into(),
                };
                self.0
                    .lock()
                    .unwrap()
                    .push(format!("text:{title}:{}", texts.len()));
                Ok(texts.iter().map(|_| vec![1.0]).collect())
            }
            fn count_tokens(&self, t: &str) -> usize {
                t.len()
            }
            fn embed_images(
                &self,
                images: &[PathBuf],
                _t: ImageTokens,
            ) -> Result<Vec<Vec<f32>>, margin_embed::EmbedError> {
                self.0
                    .lock()
                    .unwrap()
                    .push(format!("images:{}", images.len()));
                Ok(images.iter().map(|_| vec![2.0]).collect())
            }
            fn embed_audio(
                &self,
                windows: &[Vec<f32>],
            ) -> Result<Vec<Vec<f32>>, margin_embed::EmbedError> {
                self.0
                    .lock()
                    .unwrap()
                    .push(format!("audio:{}", windows.len()));
                Ok(windows.iter().map(|w| vec![w.len() as f32]).collect())
            }
        }
        let e = Counting::default();
        let text = |t: &str| (Input::Text { title: t.into() }, "x".to_string());
        let batch = vec![
            text("A"),
            text("A"),
            text("B"),
            (
                Input::Image {
                    path: "p".into(),
                    tokens: ImageTokens::High,
                },
                String::new(),
            ),
            (Input::Audio { from: 0, to: 2 }, String::new()),
            (Input::Audio { from: 2, to: 5 }, String::new()),
        ];
        let v = embed_batch(&e, &batch, Some(&[0.0; 5])).unwrap();
        assert_eq!(v.len(), 6);
        assert_eq!(v[4], vec![2.0]);
        assert_eq!(v[5], vec![3.0]);
        assert_eq!(
            *e.0.lock().unwrap(),
            ["text:A:2", "text:B:1", "images:1", "audio:2"]
        );
    }

    #[test]
    fn origins_name_their_passages_and_actors() {
        assert_eq!(
            origin_name(&Origin::FolderFile {
                workspace_id: "w".into(),
                path: "a.md".into()
            }),
            "folder"
        );
        assert_eq!(
            actor_for(&Origin::Agent {
                name: "claude-code".into()
            }),
            "agent:claude-code"
        );
        assert_eq!(
            actor_for(&Origin::Source {
                source_id: "s".into()
            }),
            ACTOR_OWNER
        );
    }
}
