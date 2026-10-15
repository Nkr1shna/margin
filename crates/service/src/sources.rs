//! Sources: files the learner uploads so outlines and lessons are grounded in them.
//!
//! An upload is stored as `data_dir/sources/{id}/file`, recorded as a `Source` and
//! queued. The source worker gives it a document (same id) and hands it to the one
//! ingestion pipeline (`ingest.rs`), which reads it (Queued → Processing → Ready) into
//! `passage` rows; while the embedding model isn't downloaded sources stay Queued:
//!
//! - text and code, and the text of PDFs, cut into chunks;
//! - pictures: an image file itself, every PDF page, frames of a video (one per 5 s, at
//!   most 120), through the image encoder, each with its text (a page's text layer or
//!   OCR, an image's OCR, what was said around a frame);
//! - sound: 10-second windows of an audio file or a video's soundtrack, through the
//!   audio encoder, plus a transcript (Parakeet speech recognition) cut into
//!   timestamped passages.
//!
//! This file keeps upload, delete, attach and "Read again", and the passages lessons
//! and the agent get today (`passages`), read from the `passage` rows of the sources (or
//! a source's old `chunk`/`media` rows until the migration has moved it), until
//! `retrieval.rs` takes over.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::Extension;
use axum::Json;
use axum::body::Body;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use chrono::{DateTime, Utc};
use margin_ai::{Image, Passage};
use margin_embed::{Purpose, Transcript};
use margin_shared::context::{ACCESS_COMPANY, Modality};
use margin_shared::{
    Citation, Event, FOLDER_FILE_TYPES_TEXT, FileStamp, Id, SOURCE_KINDS, Source, SourceKind,
    SourceStatus, TranscriptSegment, Workspace, folder_file_type, routes,
};

use crate::auth::CurrentDevice;
use crate::folders;
use crate::state::DeviceRow;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

use crate::db::{self, PassageRow};
use crate::embedding;
use crate::error::{AppError, AppResult};
use crate::ingest;
use crate::library;
use crate::state::{AppState, SOURCE_ORIGINAL, new_id};

/// The uploaded file's name inside its folder; the original name is kept in the row.
pub const FILE_NAME: &str = "file";
/// What an audio or video source's speech recognition heard, word by word
/// (`Transcript` as JSON), for the app's whole-transcript view.
pub const TRANSCRIPT_FILE: &str = "transcript.json";
/// Passages a prompt gets…
pub const PASSAGES: usize = 8;
/// …of which at most this many are pictures.
pub const MAX_IMAGES: usize = 4;
/// How long generation waits for its sources to be read before going on without them.
pub const SOURCE_WAIT: Duration = Duration::from_secs(10 * 60);
/// Uploads never attached to a topic (an Ask that wasn't sent) are deleted at startup
/// once they're this old.
pub const UNATTACHED_TTL: chrono::Duration = chrono::Duration::hours(24);

/// One embedded piece of a source, as stores written before the `passage` table kept
/// it (table `chunk`). Read only until the migration has moved every source.
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

/// One embedded picture or stretch of sound of a source, as stores written before the
/// `passage` table kept it (table `media`). Read only until the migration.
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
    /// A file of the calling device's attached folder (`name` is its path inside it).
    #[serde(default)]
    pub workspace_id: Option<Id>,
    /// The file's modification time on the device (RFC 3339).
    #[serde(default)]
    pub modified: Option<DateTime<Utc>>,
}

/// An upload's file name, folder and topic, checked: a name Margin can read, and a topic
/// that exists; or, for a folder's file, a path inside a folder the caller holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Upload {
    pub name: String,
    pub folder: Option<String>,
    pub topic_id: Option<Id>,
    pub kind: SourceKind,
    /// The attached folder the file belongs to.
    #[serde(default)]
    pub workspace: Option<Workspace>,
    #[serde(default)]
    pub modified: Option<DateTime<Utc>>,
}

