//! Sources: files the learner uploads so outlines and lessons are grounded in them.
//!
//! An upload is stored as `data_dir/sources/{id}/file` and queued. One source at a time,
//! the worker reads it and embeds what it finds (Queued → Processing → Ready); while the
//! embedding model isn't downloaded sources stay Queued:
//!
//! - text and code, and the text of PDFs, cut into chunks (`ChunkRow`);
//! - pictures (`MediaRow`): an image file itself, every PDF page (rendered into
//!   `{id}/media/page-{n}.png`), frames of a video (`{id}/media/frame-{ms}.jpg`, one per
//!   5 s, at most 120), through the image encoder;
//! - sound (`MediaRow`): 10-second windows of an audio file or a video's soundtrack,
//!   through the audio encoder, plus a transcript (FluidAudio speech recognition) cut
//!   into timestamped passages (`ChunkRow` with `start_ms`/`end_ms`).
//!
//! Retrieval scores all of them against the prompt with the one query vector and hands
//! the best to the outline and lesson prompts: text as text, pictures as image inputs,
//! sound as what was said in it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::Json;
use axum::body::Body;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use chrono::Utc;
use margin_ai::{Image, Passage};
use margin_embed::{
    CHUNK_TOKENS, FLUIDAUDIOCLI, ImageTokens, Picture, Purpose, Transcript, audio_windows,
    sample_ms,
};
use margin_shared::{
    Citation, Event, Id, SOURCE_KINDS, Source, SourceKind, SourceStatus, TranscriptSegment, routes,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

use crate::db;
use crate::embedding;
use crate::error::{AppError, AppResult};
use crate::library;
use crate::state::{AppState, SOURCE_ORIGINAL, new_id};

/// The uploaded file's name inside its folder; the original name is kept in the row.
pub const FILE_NAME: &str = "file";
/// What an audio or video source's speech recognition heard, word by word
/// (`Transcript` as JSON), for the app's whole-transcript view.
pub const TRANSCRIPT_FILE: &str = "transcript.json";
/// Chunks embedded per call to the model.
const EMBED_BATCH: usize = 16;
/// Progress is saved and sent at most this often.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);
/// Passages a prompt gets…
pub const PASSAGES: usize = 8;
/// …of which at most this many are pictures.
pub const MAX_IMAGES: usize = 4;
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
    /// Where in a recording a transcript passage is.
    #[serde(default)]
    pub start_ms: Option<u64>,
    #[serde(default)]
    pub end_ms: Option<u64>,
    pub text: String,
    pub vector: Vec<f32>,
}

/// What a `MediaRow` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    /// An image source itself.
    Image,
    /// A rendered PDF page.
    Page,
    /// A video frame.
    Frame,
    /// A 10-second window of sound.
    Audio,
}

/// One embedded picture or stretch of sound of a source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaRow {
    pub source_id: Id,
    /// The source's topic, kept in step with it.
    pub topic_id: Option<Id>,
    /// Position among the source's media, from 0.
    pub ord: u32,
    pub kind: MediaKind,
    /// The picture, by the name the source media route serves: "original", "page-3.png",
    /// "frame-2500.jpg". None for sound.
    pub file: Option<String>,
    pub page: Option<u32>,
    /// A frame's time (with no end), or a sound window's stretch.
    pub start_ms: Option<u64>,
    pub end_ms: Option<u64>,
    /// What was said meanwhile (empty without a transcript).
    pub text: String,
    pub vector: Vec<f32>,
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

#[derive(Debug, Clone, Deserialize)]
pub struct UploadQuery {
    #[serde(default)]
    pub name: String,
    pub folder: Option<String>,
    pub topic_id: Option<Id>,
}

/// An upload's file name, folder and topic, checked: a name Margin can read, and a topic
/// that exists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Upload {
    pub name: String,
    pub folder: Option<String>,
    pub topic_id: Option<Id>,
    pub kind: SourceKind,
}

