//! Sources: files the learner uploads so outlines and lessons are grounded in them.
//!
//! An upload is stored as `data_dir/sources/{id}/file` and queued. One source at a time,
//! the worker extracts its text, cuts it into chunks and embeds them (Queued →
//! Processing → Ready); while the embedding model isn't downloaded sources stay Queued.
//! Retrieval scores every chunk of the sources in play against the prompt and hands the
//! best passages to the outline and lesson prompts.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::Utc;
use margin_ai::Passage;
use margin_embed::{CHUNK_TOKENS, Purpose};
use margin_shared::{Event, Id, Source, SourceKind, SourceStatus, routes::MAX_SOURCE_BYTES};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

use crate::db;
use crate::embedding;
use crate::error::{AppError, AppResult};
use crate::library;
use crate::state::{AppState, new_id};

/// The uploaded file's name inside its folder; the original name is kept in the row.
pub const FILE_NAME: &str = "file";
/// Chunks embedded per call to the model.
const EMBED_BATCH: usize = 16;
/// Progress is saved and sent at most this often.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);
/// Passages a prompt gets.
pub const PASSAGES: usize = 8;
/// How long generation waits for its sources to be read before going on without them.
pub const SOURCE_WAIT: Duration = Duration::from_secs(10 * 60);
/// Uploads never attached to a topic (an Ask that wasn't sent) are deleted at startup
/// once they're this old.
pub const UNATTACHED_TTL: chrono::Duration = chrono::Duration::hours(24);

/// One embedded piece of a source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkRow {
    pub source_id: Id,
    /// The source's topic, kept in step with it.
    pub topic_id: Option<Id>,
    /// Position in the source, from 0.
    pub ord: u32,
    pub page: Option<u32>,
    pub text: String,
    pub vector: Vec<f32>,
}

fn chunk_id(source_id: &str, ord: u32) -> String {
    format!("{source_id}-{ord}")
}

pub fn is_pending(status: &SourceStatus) -> bool {
    matches!(
        status,
        SourceStatus::Queued | SourceStatus::Processing { .. }
    )
}

pub async fn source(state: &AppState, id: &str) -> AppResult<Source> {
    state
        .store
        .get(db::SOURCE, id)
        .await?
        .ok_or_else(|| AppError::not_found("source"))
}

// ---------------------------------------------------------------------------
// Upload and delete
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct UploadQuery {
    #[serde(default)]
    name: String,
    folder: Option<String>,
    topic_id: Option<Id>,
}

pub async fn upload(
    State(state): State<AppState>,
    query: Result<Query<UploadQuery>, QueryRejection>,
    body: Result<Bytes, BytesRejection>,
) -> AppResult<Json<Source>> {
    let Query(q) =
        query.map_err(|rej| AppError::new(rej.status(), "bad_request", rej.body_text()))?;
    let bytes = body.map_err(|rej| {
        if rej.status() == StatusCode::PAYLOAD_TOO_LARGE {
            AppError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_large",
                format!(
                    "That file is too big: sources can be at most {} MB.",
                    MAX_SOURCE_BYTES / (1024 * 1024)
                ),
            )
        } else {
            AppError::new(rej.status(), "bad_request", rej.body_text())
        }
    })?;
    let name: String = q.name.trim().chars().take(500).collect();
    if name.is_empty() {
        return Err(AppError::bad_request("Say what the file is called."));
    }
    let Some(kind) = SourceKind::for_name(&name) else {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "unsupported_source",
            format!("Margin can't read {name}. Sources are text, code and PDF files."),
        ));
    };
    let folder = q
        .folder
        .map(|f| f.trim().chars().take(500).collect::<String>())
        .filter(|f| !f.is_empty());
    if let Some(topic_id) = &q.topic_id {
        library::topic(&state, topic_id).await?;
    }

    let id = new_id();
    let dir = state.source_dir(&id);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| AppError::internal(format!("couldn't create {}: {e}", dir.display())))?;
    if let Err(e) = tokio::fs::write(dir.join(FILE_NAME), &bytes).await {
        remove_dir(&dir).await;
        return Err(AppError::internal(format!(
            "couldn't save the source in {}: {e}",
            dir.display()
        )));
    }
    let source = Source {
        id: id.clone(),
        topic_id: q.topic_id,
        name,
        folder,
        kind,
        bytes: bytes.len() as u64,
        status: SourceStatus::Queued,
        created_at: Utc::now(),
    };
    {
        let _w = state.write_lock.lock().await;
        // The topic may have been deleted while the file was being saved.
        if let Some(topic_id) = &source.topic_id
            && let Err(e) = library::topic(&state, topic_id).await
        {
            remove_dir(&dir).await;
            return Err(e);
        }
        state.store.put(db::SOURCE, &id, &source).await?;
    }
    state.emit_source(&source);
    let _ = state.source_queue.send(id);
    Ok(Json(source))
}