impl Upload {
    /// Checks an upload's query; `device` is the device sending it.
    pub async fn check(state: &AppState, q: UploadQuery, device: &DeviceRow) -> AppResult<Upload> {
        if let Some(workspace_id) = q.workspace_id.clone() {
            return Self::check_folder_file(state, &workspace_id, q, device).await;
        }
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
            workspace: None,
            modified: q.modified,
        })
    }

    /// A file of an attached folder: the folder must exist (404) and be the caller's
    /// (403 "not_yours"), no topic (400), a path inside the folder (400 "bad_path") of a
    /// type folders take (400 "unsupported_source"). `folder` is the folder's name.
    async fn check_folder_file(
        state: &AppState,
        workspace_id: &str,
        q: UploadQuery,
        device: &DeviceRow,
    ) -> AppResult<Upload> {
        let workspace: Workspace = state
            .store
            .get(db::WORKSPACE, workspace_id)
            .await?
            .ok_or_else(|| AppError::not_found("folder"))?;
        if workspace.device_id != device.id {
            return Err(AppError::new(
                StatusCode::FORBIDDEN,
                "not_yours",
                format!(
                    "This folder is attached on {}; only that device sends its files.",
                    workspace.device_name
                ),
            ));
        }
        if q.topic_id.is_some() {
            return Err(AppError::bad_request(
                "A folder's file can't be filed under a topic.",
            ));
        }
        let Some(name) = folders::clean_path(&q.name) else {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "bad_path",
                folders::bad_path_text(&q.name),
            ));
        };
        let Some(kind) = folder_file_type(&name) else {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "unsupported_source",
                format!("Margin doesn't take {name} from folders: only {FOLDER_FILE_TYPES_TEXT}."),
            ));
        };
        Ok(Upload {
            name,
            folder: Some(workspace.name.clone()),
            topic_id: None,
            kind: kind.source_kind(),
            workspace: Some(workspace),
            modified: q.modified,
        })
    }

    /// The largest file of its kind; a folder's files have no limit (owner), only
    /// `UPLOAD_PART_BYTES` bounds one request.
    pub fn limit(&self) -> u64 {
        match self.workspace {
            Some(_) => u64::MAX,
            None => routes::max_source_bytes(self.kind),
        }
    }

    /// Whether this is a file of an attached folder.
    pub fn is_folder_file(&self) -> bool {
        self.workspace.is_some()
    }

    /// The most one request may carry: the kind's limit, and for a folder's file one
    /// part (`UPLOAD_PART_BYTES`; bigger files go through `SOURCE_UPLOADS`).
    pub fn request_limit(&self) -> u64 {
        match self.workspace {
            Some(_) => routes::UPLOAD_PART_BYTES,
            None => self.limit(),
        }
    }

    /// 413 "too_large", naming its kind's limit (or, for a folder's file, saying it
    /// goes up in parts).
    pub fn too_large(&self) -> AppError {
        if self.is_folder_file() {
            return AppError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_large",
                format!(
                    "{} is over {} MB: send it in parts (source uploads).",
                    self.name,
                    routes::UPLOAD_PART_BYTES / (1024 * 1024)
                ),
            );
        }
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
/// memory, refusing more than `max` bytes of it. Returns how many bytes it added and
/// the SHA-256 (lower-case hex) of those bytes. On any failure the file is cut back to
/// where it was.
pub async fn receive(
    body: Body,
    path: &std::path::Path,
    max: u64,
) -> Result<(u64, String), ReceiveError> {
    use futures_util::StreamExt;
    use sha2::Digest;
    use tokio::io::AsyncWriteExt;

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .map_err(ReceiveError::Io)?;
    let start = file.metadata().await.map_err(ReceiveError::Io)?.len();
    let mut written = 0u64;
    let mut hash = sha2::Sha256::new();
    let mut stream = body.into_data_stream();
    let result = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| ReceiveError::Body(e.to_string()))?;
            written += chunk.len() as u64;
            if written > max {
                drain(&mut stream).await;
                return Err(ReceiveError::TooLarge);
            }
            hash.update(&chunk);
            file.write_all(&chunk).await.map_err(ReceiveError::Io)?;
        }
        file.flush().await.map_err(ReceiveError::Io)?;
        Ok((written, hex::encode(hash.finalize())))
    }
    .await;
    if result.is_err() {
        let _ = file.set_len(start).await;
    }
    result
}

