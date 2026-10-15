//! Sources uploaded in parts (`routes::SOURCE_UPLOADS`): a file bigger than
//! `routes::UPLOAD_PART_BYTES` goes up as a series of parts, each its own request, so no
//! request is bigger than the proxy in front of the service accepts. The file grows in
//! `data_dir/uploads/{id}/file` next to `upload.json` (its name, folder, topic and size)
//! and, once all of it is there, moves into `data_dir/sources/` as a Queued source.
//! Unfinished uploads are deleted at startup once they're a day old.

use std::path::PathBuf;

use axum::Json;
use axum::body::Body;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use chrono::{DateTime, Utc};
use margin_shared::{Id, Source, SourceUpload, routes::UPLOAD_PART_BYTES};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::principal::{BOOTSTRAP_PERSON_ID, CurrentDevice, CurrentPrincipal, Principal};
use crate::sources::{self, FILE_NAME, Upload, UploadQuery, content_length, receive, remove_dir};
use crate::state::{AppState, new_id};
use margin_shared::sync::Locked;

const META_FILE: &str = "upload.json";
/// Unfinished uploads older than this are deleted at startup.
pub const UNFINISHED_TTL: chrono::Duration = chrono::Duration::hours(24);

/// `upload.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Meta {
    upload: Upload,
    bytes: u64,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct StartQuery {
    #[serde(default)]
    name: String,
    folder: Option<String>,
    topic_id: Option<Id>,
    #[serde(default)]
    workspace_id: Option<Id>,
    #[serde(default)]
    modified: Option<DateTime<Utc>>,
    bytes: u64,
}

#[derive(Debug, Deserialize)]
pub struct PartQuery {
    offset: u64,
}

fn dir(state: &AppState, id: &str) -> AppResult<PathBuf> {
    // Ids are uuids; anything else could name a path outside the folder.
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(AppError::not_found("upload"));
    }
    Ok(state.config.uploads_dir().join(id))
}

/// The upload, when it is `p`'s (404 for anyone else's: it doesn't exist for them).
async fn meta(state: &AppState, p: &Principal, id: &str) -> AppResult<(PathBuf, Meta)> {
    let (dir, meta) = any_meta(state, id).await?;
    let owner = meta.upload.owner.as_deref().unwrap_or(BOOTSTRAP_PERSON_ID);
    if owner != p.person_id {
        return Err(AppError::not_found("upload"));
    }
    Ok((dir, meta))
}

async fn any_meta(state: &AppState, id: &str) -> AppResult<(PathBuf, Meta)> {
    let dir = dir(state, id)?;
    let bytes = match tokio::fs::read(dir.join(META_FILE)).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(AppError::not_found("upload"));
        }
        Err(e) => return Err(AppError::internal(format!("{}: {e}", dir.display()))),
    };
    let meta = serde_json::from_slice(&bytes)
        .map_err(|e| AppError::internal(format!("{}: {e}", dir.display())))?;
    Ok((dir, meta))
}

async fn received(dir: &std::path::Path) -> AppResult<u64> {
    match tokio::fs::metadata(dir.join(FILE_NAME)).await {
        Ok(m) => Ok(m.len()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(AppError::internal(format!("{}: {e}", dir.display()))),
    }
}

/// Marks an upload busy while one request works on it; a second at once gets 409.
struct Busy<'a> {
    state: &'a AppState,
    id: Id,
}

impl<'a> Busy<'a> {
    fn claim(state: &'a AppState, id: &str) -> AppResult<Self> {
        let mut busy = state.uploads_busy.locked();
        if !busy.insert(id.to_string()) {
            return Err(AppError::conflict(
                "upload_busy",
                "Another part of this upload is still arriving.",
            ));
        }
        Ok(Self {
            state,
            id: id.to_string(),
        })
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.state.uploads_busy.locked().remove(&self.id);
    }
}

/// `POST /api/source-uploads?name&folder&topic_id&bytes` (plus `workspace_id` and
/// `modified` for a folder's file): checks the file's name and size (413 over its
/// kind's limit; a folder's files have none) and starts an upload in parts.
pub async fn start(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    CurrentDevice(device): CurrentDevice,
    query: Result<Query<StartQuery>, QueryRejection>,
) -> AppResult<Json<SourceUpload>> {
    let Query(q) =
        query.map_err(|rej| AppError::new(rej.status(), "bad_request", rej.body_text()))?;
    let source = UploadQuery {
        name: q.name,
        folder: q.folder,
        topic_id: q.topic_id,
        workspace_id: q.workspace_id,
        modified: q.modified,
    };
    let upload = Upload::check(&state, source, &p, &device).await?;
    if q.bytes > upload.limit() {
        return Err(upload.too_large());
    }
    let id = new_id();
    let dir = dir(&state, &id)?;
    let meta = Meta {
        upload,
        bytes: q.bytes,
        created_at: Utc::now(),
    };
    let saved = async {
        tokio::fs::create_dir_all(&dir).await?;
        tokio::fs::write(dir.join(META_FILE), serde_json::to_vec(&meta)?).await?;
        anyhow::Ok(())
    }
    .await;
    if let Err(e) = saved {
        remove_dir(&dir).await;
        return Err(AppError::internal(format!("{}: {e:#}", dir.display())));
    }
    Ok(Json(SourceUpload {
        id,
        bytes: q.bytes,
        received: 0,
    }))
}