pub async fn delete_source(
    State(state): State<AppState>,
    Path(id): Path<Id>,
) -> AppResult<StatusCode> {
    let event = {
        let _w = state.write_lock.lock().await;
        source(&state, &id).await?;
        delete_locked(&state, &id).await?
    };
    state.emit(event);
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes a source's chunks, row and file. Call with the write lock held; returns the
/// event to emit.
pub async fn delete_locked(state: &AppState, id: &str) -> anyhow::Result<Event> {
    state.store.delete_where(db::CHUNK, "source_id", id).await?;
    state.store.delete(db::SOURCE, id).await?;
    remove_dir(&state.source_dir(id)).await;
    Ok(Event::SourceDeleted { id: id.to_string() })
}

async fn remove_dir(dir: &std::path::Path) {
    match tokio::fs::remove_dir_all(dir).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("couldn't remove {}: {e}", dir.display()),
    }
}

/// Attaches unattached sources to `topic_id` (and their chunks). Call with the write lock
/// held; returns the events to emit. Sources that no longer exist or already belong to a
/// topic are left alone.
pub async fn attach_locked(
    state: &AppState,
    source_ids: &[Id],
    topic_id: &str,
) -> anyhow::Result<Vec<Event>> {
    let mut events = Vec::new();
    for id in source_ids {
        let Some(mut source) = state.store.get::<Source>(db::SOURCE, id).await? else {
            continue;
        };
        if source.topic_id.is_some() {
            continue;
        }
        source.topic_id = Some(topic_id.to_string());
        state.store.put(db::SOURCE, id, &source).await?;
        state
            .store
            .set_where(db::CHUNK, "source_id", id, "topic_id", &topic_id)
            .await?;
        events.push(Event::SourceChanged { source });
    }
    Ok(events)
}

// ---------------------------------------------------------------------------
// The queue
// ---------------------------------------------------------------------------

/// Queues every source still waiting to be read, oldest first.
pub async fn enqueue_queued(state: &AppState) -> anyhow::Result<()> {
    let mut sources: Vec<Source> = state.store.list(db::SOURCE).await?;
    sources.sort_by_key(|s| s.created_at);
    for s in sources {
        if s.status == SourceStatus::Queued {
            let _ = state.source_queue.send(s.id);
        }
    }
    Ok(())
}

/// Reads queued sources one at a time, forever.
pub async fn run_worker(state: AppState, mut queue: mpsc::UnboundedReceiver<Id>) {
    while let Some(id) = queue.recv().await {
        if let Err(e) = process(&state, &id).await {
            tracing::error!("source {id}: {e:#}");
            let _ = fail(&state, &id, &format!("{e:#}")).await;
        }
    }
}

/// Applies `f` to the stored source under the write lock and emits it. Returns false
/// when the source no longer exists.
async fn update(state: &AppState, id: &str, f: impl FnOnce(&mut Source)) -> anyhow::Result<bool> {
    let source = {
        let _w = state.write_lock.lock().await;
        let Some(mut s) = state.store.get::<Source>(db::SOURCE, id).await? else {
            return Ok(false);
        };
        f(&mut s);
        state.store.put(db::SOURCE, id, &s).await?;
        s
    };
    state.emit_source(&source);
    Ok(true)
}

async fn fail(state: &AppState, id: &str, error: &str) -> anyhow::Result<()> {
    tracing::warn!("source {id} failed: {error}");
    let error = error.to_string();
    update(state, id, move |s| {
        s.status = SourceStatus::Failed { error }
    })
    .await?;
    Ok(())
}