/// Most of a refused body read and dropped so the client gets to read the refusal.
pub const DRAIN_MAX_BYTES: u64 = 128 * 1024 * 1024;
/// How long a refused body is read for at most.
pub const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Reads and drops what is left of a refused upload (at most `DRAIN_MAX_BYTES`, for at
/// most `DRAIN_TIMEOUT`). Answering while the client is still sending makes the server
/// close the connection under it, and the client sees a reset or a broken pipe instead
/// of the 413; past the bounds the connection is closed all the same.
async fn drain<S, E>(stream: &mut S)
where
    S: futures_util::Stream<Item = Result<axum::body::Bytes, E>> + Unpin,
{
    use futures_util::StreamExt;
    let mut left = DRAIN_MAX_BYTES;
    let _ = tokio::time::timeout(DRAIN_TIMEOUT, async {
        while let Some(Ok(chunk)) = stream.next().await {
            let n = chunk.len() as u64;
            if n >= left {
                break;
            }
            left -= n;
        }
    })
    .await;
}

pub async fn upload(
    State(state): State<AppState>,
    Extension(CurrentDevice(device)): Extension<CurrentDevice>,
    headers: HeaderMap,
    query: Result<Query<UploadQuery>, QueryRejection>,
    body: Body,
) -> AppResult<Json<Source>> {
    let Query(q) =
        query.map_err(|rej| AppError::new(rej.status(), "bad_request", rej.body_text()))?;
    let upload = Upload::check(&state, q, &device).await?;
    // Refused before a byte is read when the size is known up front.
    if content_length(&headers).is_some_and(|n| n > upload.request_limit()) {
        drain(&mut body.into_data_stream()).await;
        return Err(upload.too_large());
    }

    let id = new_id();
    let dir = state.source_dir(&id);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| AppError::internal(format!("couldn't create {}: {e}", dir.display())))?;
    let path = dir.join(FILE_NAME);
    let (bytes, sha256) = match receive(body, &path, upload.request_limit()).await {
        Ok(r) => r,
        Err(e) => {
            remove_dir(&dir).await;
            return Err(e.into_app_error(&upload, &path));
        }
    };
    let source = create_or_replace(&state, &id, upload, bytes, sha256).await?;
    Ok(Json(source))
}

/// The SHA-256 (lower-case hex) of a file, read in pieces on a blocking thread.
pub async fn hash_file(path: &std::path::Path) -> std::io::Result<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use sha2::Digest;
        use std::io::Read;
        let mut file = std::fs::File::open(&path)?;
        let mut hash = sha2::Sha256::new();
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hash.update(&buf[..n]);
        }
        Ok(hex::encode(hash.finalize()))
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Records the source whose file is now in `source_dir(id)` and queues it to be read.
/// Removes the folder when the topic (or the attached folder) went away meanwhile.
/// Returns the wire record.
///
/// A file of an attached folder whose path already has a source **replaces it in
/// place**: the new file moves over the old one's, which keeps its id (so its document,
/// unchanged passages and citations stay), its renders are dropped and it is read again;
/// `source_dir(id)` is removed. `sha256` is what was received; the folder's row records
/// it (`folders::record_received_locked`).
pub async fn create_or_replace(
    state: &AppState,
    id: &str,
    upload: Upload,
    bytes: u64,
    sha256: String,
) -> AppResult<Source> {
    let dir = state.source_dir(id);
    let workspace_id = upload.workspace.as_ref().map(|w| w.id.clone());
    let source = {
        let _w = state.write_lock.lock().await;
        let result = match &workspace_id {
            None => create_locked(state, id, upload, bytes).await,
            Some(ws) => replace_locked(state, id, ws, upload, bytes, sha256).await,
        };
        match result {
            Ok(s) => s,
            Err(e) => {
                remove_dir(&dir).await;
                return Err(e);
            }
        }
    };
    state.emit_source(&source);
    let _ = state.source_queue.send(source.id.clone());
    if let Some(ws) = &workspace_id {
        folders::refresh_sync_logged(state, ws).await;
    }
    Ok(state.public_source(&source))
}