impl Upload {
    pub async fn check(state: &AppState, q: UploadQuery) -> AppResult<Upload> {
        let name: String = q.name.trim().chars().take(500).collect();
        if name.is_empty() {
            return Err(AppError::bad_request("Say what the file is called."));
        }
        let Some(kind) = SourceKind::for_name(&name) else {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "unsupported_source",
                format!("Margin can't read {name}. Sources can be {SOURCE_KINDS} files."),
            ));
        };
        let folder = q
            .folder
            .map(|f| f.trim().chars().take(500).collect::<String>())
            .filter(|f| !f.is_empty());
        if let Some(topic_id) = &q.topic_id {
            library::topic(state, topic_id).await?;
        }
        Ok(Upload {
            name,
            folder,
            topic_id: q.topic_id,
            kind,
        })
    }

    /// The largest file of its kind.
    pub fn limit(&self) -> u64 {
        routes::max_source_bytes(self.kind)
    }

    /// 413 "too_large", naming its kind's limit.
    pub fn too_large(&self) -> AppError {
        AppError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            routes::too_large_text(&self.name, self.kind),
        )
    }
}

/// The request's Content-Length, when it sent one.
pub fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
}

/// Why a request body couldn't be saved.
#[derive(Debug)]
pub enum ReceiveError {
    /// More than the limit came; what was written of it was taken back off.
    TooLarge,
    /// The client stopped sending (or the connection broke).
    Body(String),
    Io(std::io::Error),
}

impl ReceiveError {
    pub fn into_app_error(self, upload: &Upload, path: &std::path::Path) -> AppError {
        match self {
            ReceiveError::TooLarge => upload.too_large(),
            ReceiveError::Body(e) => AppError::new(
                StatusCode::BAD_REQUEST,
                "incomplete_upload",
                format!("The upload of {} broke off ({e}).", upload.name),
            ),
            ReceiveError::Io(e) => {
                AppError::internal(format!("couldn't save {}: {e}", path.display()))
            }
        }
    }
}

/// Streams `body` onto the end of `path` (created if missing) without holding it in
/// memory, refusing more than `max` bytes of it. Returns how many bytes it added. On
/// any failure the file is cut back to where it was.
pub async fn receive(body: Body, path: &std::path::Path, max: u64) -> Result<u64, ReceiveError> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .map_err(ReceiveError::Io)?;
    let start = file.metadata().await.map_err(ReceiveError::Io)?.len();
    let mut written = 0u64;
    let mut stream = body.into_data_stream();
    let result = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| ReceiveError::Body(e.to_string()))?;
            written += chunk.len() as u64;
            if written > max {
                return Err(ReceiveError::TooLarge);
            }
            file.write_all(&chunk).await.map_err(ReceiveError::Io)?;
        }
        file.flush().await.map_err(ReceiveError::Io)?;
        Ok(written)
    }
    .await;
    if result.is_err() {
        let _ = file.set_len(start).await;
    }
    result
}

pub async fn upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<UploadQuery>, QueryRejection>,
    body: Body,
) -> AppResult<Json<Source>> {
    let Query(q) =
        query.map_err(|rej| AppError::new(rej.status(), "bad_request", rej.body_text()))?;
    let upload = Upload::check(&state, q).await?;
    // Refused before a byte is read when the size is known up front.
    if content_length(&headers).is_some_and(|n| n > upload.limit()) {
        return Err(upload.too_large());
    }

    let id = new_id();
    let dir = state.source_dir(&id);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| AppError::internal(format!("couldn't create {}: {e}", dir.display())))?;
    let path = dir.join(FILE_NAME);
    let bytes = match receive(body, &path, upload.limit()).await {
        Ok(n) => n,
        Err(e) => {
            remove_dir(&dir).await;
            return Err(e.into_app_error(&upload, &path));
        }
    };
    let source = create(&state, &id, upload, bytes).await?;
    Ok(Json(source))
}

/// Records the source whose file is now in `source_dir(id)` and queues it to be read.
/// Removes the folder when the topic went away meanwhile. Returns the wire record.
pub async fn create(state: &AppState, id: &str, upload: Upload, bytes: u64) -> AppResult<Source> {
    let dir = state.source_dir(id);
    let source = Source {
        media_url: None,
        id: id.to_string(),
        topic_id: upload.topic_id,
        name: upload.name,
        folder: upload.folder,
        kind: upload.kind,
        bytes,
        status: SourceStatus::Queued,
        created_at: Utc::now(),
        workspace_id: None,
        modified: None,
    };
    {
        let _w = state.write_lock.lock().await;
        // The topic may have been deleted while the file was being saved.
        if let Some(topic_id) = &source.topic_id
            && let Err(e) = library::topic(state, topic_id).await
        {
            remove_dir(&dir).await;
            return Err(e);
        }
        if let Err(e) = state.store.put(db::SOURCE, id, &source).await {
            remove_dir(&dir).await;
            return Err(e.into());
        }
    }
    state.emit_source(&source);
    let _ = state.source_queue.send(id.to_string());
    Ok(state.public_source(&source))
}