async fn process(state: &AppState, id: &str) -> anyhow::Result<()> {
    let Some(source) = state.store.get::<Source>(db::SOURCE, id).await? else {
        return Ok(());
    };
    if source.status != SourceStatus::Queued {
        return Ok(());
    }
    let embedder = match embedding::embedder(state).await {
        Ok(Some(e)) => e,
        // Stays Queued; the finished download queues it again.
        Ok(None) => return Ok(()),
        Err(e) => return fail(state, id, &e.to_string()).await,
    };
    if !update(state, id, |s| {
        s.status = SourceStatus::Processing { done: 0, total: 0 }
    })
    .await?
    {
        return Ok(());
    }

    let kind = source.kind;
    let path = state.source_dir(id).join(FILE_NAME);
    let tools = state.source_tools.clone();
    let doc = match tokio::task::spawn_blocking(move || tools.extract(&path, kind)).await? {
        Ok(doc) => doc,
        Err(e) => return fail(state, id, &e.to_string()).await,
    };
    let pages = doc.pages.iter().filter_map(|p| p.number).max();
    let (tools, counter) = (state.source_tools.clone(), embedder.clone());
    let chunks = tokio::task::spawn_blocking(move || {
        tools.chunk(&doc, kind, &|t| counter.count_tokens(t), CHUNK_TOKENS)
    })
    .await?;
    if chunks.is_empty() {
        return fail(state, id, "There's no text in this file.").await;
    }
    let total = chunks.len() as u32;
    if !update(state, id, |s| {
        s.status = SourceStatus::Processing { done: 0, total }
    })
    .await?
    {
        return Ok(());
    }
    // Leftovers of a run the service stopped in the middle of.
    state.store.delete_where(db::CHUNK, "source_id", id).await?;

    let mut done = 0u32;
    let mut last_progress = Instant::now();
    for batch in chunks.chunks(EMBED_BATCH) {
        let texts: Vec<String> = batch.iter().map(|c| c.text.clone()).collect();
        let model = embedder.clone();
        let vectors =
            match tokio::task::spawn_blocking(move || model.embed(&texts, Purpose::Document))
                .await?
            {
                Ok(v) => v,
                Err(e) => return fail(state, id, &e.to_string()).await,
            };
        if vectors.len() != batch.len() {
            return fail(
                state,
                id,
                &format!(
                    "The embedding model returned {} vectors for {} chunks.",
                    vectors.len(),
                    batch.len()
                ),
            )
            .await;
        }
        {
            // Under the lock, so a delete or an attach can't interleave with the write.
            let _w = state.write_lock.lock().await;
            let Some(current) = state.store.get::<Source>(db::SOURCE, id).await? else {
                return Ok(());
            };
            for (chunk, vector) in batch.iter().zip(vectors) {
                let row = ChunkRow {
                    source_id: id.to_string(),
                    topic_id: current.topic_id.clone(),
                    ord: done,
                    page: chunk.page,
                    text: chunk.text.clone(),
                    vector,
                };
                state
                    .store
                    .put(db::CHUNK, &chunk_id(id, done), &row)
                    .await?;
                done += 1;
            }
        }
        if done < total && last_progress.elapsed() >= PROGRESS_INTERVAL {
            last_progress = Instant::now();
            if !update(state, id, |s| {
                s.status = SourceStatus::Processing { done, total }
            })
            .await?
            {
                return Ok(());
            }
        }
    }
    update(state, id, |s| {
        s.status = SourceStatus::Ready {
            chunks: total,
            pages,
        }
    })
    .await?;
    tracing::info!("source {id} is ready: {total} chunks");
    Ok(())
}

// ---------------------------------------------------------------------------
// Retrieval
// ---------------------------------------------------------------------------

/// The sources a lesson draws on: its topic's, plus those its Ask named (which stay with
/// the subject when the Ask was filed under an existing topic).
pub async fn for_lesson(state: &AppState, topic_id: &str, asked: &[Id]) -> anyhow::Result<Vec<Id>> {
    let mut sources: Vec<Source> = state
        .store
        .list_where(db::SOURCE, "topic_id", topic_id)
        .await?;
    sources.sort_by_key(|s| s.created_at);
    let mut ids: Vec<Id> = sources.into_iter().map(|s| s.id).collect();
    for id in asked {
        if !ids.contains(id) {
            ids.push(id.clone());
        }
    }
    Ok(ids)
}