/// A new topic or chat source. Call with the write lock held.
async fn create_locked(
    state: &AppState,
    id: &str,
    upload: Upload,
    bytes: u64,
) -> AppResult<Source> {
    // The topic may have been deleted while the file was being saved.
    if let Some(topic_id) = &upload.topic_id {
        library::topic(state, topic_id).await?;
    }
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
    state.store.put(db::SOURCE, id, &source).await?;
    Ok(source)
}

/// A folder's file: a new source, or the path's source with the new file moved over
/// its own. Call with the write lock held; on `Err` the caller removes `source_dir(id)`.
async fn replace_locked(
    state: &AppState,
    id: &str,
    workspace_id: &str,
    upload: Upload,
    bytes: u64,
    sha256: String,
) -> AppResult<Source> {
    // The folder may have been removed while the file was being saved.
    let ws: Workspace = state
        .store
        .get(db::WORKSPACE, workspace_id)
        .await?
        .ok_or_else(|| AppError::not_found("folder"))?;
    let kind = folder_file_type(&upload.name).ok_or_else(|| {
        AppError::internal(format!(
            "{} passed the check but isn't a folder file",
            upload.name
        ))
    })?;
    let stamp = FileStamp {
        bytes,
        modified: upload.modified,
        sha256,
    };
    let existing = state
        .store
        .list_where::<Source>(db::SOURCE, "workspace_id", workspace_id)
        .await?
        .into_iter()
        .find(|s| s.name == upload.name);
    let source = match existing {
        Some(mut old) => {
            let old_dir = state.source_dir(&old.id);
            let moved = async {
                tokio::fs::create_dir_all(&old_dir).await?;
                tokio::fs::rename(
                    state.source_dir(id).join(FILE_NAME),
                    old_dir.join(FILE_NAME),
                )
                .await?;
                // The named link the readers open is made again when it's read.
                if let Some(link) = link_name(&old.name) {
                    match tokio::fs::remove_file(old_dir.join(link)).await {
                        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
                        _ => {}
                    }
                }
                std::io::Result::Ok(())
            }
            .await;
            if let Err(e) = moved {
                return Err(AppError::internal(format!(
                    "couldn't replace {}: {e}",
                    old_dir.display()
                )));
            }
            if matches!(old.status, SourceStatus::Processing { .. }) {
                // The read in progress finishes on the old copy and must not mark the
                // new one read; it's queued behind it (its renders are redone then).
                state
                    .sources_replaced
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(old.id.clone());
            } else {
                remove_dir(&state.source_media_dir(&old.id)).await;
            }
            remove_dir(&state.source_dir(id)).await;
            old.bytes = bytes;
            old.modified = upload.modified;
            old.kind = upload.kind;
            old.folder = Some(ws.name.clone());
            old.status = SourceStatus::Queued;
            state.store.put(db::SOURCE, &old.id, &old).await?;
            old
        }
        None => {
            let source = Source {
                media_url: None,
                id: id.to_string(),
                topic_id: None,
                name: upload.name,
                folder: Some(ws.name.clone()),
                kind: upload.kind,
                bytes,
                status: SourceStatus::Queued,
                created_at: Utc::now(),
                workspace_id: Some(workspace_id.to_string()),
                modified: upload.modified,
            };
            state.store.put(db::SOURCE, id, &source).await?;
            source
        }
    };
    folders::record_received_locked(state, workspace_id, &source.name, kind, &source.id, stamp)
        .await?;
    Ok(source)
}