/// `GET /api/source-uploads/{id}`: how much of it the service has.
pub async fn get(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    Path(id): Path<Id>,
) -> AppResult<Json<SourceUpload>> {
    let (dir, meta) = meta(&state, &p, &id).await?;
    Ok(Json(SourceUpload {
        id,
        bytes: meta.bytes,
        received: received(&dir).await?,
    }))
}

/// `PUT /api/source-uploads/{id}?offset`: appends a part of at most `UPLOAD_PART_BYTES`
/// at `offset`, which must be what the service has so far (409 "wrong_offset"
/// otherwise; `GET` says where to go on).
pub async fn put_part(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    Path(id): Path<Id>,
    headers: HeaderMap,
    query: Result<Query<PartQuery>, QueryRejection>,
    body: Body,
) -> AppResult<Json<SourceUpload>> {
    let Query(q) =
        query.map_err(|rej| AppError::new(rej.status(), "bad_request", rej.body_text()))?;
    let (dir, meta) = meta(&state, &p, &id).await?;
    let _busy = Busy::claim(&state, &id)?;
    let have = received(&dir).await?;
    if q.offset != have {
        return Err(AppError::conflict(
            "wrong_offset",
            format!(
                "The service has {have} bytes of {}; the next part starts there.",
                meta.upload.name
            ),
        ));
    }
    let max = UPLOAD_PART_BYTES.min(meta.bytes - have);
    if content_length(&headers).is_some_and(|n| n > max) {
        return Err(part_too_large(&meta, have));
    }
    let path = dir.join(FILE_NAME);
    match receive(body, &path, max).await {
        Ok((n, _)) => Ok(Json(SourceUpload {
            id,
            bytes: meta.bytes,
            received: have + n,
        })),
        Err(sources::ReceiveError::TooLarge) => Err(part_too_large(&meta, have)),
        Err(e) => Err(e.into_app_error(&meta.upload, &path)),
    }
}

fn part_too_large(meta: &Meta, have: u64) -> AppError {
    if meta.bytes - have < UPLOAD_PART_BYTES {
        AppError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            format!(
                "That's more of {} than the {} bytes announced.",
                meta.upload.name, meta.bytes
            ),
        )
    } else {
        AppError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            format!(
                "Parts of an upload can be at most {} MB.",
                UPLOAD_PART_BYTES / (1024 * 1024)
            ),
        )
    }
}

/// `POST /api/source-uploads/{id}/finish`: the whole file is there; it becomes a Queued
/// source (409 "incomplete_upload" while parts are missing).
pub async fn finish(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    Path(id): Path<Id>,
) -> AppResult<Json<Source>> {
    let (dir, meta) = meta(&state, &p, &id).await?;
    let _busy = Busy::claim(&state, &id)?;
    let have = received(&dir).await?;
    if have != meta.bytes {
        return Err(AppError::conflict(
            "incomplete_upload",
            format!(
                "The service has {have} of {} bytes of {}.",
                meta.bytes, meta.upload.name
            ),
        ));
    }
    let source_id = new_id();
    let target = state.source_dir(&source_id);
    let moved = async {
        tokio::fs::create_dir_all(&target).await?;
        if have == 0 {
            tokio::fs::write(target.join(FILE_NAME), b"").await?;
        } else {
            tokio::fs::rename(dir.join(FILE_NAME), target.join(FILE_NAME)).await?;
        }
        std::io::Result::Ok(())
    }
    .await;
    if let Err(e) = moved {
        remove_dir(&target).await;
        return Err(AppError::internal(format!(
            "couldn't move {} into {}: {e}",
            dir.display(),
            target.display()
        )));
    }
    remove_dir(&dir).await;
    // What arrived, hashed once (a folder's row records it).
    let sha256 = match sources::hash_file(&target.join(FILE_NAME)).await {
        Ok(h) => h,
        Err(e) => {
            remove_dir(&target).await;
            return Err(AppError::internal(format!(
                "couldn't read {}: {e}",
                target.display()
            )));
        }
    };
    let source = sources::create_or_replace(&state, &source_id, meta.upload, have, sha256).await?;
    Ok(Json(source))
}

/// `DELETE /api/source-uploads/{id}`: gives an upload up.
pub async fn cancel(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    Path(id): Path<Id>,
) -> AppResult<StatusCode> {
    let (dir, _) = meta(&state, &p, &id).await?;
    let _busy = Busy::claim(&state, &id)?;
    remove_dir(&dir).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes unfinished uploads older than `UNFINISHED_TTL` (and folders without a
/// readable `upload.json`). Run at startup.
pub async fn purge_stale(state: &AppState) -> anyhow::Result<()> {
    let root = state.config.uploads_dir();
    let mut entries = match tokio::fs::read_dir(&root).await {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let now = Utc::now();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let meta: Option<Meta> = tokio::fs::read(path.join(META_FILE))
            .await
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        let stale = meta.is_none_or(|m| now - m.created_at > UNFINISHED_TTL);
        if stale {
            tracing::info!("deleting unfinished upload {}", path.display());
            remove_dir(&path).await;
        }
    }
    Ok(())
}