/// The passages of `source_ids` most relevant to `query`, best first. Sources still
/// being read are waited for (up to `SOURCE_WAIT`); after that, or when retrieval
/// fails, generation goes on with what there is.
pub async fn passages(state: &AppState, source_ids: &[Id], query: &str) -> Vec<Passage> {
    if source_ids.is_empty() {
        return Vec::new();
    }
    match retrieve(state, source_ids, query, SOURCE_WAIT).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("couldn't retrieve source passages: {e:#}");
            Vec::new()
        }
    }
}

async fn retrieve(
    state: &AppState,
    source_ids: &[Id],
    query: &str,
    wait: Duration,
) -> anyhow::Result<Vec<Passage>> {
    let sources = wait_until_read(state, source_ids, wait).await?;
    let ready: Vec<Source> = sources
        .into_iter()
        .filter(|s| matches!(s.status, SourceStatus::Ready { .. }))
        .collect();
    if ready.is_empty() {
        return Ok(Vec::new());
    }
    let Some(embedder) = embedding::embedder(state).await? else {
        tracing::warn!("the embedding model isn't downloaded; writing without sources");
        return Ok(Vec::new());
    };
    let text = query.to_string();
    let query_vec = tokio::task::spawn_blocking(move || embedder.embed(&[text], Purpose::Query))
        .await??
        .into_iter()
        .next()
        .context("the embedding model returned no vector for the query")?;

    let mut candidates: Vec<(String, ChunkRow)> = Vec::new();
    for s in &ready {
        let chunks: Vec<ChunkRow> = state
            .store
            .list_where(db::CHUNK, "source_id", &s.id)
            .await?;
        candidates.extend(chunks.into_iter().map(|c| (s.name.clone(), c)));
    }
    let tools = state.source_tools.clone();
    let query = query.to_string();
    let passages = tokio::task::spawn_blocking(move || {
        let mut scored: Vec<(f32, String, ChunkRow)> = candidates
            .into_iter()
            .map(|(name, c)| {
                let score = tools.score(&query_vec, &query, &c.vector, &c.text);
                (score, name, c)
            })
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut seen = HashSet::new();
        scored
            .into_iter()
            .filter(|(_, _, c)| {
                seen.insert(c.text.split_whitespace().collect::<Vec<_>>().join(" "))
            })
            .take(PASSAGES)
            .map(|(_, source, c)| Passage {
                source,
                page: c.page,
                text: c.text,
            })
            .collect::<Vec<_>>()
    })
    .await?;
    Ok(passages)
}

/// The sources that still exist, once none is Queued or Processing, or when `wait` runs
/// out (logged).
async fn wait_until_read(
    state: &AppState,
    source_ids: &[Id],
    wait: Duration,
) -> anyhow::Result<Vec<Source>> {
    let deadline = Instant::now() + wait;
    // Subscribed before the first look, so a change in between isn't missed.
    let mut events = state.events.subscribe();
    loop {
        let mut sources = Vec::new();
        for id in source_ids {
            if let Some(s) = state.store.get::<Source>(db::SOURCE, id).await? {
                sources.push(s);
            }
        }
        let pending = sources.iter().filter(|s| is_pending(&s.status)).count();
        if pending == 0 {
            return Ok(sources);
        }
        // Queued sources wait for the model; with none downloaded or downloading they
        // would never be read, so don't hold generation up for them.
        let processing = sources
            .iter()
            .any(|s| matches!(s.status, SourceStatus::Processing { .. }));
        if !processing && !embedding::will_be_available(state).await {
            tracing::warn!(
                "{pending} source(s) wait for the embedding model, which isn't downloaded; writing without them"
            );
            return Ok(sources);
        }
        let now = Instant::now();
        if now >= deadline {
            tracing::warn!(
                "{pending} source(s) still not read after {} s; writing without them",
                wait.as_secs()
            );
            return Ok(sources);
        }
        let changed = async {
            loop {
                match events.recv().await {
                    Ok(Event::SourceChanged { .. } | Event::SourceDeleted { .. })
                    | Err(RecvError::Lagged(_)) => return,
                    Ok(_) => {}
                    Err(RecvError::Closed) => std::future::pending::<()>().await,
                }
            }
        };
        // Look again on any source change, and every few seconds regardless.
        let _ = tokio::time::timeout((deadline - now).min(Duration::from_secs(5)), changed).await;
    }
}