/// "Read again": queue a source to be read from the start, in place (same id, topic and
/// file). For one that failed or was read with a warning, once what was wrong is fixed.
pub async fn reprocess(
    State(state): State<AppState>,
    Path(id): Path<Id>,
) -> AppResult<Json<Source>> {
    let source = {
        let _w = state.write_lock.lock().await;
        let mut s = source(&state, &id).await?;
        if is_pending(&s.status) {
            return Err(AppError::conflict(
                "source_busy",
                format!("{} is already being read.", s.name),
            ));
        }
        s.status = SourceStatus::Queued;
        state.store.put(db::SOURCE, &id, &s).await?;
        s
    };
    state.emit_source(&source);
    let _ = state.source_queue.send(id);
    Ok(Json(state.public_source(&source)))
}

/// An audio or video source's whole transcript, line by line: from the words kept when it
/// was read, or (sources read before those were kept) its timed passages. Empty for
/// other kinds and recordings without one.
pub async fn transcript(
    State(state): State<AppState>,
    Path(id): Path<Id>,
) -> AppResult<Json<Vec<TranscriptSegment>>> {
    let source = source(&state, &id).await?;
    if !matches!(source.kind, SourceKind::Audio | SourceKind::Video) {
        return Ok(Json(Vec::new()));
    }
    let path = state.source_dir(&id).join(TRANSCRIPT_FILE);
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let words: Transcript = serde_json::from_slice(&bytes)
                .map_err(|e| AppError::internal(format!("{}: {e}", path.display())))?;
            return Ok(Json(words.lines()));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(AppError::internal(format!("{}: {e}", path.display()))),
    }
    let mut rows: Vec<ChunkRow> = state.store.list_where(db::CHUNK, "source_id", &id).await?;
    rows.sort_by_key(|r| (r.start_ms, r.ord));
    let lines = rows
        .into_iter()
        .filter_map(|r| {
            let start_ms = r.start_ms?;
            Some(TranscriptSegment {
                start_ms,
                end_ms: r.end_ms.unwrap_or(start_ms).max(start_ms),
                text: r.text.trim().to_string(),
            })
        })
        .filter(|s| !s.text.is_empty())
        .collect();
    Ok(Json(lines))
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
    state.store.delete_where(db::MEDIA, "source_id", id).await?;
    state.store.delete(db::SOURCE, id).await?;
    remove_dir(&state.source_dir(id)).await;
    Ok(Event::SourceDeleted { id: id.to_string() })
}