/// "Read again": queue a source to be read from the start, in place (same id, topic and
/// file). For one that failed or was read with a warning, once what was wrong is fixed.
/// What didn't change keeps its vectors (`ingest`'s diff).
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
    let mut timed: Vec<(u64, u64, i64, String)> = candidates(&state, &id)
        .await?
        .into_iter()
        .filter(|r| r.modality == Modality::Text.as_str())
        .filter_map(|r| {
            let start = r.start_ms? as u64;
            Some((start, r.end_ms.map_or(start, |e| e as u64), r.ord, r.text))
        })
        .collect();
    timed.sort_by_key(|(start, _, ord, _)| (*start, *ord));
    let lines = timed
        .into_iter()
        .map(|(start_ms, end_ms, _, text)| TranscriptSegment {
            start_ms,
            end_ms: end_ms.max(start_ms),
            text: text.trim().to_string(),
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

/// Deletes a source's passages, document, row and file (an ingest still running for it
/// stops at its next write). Call with the write lock held; returns the event to emit.
pub async fn delete_locked(state: &AppState, id: &str) -> anyhow::Result<Event> {
    delete_old_rows(state, id).await?;
    ingest::delete_document_locked(state, id).await?;
    state.store.delete(db::SOURCE, id).await?;
    remove_dir(&state.source_dir(id)).await;
    Ok(Event::SourceDeleted { id: id.to_string() })
}

/// Whether the `chunk`/`media` tables of a store from before the `passage` table are
/// still there (until the migration removes them).
pub async fn has_old_rows(state: &AppState) -> anyhow::Result<bool> {
    Ok(state.store.table_exists(db::CHUNK).await? && state.store.table_exists(db::MEDIA).await?)
}

/// Deletes a source's `chunk`/`media` rows, while those tables exist.
pub(crate) async fn delete_old_rows(state: &AppState, id: &str) -> anyhow::Result<()> {
    if has_old_rows(state).await? {
        state.store.delete_where(db::CHUNK, "source_id", id).await?;
        state.store.delete_where(db::MEDIA, "source_id", id).await?;
    }
    Ok(())
}

pub(crate) async fn remove_dir(dir: &std::path::Path) {
    match tokio::fs::remove_dir_all(dir).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("couldn't remove {}: {e}", dir.display()),
    }
}

/// Attaches unattached sources to `topic_id`, and makes their passages searchable from
/// the topic. Call with the write lock held; returns the events to emit. Sources that no
/// longer exist or already belong to a topic are left alone.
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
        // A folder's files stay in their folder, never in a topic.
        if source.topic_id.is_some() || source.workspace_id.is_some() {
            continue;
        }
        source.topic_id = Some(topic_id.to_string());
        state.store.put(db::SOURCE, id, &source).await?;
        events.push(Event::SourceChanged {
            source: state.public_source(&source),
        });
        events.extend(ingest::add_scope_locked(state, id, topic_id).await?);
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

/// Starts the ingest worker (which first moves old stores to the passage index), then
/// gives every source sent to `queue` its document and hands it to that worker, forever.
pub async fn run_worker(state: AppState, mut queue: mpsc::UnboundedReceiver<Id>) {
    while let Some(id) = queue.recv().await {
        if let Err(e) = ingest::queue_source(&state, &id).await {
            tracing::error!("source {id}: {e:#}");
            let _ = ingest::fail(&state, &id, &format!("{e:#}")).await;
        }
    }
}

/// The warning on a recording read without a transcript, `why` being what went wrong.
pub(crate) fn transcript_warning(kind: SourceKind, why: &str) -> String {
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

/// The warning on an image or scanned PDF read without OCR, `why` being what's missing.
pub(crate) fn ocr_warning(kind: SourceKind, why: &str) -> String {
    let what = if kind == SourceKind::Pdf {
        "Its scanned pages were read without their text"
    } else {
        "It was read without the text in it"
    };
    format!(
        "{what}: {why}. Its pictures are still searched, but lessons can't quote that \
         text. Read it again once that's fixed."
    )
}

/// The uploaded file under a name with the original's extension (`input.m4a`, a link to
/// `file`): the readers take the extension as a hint to the format. `file` itself when
/// the name has no extension.
pub(crate) async fn named_link(dir: &std::path::Path, name: &str) -> std::io::Result<PathBuf> {
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

/// A source's passages: its `passage` rows (the document index), or the `chunk`/`media`
/// rows of a store the migration hasn't moved it out of yet, as passages.
pub async fn candidates(state: &AppState, source_id: &str) -> anyhow::Result<Vec<PassageRow>> {
    let rows = state.store.passages_of(source_id).await?;
    if !rows.is_empty() || !has_old_rows(state).await? {
        return Ok(rows);
    }
    let chunks: Vec<ChunkRow> = state
        .store
        .list_where(db::CHUNK, "source_id", source_id)
        .await?;
    let media: Vec<MediaRow> = state
        .store
        .list_where(db::MEDIA, "source_id", source_id)
        .await?;
    let mut rows: Vec<PassageRow> = chunks.into_iter().map(old_chunk_passage).collect();
    let offset = rows.len() as i64;
    rows.extend(media.into_iter().map(|m| old_media_passage(m, offset)));
    Ok(rows)
}

fn old_passage(source_id: &str, modality: Modality, ord: i64, text: String) -> PassageRow {
    PassageRow {
        id: format!("{source_id}:old:{ord}"),
        project_id: String::new(),
        document_id: source_id.to_string(),
        origin: "source".into(),
        scope_ids: Vec::new(),
        access: vec![ACCESS_COMPANY.to_string()],
        unit_id: format!("{source_id}:old:{ord}"),
        modality: modality.as_str().into(),
        ord,
        page: None,
        start_line: None,
        end_line: None,
        start_ms: None,
        end_ms: None,
        section_path: String::new(),
        text,
        file: None,
        chunk_hash: String::new(),
        embedder: String::new(),
        vector: Vec::new(),
    }
}

fn old_chunk_passage(c: ChunkRow) -> PassageRow {
    PassageRow {
        page: c.page.map(i64::from),
        start_ms: c.start_ms.map(|v| v as i64),
        end_ms: c.end_ms.map(|v| v as i64),
        vector: c.vector,
        ..old_passage(&c.source_id, Modality::Text, c.ord as i64, c.text)
    }
}

fn old_media_passage(m: MediaRow, offset: i64) -> PassageRow {
    let modality = match m.kind {
        MediaKind::Image | MediaKind::Page => Modality::Picture,
        MediaKind::Frame => Modality::Frame,
        MediaKind::Audio => Modality::Sound,
    };
    PassageRow {
        page: m.page.map(i64::from),
        start_ms: m.start_ms.map(|v| v as i64),
        end_ms: m.end_ms.map(|v| v as i64),
        file: m.file,
        vector: m.vector,
        ..old_passage(&m.source_id, modality, offset + m.ord as i64, m.text)
    }
}

/// The stretch of a recording a passage covers, when it is read as text (a transcript
/// passage or a sound window).
fn spoken_range(p: &PassageRow) -> Option<(u64, u64)> {
    let text = p.modality == Modality::Text.as_str();
    let sound = p.modality == Modality::Sound.as_str();
    if !(text || sound) {
        return None;
    }
    Some((p.start_ms? as u64, p.end_ms? as u64))
}

/// The picture of a picture or frame passage.
fn picture_of(p: &PassageRow) -> Option<&str> {
    let pictured =
        p.modality == Modality::Picture.as_str() || p.modality == Modality::Frame.as_str();
    pictured.then_some(p.file.as_deref()).flatten()
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

    let mut rows: Vec<PassageRow> = Vec::new();
    for s in &ready {
        rows.extend(candidates(state, &s.id).await?);
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
        for r in &rows {
            if r.modality == Modality::Frame.as_str()
                && let (Some(at), Some(file)) = (r.start_ms, &r.file)
            {
                frames
                    .entry(r.document_id.clone())
                    .or_default()
                    .push((at as u64, file.clone()));
            }
        }
        let mut scored: Vec<(f32, PassageRow)> = rows
            .into_iter()
            // A sound window without a transcript has nothing the writer can read.
            .filter(|r| !(r.modality == Modality::Sound.as_str() && r.text.trim().is_empty()))
            .map(|r| (tools.score(&query_vec, &query, &r.vector, &r.text), r))
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));

        let mut seen_text = HashSet::new();
        let mut spoken: Vec<(String, (u64, u64))> = Vec::new();
        let mut images = 0;
        let mut seen_images: HashSet<Vec<u8>> = HashSet::new();
        let mut picked = Vec::new();
        for (_, r) in scored {
            if picked.len() == PASSAGES {
                break;
            }
            let Some(source) = sources.get(&r.document_id) else {
                continue;
            };
            let Some((original, media_dir)) = media_dirs.get(&r.document_id) else {
                continue;
            };
            let (page, start_ms, end_ms) = (
                r.page.map(|v| v as u32),
                r.start_ms.map(|v| v as u64),
                r.end_ms.map(|v| v as u64),
            );
            if let Some(file) = picture_of(&r) {
                if images == MAX_IMAGES {
                    continue;
                }
                let path = if file == SOURCE_ORIGINAL {
                    original.clone()
                } else {
                    match page {
                        // Pages are rendered when first needed.
                        Some(n) => match tools.pdf_page_picture(original, n, media_dir) {
                            Ok(p) => p,
                            Err(e) => {
                                tracing::warn!("couldn't render page {n} of {}: {e}", source.name);
                                continue;
                            }
                        },
                        None => media_dir.join(file),
                    }
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
                picked.push(retrieved(
                    source,
                    &r,
                    (page, start_ms, end_ms),
                    Some(image),
                    r.file.clone(),
                ));
                continue;
            }
            let normalized = r.text.split_whitespace().collect::<Vec<_>>().join(" ");
            if normalized.is_empty() || !seen_text.insert(normalized) {
                continue;
            }
            if let Some(range) = spoken_range(&r) {
                if spoken
                    .iter()
                    .any(|(id, s)| *id == r.document_id && mostly_overlap(*s, range))
                {
                    continue;
                }
                spoken.push((r.document_id.clone(), range));
            }
            // A picture for the citation: the PDF page, or the video frame nearest the words.
            let picture = match (page, start_ms) {
                (Some(n), _) => tools
                    .pdf_page_picture(original, n, media_dir)
                    .ok()
                    .map(|_| margin_embed::page_file(n)),
                (None, Some(start)) => {
                    let middle = (start + end_ms.unwrap_or(start)) / 2;
                    frames
                        .get(&r.document_id)
                        .and_then(|f| f.iter().min_by_key(|(at, _)| at.abs_diff(middle)))
                        .map(|(_, file)| file.clone())
                }
                _ => None,
            };
            picked.push(retrieved(
                source,
                &r,
                (page, start_ms, end_ms),
                None,
                picture,
            ));
        }
        picked
    })
    .await?;
    Ok(picked)
}

/// A picked passage and its citation.
fn retrieved(
    source: &Source,
    row: &PassageRow,
    (page, start_ms, end_ms): (Option<u32>, Option<u64>, Option<u64>),
    image: Option<Image>,
    picture: Option<String>,
) -> Retrieved {
    let has_doc = !row.embedder.is_empty();
    Retrieved {
        passage: Passage {
            source: source.name.clone(),
            page,
            text: row.text.clone(),
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
            excerpt: row.text.trim().to_string(),
            image_url: picture,
            // Rows the migration hasn't moved yet have no document.
            document_id: has_doc.then(|| row.document_id.clone()),
            start_line: row.start_line.map(|v| v as u32),
            end_line: row.end_line.map(|v| v as u32),
            file: None,
            section_path: Some(row.section_path.trim().to_string()).filter(|s| !s.is_empty()),
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