pub(crate) async fn remove_dir(dir: &std::path::Path) {
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
        state
            .store
            .set_where(db::MEDIA, "source_id", id, "topic_id", &topic_id)
            .await?;
        events.push(Event::SourceChanged {
            source: state.public_source(&source),
        });
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

/// What `process` embeds for a picture or a stretch of sound, before it has a vector.
struct MediaItem {
    row: MediaRow,
    input: MediaInput,
}

enum MediaInput {
    Image(PathBuf, ImageTokens),
    /// A window of the sound: samples [start, end).
    Audio(usize, usize),
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
    let path = match named_link(&state.source_dir(id), &source.name).await {
        Ok(path) => path,
        Err(e) => return fail(state, id, &format!("couldn't prepare the file: {e}")).await,
    };
    let media_dir = state.source_media_dir(id);
    // Pages and frames of a run the service stopped in the middle of.
    remove_dir(&media_dir).await;
    let (tools, from, to) = (state.source_tools.clone(), path.clone(), media_dir.clone());
    let doc = match tokio::task::spawn_blocking(move || tools.extract(&from, kind, &to)).await? {
        Ok(doc) => doc,
        Err(e) => return fail(state, id, &e.to_string()).await,
    };
    let pages = doc.pages.iter().filter_map(|p| p.number).max();
    let sound = doc.sound.clone().map(Arc::new);

    // What is said, for audio and video.
    let mut transcript: Option<Transcript> = None;
    let mut no_transcript: Option<String> = None;
    if let Some(sound) = &sound {
        let (tools, samples, work) = (
            state.source_tools.clone(),
            sound.clone(),
            state.source_dir(id),
        );
        match tokio::task::spawn_blocking(move || tools.transcribe(&samples, &work)).await? {
            Ok(Some(t)) => transcript = Some(t),
            Ok(None) => {
                no_transcript = Some(format!(
                    "{FLUIDAUDIOCLI} isn't installed on the service Mac (Settings → Tools shows how \
                     to install it)"
                ))
            }
            Err(e) => no_transcript = Some(e.to_string()),
        }
        if let Some(why) = &no_transcript {
            tracing::warn!("source {id}: no transcript: {why}");
        }
    }
    // Kept for the app's whole-transcript view; a read without one drops an old one.
    let transcript_path = state.source_dir(id).join(TRANSCRIPT_FILE);
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

    let (tools, counter, transcript_for_chunks) = (
        state.source_tools.clone(),
        embedder.clone(),
        transcript.clone(),
    );
    let chunks = tokio::task::spawn_blocking(move || {
        let count = |t: &str| counter.count_tokens(t);
        let mut chunks = tools.chunk(&doc, kind, &count, CHUNK_TOKENS);
        if let Some(t) = &transcript_for_chunks {
            chunks.extend(t.chunks(&count, CHUNK_TOKENS));
        }
        (chunks, doc.pictures)
    })
    .await?;
    let (chunks, pictures) = chunks;
    let items = media_items(id, kind, &pictures, sound.as_deref(), transcript.as_ref());

    if chunks.is_empty() && items.is_empty() {
        let error = match kind {
            SourceKind::Text | SourceKind::Code | SourceKind::Pdf => {
                "There's no text in this file."
            }
            _ => "There's no picture or sound in this file.",
        };
        return fail(state, id, error).await;
    }
    let total = (chunks.len() + items.len()) as u32;
    if !update(state, id, |s| {
        s.status = SourceStatus::Processing { done: 0, total }
    })
    .await?
    {
        return Ok(());
    }
    // Leftovers of a run the service stopped in the middle of.
    state.store.delete_where(db::CHUNK, "source_id", id).await?;
    state.store.delete_where(db::MEDIA, "source_id", id).await?;

    let mut progress = Progress {
        done: 0,
        total,
        last: Instant::now(),
    };
    let mut ord = 0u32;
    for batch in chunks.chunks(EMBED_BATCH) {
        let texts: Vec<String> = batch.iter().map(|c| c.text.clone()).collect();
        let model = embedder.clone();
        let vectors =
            match tokio::task::spawn_blocking(move || model.embed(&texts, Purpose::DOCUMENT))
                .await?
            {
                Ok(v) => v,
                Err(e) => return fail(state, id, &e.to_string()).await,
            };
        if vectors.len() != batch.len() {
            return fail(state, id, &wrong_count(vectors.len(), batch.len())).await;
        }
        let rows = batch.iter().zip(vectors).map(|(chunk, vector)| {
            let row = ChunkRow {
                source_id: id.to_string(),
                topic_id: None,
                ord,
                page: chunk.page,
                start_ms: chunk.start_ms,
                end_ms: chunk.end_ms,
                text: chunk.text.clone(),
                vector,
            };
            ord += 1;
            row
        });
        let rows: Vec<ChunkRow> = rows.collect();
        if !save_rows(state, id, db::CHUNK, rows, |r, topic| r.topic_id = topic).await? {
            return Ok(());
        }
        if !progress.add(state, id, batch.len()).await? {
            return Ok(());
        }
    }

    let mut ord = 0u32;
    for batch in items.chunks(EMBED_BATCH) {
        // Consecutive items embedded the same way go to the model together.
        let mut start = 0;
        while start < batch.len() {
            let same = batch[start..]
                .iter()
                .take_while(|i| same_input(&i.input, &batch[start].input))
                .count();
            let group = &batch[start..start + same];
            start += same;
            let model = embedder.clone();
            let vectors = match &group[0].input {
                MediaInput::Image(_, tokens) => {
                    let tokens = *tokens;
                    let paths: Vec<PathBuf> = group
                        .iter()
                        .filter_map(|i| match &i.input {
                            MediaInput::Image(p, _) => Some(p.clone()),
                            MediaInput::Audio(..) => None,
                        })
                        .collect();
                    tokio::task::spawn_blocking(move || model.embed_images(&paths, tokens)).await?
                }
                MediaInput::Audio(..) => {
                    let samples = sound.clone().unwrap_or_default();
                    let windows: Vec<Vec<f32>> = group
                        .iter()
                        .filter_map(|i| match i.input {
                            MediaInput::Audio(from, to) => Some(samples[from..to].to_vec()),
                            MediaInput::Image(..) => None,
                        })
                        .collect();
                    tokio::task::spawn_blocking(move || model.embed_audio(&windows)).await?
                }
            };
            let vectors = match vectors {
                Ok(v) => v,
                Err(e) => return fail(state, id, &e.to_string()).await,
            };
            if vectors.len() != group.len() {
                return fail(state, id, &wrong_count(vectors.len(), group.len())).await;
            }
            let rows: Vec<MediaRow> = group
                .iter()
                .zip(vectors)
                .map(|(item, vector)| {
                    let row = MediaRow {
                        ord,
                        vector,
                        ..item.row.clone()
                    };
                    ord += 1;
                    row
                })
                .collect();
            if !save_rows(state, id, db::MEDIA, rows, |r, topic| r.topic_id = topic).await? {
                return Ok(());
            }
            if !progress.add(state, id, group.len()).await? {
                return Ok(());
            }
        }
    }

    // A recording without a transcript gives the lesson writer less to go on (it can't
    // listen): its sound is embedded and found, but lessons can't quote it. It's Ready,
    // with a warning that says so and a "Read again" for once it's fixed.
    let warning = no_transcript.map(|why| transcript_warning(kind, &why));
    update(state, id, |s| {
        s.status = SourceStatus::Ready {
            chunks: total,
            pages,
            warning,
        }
    })
    .await?;
    tracing::info!("source {id} is ready: {total} chunks and media items");
    Ok(())
}

/// The warning on a recording read without a transcript, `why` being what went wrong.
fn transcript_warning(kind: SourceKind, why: &str) -> String {
    if kind == SourceKind::Video {
        format!(
            "Its soundtrack wasn't transcribed: {why}. Its pictures are read, but lessons \
             can't quote what's said. Read it again once that's fixed."
        )
    } else {
        format!(
            "It wasn't transcribed: {why}. Its sound is still searched, but lessons can't \
             quote what's said. Read it again once that's fixed."
        )
    }
}

/// The uploaded file under a name with the original's extension (`input.m4a`, a link to
/// `file`): AVFoundation picks the container format by extension and won't open an
/// audio or video file without one. `file` itself when the name has no extension.
async fn named_link(dir: &std::path::Path, name: &str) -> std::io::Result<PathBuf> {
    let Some(link) = link_name(name).map(|n| dir.join(n)) else {
        return Ok(dir.join(FILE_NAME));
    };
    match tokio::fs::remove_file(&link).await {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    tokio::fs::symlink(FILE_NAME, &link).await?;
    Ok(link)
}

/// `input.{ext}` for a source called `name`, if it has a plain extension.
fn link_name(name: &str) -> Option<String> {
    name.rsplit(['/', '\\'])
        .next()
        .and_then(|n| n.rsplit_once('.'))
        .map(|(_, e)| e.to_ascii_lowercase())
        .filter(|e| !e.is_empty() && e.len() <= 8 && e.bytes().all(|b| b.is_ascii_alphanumeric()))
        .map(|e| format!("input.{e}"))
}

/// The file to read a source from: its `named_link` once made, else `file`.
fn input_path(dir: &std::path::Path, name: &str) -> PathBuf {
    link_name(name)
        .map(|n| dir.join(n))
        .filter(|p| p.exists())
        .unwrap_or_else(|| dir.join(FILE_NAME))
}

fn wrong_count(got: usize, wanted: usize) -> String {
    format!("The embedding model returned {got} vectors for {wanted} inputs.")
}

fn same_input(a: &MediaInput, b: &MediaInput) -> bool {
    match (a, b) {
        (MediaInput::Image(_, x), MediaInput::Image(_, y)) => x == y,
        (MediaInput::Audio(..), MediaInput::Audio(..)) => true,
        _ => false,
    }
}

/// Progress of a source being embedded, saved and sent at most every
/// `PROGRESS_INTERVAL`.
struct Progress {
    done: u32,
    total: u32,
    last: Instant,
}

impl Progress {
    /// Counts `n` more done. False when the source is gone.
    async fn add(&mut self, state: &AppState, id: &str, n: usize) -> anyhow::Result<bool> {
        self.done += n as u32;
        if self.done >= self.total || self.last.elapsed() < PROGRESS_INTERVAL {
            return Ok(true);
        }
        self.last = Instant::now();
        let (done, total) = (self.done, self.total);
        update(state, id, |s| {
            s.status = SourceStatus::Processing { done, total }
        })
        .await
    }
}

/// Saves rows of a source that still exists, with its current topic set by `set_topic`.
/// False when it was deleted meanwhile.
async fn save_rows<T: Serialize + RowId>(
    state: &AppState,
    id: &str,
    table: &'static str,
    mut rows: Vec<T>,
    set_topic: impl Fn(&mut T, Option<Id>),
) -> anyhow::Result<bool> {
    // Under the lock, so a delete or an attach can't interleave with the write.
    let _w = state.write_lock.lock().await;
    let Some(current) = state.store.get::<Source>(db::SOURCE, id).await? else {
        return Ok(false);
    };
    for row in &mut rows {
        set_topic(row, current.topic_id.clone());
        state.store.put(table, &row.row_id(), row).await?;
    }
    Ok(true)
}

trait RowId {
    fn row_id(&self) -> String;
}

impl RowId for ChunkRow {
    fn row_id(&self) -> String {
        format!("{}-{}", self.source_id, self.ord)
    }
}

impl RowId for MediaRow {
    fn row_id(&self) -> String {
        format!("{}-m{}", self.source_id, self.ord)
    }
}

/// The pictures and sound windows of a source, each with the words said during it.
fn media_items(
    source_id: &str,
    kind: SourceKind,
    pictures: &[Picture],
    sound: Option<&Vec<f32>>,
    transcript: Option<&Transcript>,
) -> Vec<MediaItem> {
    let said = |start: u64, end: u64| transcript.map(|t| t.span(start, end)).unwrap_or_default();
    let row = |kind, file: Option<String>, page, start_ms, end_ms, text| MediaRow {
        source_id: source_id.to_string(),
        topic_id: None,
        ord: 0,
        kind,
        file,
        page,
        start_ms,
        end_ms,
        text,
        vector: Vec::new(),
    };
    let mut items = Vec::new();
    // Frames stand for the stretch of video around them.
    let frame_stretch = pictures
        .windows(2)
        .find_map(|w| Some(w[1].at_ms? - w[0].at_ms?))
        .unwrap_or(margin_embed::FRAME_INTERVAL_MS);
    for p in pictures {
        let (media_kind, file) = match (kind, p.page, p.at_ms) {
            (SourceKind::Image, ..) => (MediaKind::Image, SOURCE_ORIGINAL.to_string()),
            (_, Some(n), _) => (MediaKind::Page, margin_embed::page_file(n)),
            (_, _, Some(ms)) => (MediaKind::Frame, margin_embed::frame_file(ms)),
            _ => continue,
        };
        // A frame is cited at its moment (the viewer starts the video there).
        let (start, end) = (p.at_ms, None);
        let text = match p.at_ms {
            Some(at) => said(at.saturating_sub(frame_stretch / 2), at + frame_stretch / 2),
            None => String::new(),
        };
        items.push(MediaItem {
            row: row(media_kind, Some(file), p.page, start, end, text),
            input: MediaInput::Image(p.path.clone(), p.tokens),
        });
    }
    if let Some(samples) = sound {
        for (start, window) in audio_windows(samples) {
            let (from, to) = (sample_ms(start), sample_ms(start + window.len()));
            items.push(MediaItem {
                row: row(
                    MediaKind::Audio,
                    None,
                    None,
                    Some(from),
                    Some(to),
                    said(from, to),
                ),
                input: MediaInput::Audio(start, start + window.len()),
            });
        }
    }
    items
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

/// A passage for a prompt, with the citation that points back at it (`n` is filled in
/// once the lesson cites it; `image_url` holds the bare media file name).
#[derive(Debug, Clone)]
pub struct Retrieved {
    pub passage: Passage,
    pub citation: Citation,
}

/// The passages of `source_ids` most relevant to `query`, best first: text chunks,
/// transcript passages and pictures together, at most `PASSAGES` of them with at most
/// `MAX_IMAGES` pictures. Sources still being read are waited for (up to
/// `SOURCE_WAIT`); after that, or when retrieval fails, generation goes on with what
/// there is.
pub async fn passages(state: &AppState, source_ids: &[Id], query: &str) -> Vec<Retrieved> {
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

/// `passages` for the agent's `search_sources`: waits at most `wait` for sources still
/// being read, and says why retrieval failed instead of going on without it.
pub async fn passages_within(
    state: &AppState,
    source_ids: &[Id],
    query: &str,
    wait: Duration,
) -> anyhow::Result<Vec<Retrieved>> {
    if source_ids.is_empty() {
        return Ok(Vec::new());
    }
    retrieve(state, source_ids, query, wait).await
}

/// Something retrieval can pick.
enum Candidate {
    Text(ChunkRow),
    Media(MediaRow),
}

impl Candidate {
    fn text(&self) -> &str {
        match self {
            Candidate::Text(c) => &c.text,
            Candidate::Media(m) => &m.text,
        }
    }

    fn vector(&self) -> &[f32] {
        match self {
            Candidate::Text(c) => &c.vector,
            Candidate::Media(m) => &m.vector,
        }
    }

    fn source_id(&self) -> &str {
        match self {
            Candidate::Text(c) => &c.source_id,
            Candidate::Media(m) => &m.source_id,
        }
    }

    /// The stretch of a recording it covers, when it is read as text (a transcript
    /// passage or a sound window).
    fn spoken_range(&self) -> Option<(u64, u64)> {
        match self {
            Candidate::Text(c) => Some((c.start_ms?, c.end_ms?)),
            Candidate::Media(m) if m.kind == MediaKind::Audio => Some((m.start_ms?, m.end_ms?)),
            Candidate::Media(_) => None,
        }
    }

    fn picture(&self) -> Option<&str> {
        match self {
            Candidate::Media(m) if m.kind != MediaKind::Audio => m.file.as_deref(),
            _ => None,
        }
    }
}

/// Whether two stretches overlap by more than half of the shorter one.
fn mostly_overlap(a: (u64, u64), b: (u64, u64)) -> bool {
    let overlap = a.1.min(b.1).saturating_sub(a.0.max(b.0));
    let shorter = (a.1 - a.0).min(b.1 - b.0).max(1);
    overlap * 2 > shorter
}

async fn retrieve(
    state: &AppState,
    source_ids: &[Id],
    query: &str,
    wait: Duration,
) -> anyhow::Result<Vec<Retrieved>> {
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

    let mut candidates: Vec<Candidate> = Vec::new();
    for s in &ready {
        let chunks: Vec<ChunkRow> = state
            .store
            .list_where(db::CHUNK, "source_id", &s.id)
            .await?;
        candidates.extend(chunks.into_iter().map(Candidate::Text));
        let media: Vec<MediaRow> = state
            .store
            .list_where(db::MEDIA, "source_id", &s.id)
            .await?;
        candidates.extend(media.into_iter().map(Candidate::Media));
    }
    let tools = state.source_tools.clone();
    let query = query.to_string();
    let media_dirs: HashMap<Id, (PathBuf, PathBuf)> = ready
        .iter()
        .map(|s| {
            (
                s.id.clone(),
                (
                    input_path(&state.source_dir(&s.id), &s.name),
                    state.source_media_dir(&s.id),
                ),
            )
        })
        .collect();
    let sources: HashMap<Id, Source> = ready.into_iter().map(|s| (s.id.clone(), s)).collect();
    let picked = tokio::task::spawn_blocking(move || {
        // Video frames by source, for a picture of transcript passages.
        let mut frames: HashMap<String, Vec<(u64, String)>> = HashMap::new();
        for c in &candidates {
            if let Candidate::Media(m) = c
                && m.kind == MediaKind::Frame
                && let (Some(at), Some(file)) = (m.start_ms, &m.file)
            {
                frames
                    .entry(m.source_id.clone())
                    .or_default()
                    .push((at, file.clone()));
            }
        }
        let mut scored: Vec<(f32, Candidate)> = candidates
            .into_iter()
            // A sound window without a transcript has nothing the writer can read.
            .filter(|c| !matches!(c, Candidate::Media(m) if m.kind == MediaKind::Audio && m.text.trim().is_empty()))
            .map(|c| (tools.score(&query_vec, &query, c.vector(), c.text()), c))
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));

        let mut seen_text = HashSet::new();
        let mut spoken: Vec<(String, (u64, u64))> = Vec::new();
        let mut images = 0;
        let mut seen_images: HashSet<Vec<u8>> = HashSet::new();
        let mut picked = Vec::new();
        for (_, c) in scored {
            if picked.len() == PASSAGES {
                break;
            }
            let Some(source) = sources.get(c.source_id()) else {
                continue;
            };
            let Some((original, media_dir)) = media_dirs.get(c.source_id()) else {
                continue;
            };
            if let Some(file) = c.picture() {
                if images == MAX_IMAGES {
                    continue;
                }
                let path = if file == SOURCE_ORIGINAL {
                    original.clone()
                } else {
                    media_dir.join(file)
                };
                let image = match tools.image_for_model(&path) {
                    Ok((media_type, data)) => Image { media_type, data },
                    Err(e) => {
                        tracing::warn!("couldn't read {}: {e}", path.display());
                        continue;
                    }
                };
                // The same picture twice (a still stretch of video) tells the writer
                // nothing new.
                if !seen_images.insert(image.data.clone()) {
                    continue;
                }
                images += 1;
                let Candidate::Media(m) = c else { continue };
                picked.push(retrieved(source, &m.text, m.page, m.start_ms, m.end_ms, Some(image), m.file.clone()));
                continue;
            }
            let normalized = c.text().split_whitespace().collect::<Vec<_>>().join(" ");
            if normalized.is_empty() || !seen_text.insert(normalized) {
                continue;
            }
            if let Some(range) = c.spoken_range() {
                if spoken
                    .iter()
                    .any(|(id, r)| id == c.source_id() && mostly_overlap(*r, range))
                {
                    continue;
                }
                spoken.push((c.source_id().to_string(), range));
            }
            let (page, start_ms, end_ms) = match &c {
                Candidate::Text(t) => (t.page, t.start_ms, t.end_ms),
                Candidate::Media(m) => (m.page, m.start_ms, m.end_ms),
            };
            // A picture for the citation: the PDF page, or the video frame nearest the words.
            let picture = match (page, start_ms) {
                (Some(n), _) => Some(margin_embed::page_file(n))
                    .filter(|f| media_dir.join(f).is_file()),
                (None, Some(start)) => {
                    let middle = (start + end_ms.unwrap_or(start)) / 2;
                    frames
                        .get(c.source_id())
                        .and_then(|f| f.iter().min_by_key(|(at, _)| at.abs_diff(middle)))
                        .map(|(_, file)| file.clone())
                }
                _ => None,
            };
            picked.push(retrieved(source, c.text(), page, start_ms, end_ms, None, picture));
        }
        picked
    })
    .await?;
    Ok(picked)
}

/// A picked passage and its citation.
fn retrieved(
    source: &Source,
    text: &str,
    page: Option<u32>,
    start_ms: Option<u64>,
    end_ms: Option<u64>,
    image: Option<Image>,
    picture: Option<String>,
) -> Retrieved {
    Retrieved {
        passage: Passage {
            source: source.name.clone(),
            page,
            text: text.to_string(),
            start_ms,
            end_ms,
            image,
        },
        citation: Citation {
            n: 0,
            source_id: source.id.clone(),
            source_name: source.name.clone(),
            kind: source.kind,
            page,
            start_ms,
            end_ms,
            excerpt: text.trim().to_string(),
            image_url: picture,
            document_id: None,
            start_line: None,
            end_line: None,
            file: None,
        },
    }
}

/// Renumbers a finished lesson's `[^n]` markers (n = 1-based index into `passages`) in
/// order of first appearance and builds its citations, `image_url` still a bare media
/// file name (`App::public_lesson` signs it).
pub fn cite(body: &str, passages: &[Retrieved]) -> (String, Vec<Citation>) {
    let renumbered = margin_ai::renumber_citations(body, passages.len());
    let citations = renumbered
        .cited
        .iter()
        .enumerate()
        .map(|(i, &index)| Citation {
            n: i as u32 + 1,
            ..passages[index].citation.clone()
        })
        .collect();
    (renumbered.body, citations)
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
