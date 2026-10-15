//! The local embedding model (EmbeddingGemma 2 on ONNX Runtime, in the format
//! `service.toml`'s `embedder` names): `GET/POST/DELETE /api/embedding`. Never bundled:
//! downloaded from Hugging Face when the owner asks, loaded on first use, and dropped
//! when deleted. The speech model (Parakeet: `POST/DELETE /api/embedding/speech`) is
//! handled the same way, and the re-index after the embedder changed reports its
//! progress here.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use margin_embed::{EmbedError, Embedder};
use margin_shared::{EmbeddingModel, EmbeddingModelState, Event, SpeechModel};

use crate::error::{AppError, AppResult};
use crate::principal::RequireAdmin;
use crate::state::AppState;
use crate::{ingest, sources};

/// Download progress is sent at most this often.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

/// What the files on disk can't tell: a download running, or how the last one failed.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelJob {
    Idle,
    Downloading { received: u64, total: u64 },
    Failed { error: String },
}

/// Whether a finished download is on disk, and its size.
async fn on_disk(state: &AppState) -> (bool, u64) {
    let tools = state.source_tools.clone();
    let files = state.model_files.clone();
    tokio::task::spawn_blocking(move || {
        if tools.is_complete(&files) {
            (true, tools.size_on_disk(&files))
        } else {
            (false, 0)
        }
    })
    .await
    .unwrap_or((false, 0))
}

/// Bytes a download fetches: asked of Hugging Face once, then remembered. 0 while it
/// can't be reached.
async fn download_bytes(state: &AppState) -> u64 {
    if let Some(bytes) = *state.model_download_bytes.lock().unwrap() {
        return bytes;
    }
    match state
        .source_tools
        .remote_size(&state.model_files, &state.http, &state.hf_base)
        .await
    {
        Ok(bytes) => {
            *state.model_download_bytes.lock().unwrap() = Some(bytes);
            bytes
        }
        Err(e) => {
            tracing::info!("couldn't get the embedding model's download size: {e}");
            0
        }
    }
}

/// Whether the model is downloaded or downloading, so queued sources will get read.
pub async fn will_be_available(state: &AppState) -> bool {
    if matches!(
        *state.model_job.lock().unwrap(),
        ModelJob::Downloading { .. }
    ) {
        return true;
    }
    on_disk(state).await.0
}

pub async fn model(state: &AppState) -> EmbeddingModel {
    let job = state.model_job.lock().unwrap().clone();
    let model_state = match job {
        ModelJob::Downloading { received, total } => {
            EmbeddingModelState::Downloading { received, total }
        }
        ModelJob::Failed { error } => EmbeddingModelState::Failed { error },
        ModelJob::Idle => match on_disk(state).await {
            (true, bytes) => EmbeddingModelState::Ready { bytes },
            (false, _) => EmbeddingModelState::NotDownloaded {
                download_bytes: download_bytes(state).await,
            },
        },
    };
    let model = EmbeddingModel {
        repo: state.model_files.backend.repo().into(),
        state: model_state,
        speech: speech(state).await,
        reindex: *state.reindex.lock().unwrap(),
    };
    *state.last_model.lock().unwrap() = Some(model.clone());
    model
}

/// The speech model's part, when the service downloads one.
async fn speech(state: &AppState) -> Option<SpeechModel> {
    let repo = state.source_tools.speech_model()?.repo();
    let job = state.speech_job.lock().unwrap().clone();
    let speech_state = match job {
        ModelJob::Downloading { received, total } => {
            EmbeddingModelState::Downloading { received, total }
        }
        ModelJob::Failed { error } => EmbeddingModelState::Failed { error },
        ModelJob::Idle => match speech_on_disk(state).await {
            (true, bytes) => EmbeddingModelState::Ready { bytes },
            (false, _) => EmbeddingModelState::NotDownloaded {
                download_bytes: speech_download_bytes(state).await,
            },
        },
    };
    Some(SpeechModel {
        repo,
        state: speech_state,
    })
}

/// Whether the speech model's download is on disk, and its size.
async fn speech_on_disk(state: &AppState) -> (bool, u64) {
    let tools = state.source_tools.clone();
    tokio::task::spawn_blocking(move || match tools.speech_model() {
        Some(m) if m.is_complete() => (true, m.size_on_disk()),
        _ => (false, 0),
    })
    .await
    .unwrap_or((false, 0))
}

/// Bytes the speech model's download fetches: asked once, then remembered.
async fn speech_download_bytes(state: &AppState) -> u64 {
    if let Some(bytes) = *state.speech_download_bytes.lock().unwrap() {
        return bytes;
    }
    let Some(speech) = state.source_tools.speech_model() else {
        return 0;
    };
    match speech.remote_size(&state.http, &state.hf_base).await {
        Ok(bytes) => {
            *state.speech_download_bytes.lock().unwrap() = Some(bytes);
            bytes
        }
        Err(e) => {
            tracing::info!("couldn't get the speech model's download size: {e}");
            0
        }
    }
}

fn emit_model(state: &AppState, model: &EmbeddingModel) {
    state.emit(Event::EmbeddingModelChanged {
        model: model.clone(),
    });
}

/// Sends the last model sent with `change` applied: progress changes only its own part,
/// without asking the disk or Hugging Face again.
fn emit_changed(state: &AppState, change: impl FnOnce(&mut EmbeddingModel)) {
    let model = {
        let mut last = state.last_model.lock().unwrap();
        let Some(model) = last.as_mut() else {
            return;
        };
        change(model);
        model.clone()
    };
    emit_model(state, &model);
}

/// Sends the re-index's progress (`None` once it is done).
pub async fn set_reindex(state: &AppState, progress: Option<margin_shared::ReindexProgress>) {
    *state.reindex.lock().unwrap() = progress;
    if state.last_model.lock().unwrap().is_some() {
        emit_changed(state, |m| m.reindex = progress);
    } else {
        emit_model(state, &model(state).await);
    }
}

pub async fn get_model(State(state): State<AppState>) -> Json<EmbeddingModel> {
    Json(model(&state).await)
}

/// Starts downloading the model in the background. Asking again while it downloads, or
/// once it's downloaded, changes nothing. The models are the organisation's: admins
/// only (403 `admin_required`), like `PATCH /api/settings`.
pub async fn start_download(
    State(state): State<AppState>,
    _admin: RequireAdmin,
) -> AppResult<Json<EmbeddingModel>> {
    let started = {
        // Serializes with deleting the model.
        let _w = state.write_lock.lock().await;
        let downloading = matches!(
            *state.model_job.lock().unwrap(),
            ModelJob::Downloading { .. }
        );
        if downloading || on_disk(&state).await.0 {
            false
        } else {
            let total = state.model_download_bytes.lock().unwrap().unwrap_or(0);
            *state.model_job.lock().unwrap() = ModelJob::Downloading { received: 0, total };
            true
        }
    };
    let current = model(&state).await;
    if started {
        emit_model(&state, &current);
        tokio::spawn(run_download(state.clone()));
    }
    Ok(Json(current))
}

async fn run_download(state: AppState) {
    let last_sent = Mutex::new(Instant::now());
    let progress = |received: u64, total: u64| {
        {
            let mut job = state.model_job.lock().unwrap();
            if !matches!(*job, ModelJob::Downloading { .. }) {
                return;
            }
            *job = ModelJob::Downloading { received, total };
        }
        let mut last = last_sent.lock().unwrap();
        if last.elapsed() < PROGRESS_INTERVAL {
            return;
        }
        *last = Instant::now();
        emit_changed(&state, |m| {
            m.state = EmbeddingModelState::Downloading { received, total }
        });
    };
    let result = state
        .source_tools
        .download(
            &state.model_files,
            &state.download_http,
            &state.hf_base,
            &progress,
        )
        .await;
    *state.model_job.lock().unwrap() = match &result {
        Ok(()) => ModelJob::Idle,
        Err(e) => {
            tracing::warn!("embedding model download failed: {e}");
            ModelJob::Failed {
                error: e.to_string(),
            }
        }
    };
    emit_model(&state, &model(&state).await);
    if result.is_ok() {
        // Sources uploaded meanwhile waited for the model.
        if let Err(e) = sources::enqueue_queued(&state).await {
            tracing::error!("couldn't queue sources after the download: {e:#}");
        }
        // Other documents that waited, and the move to the passage index if it did.
        if let Err(e) = ingest::enqueue_waiting(&state).await {
            tracing::error!("couldn't queue documents after the download: {e:#}");
        }
    }
}

/// Deletes the downloaded model and unloads it. Not while it downloads. Admins only.
pub async fn delete_model(
    State(state): State<AppState>,
    _admin: RequireAdmin,
) -> AppResult<Json<EmbeddingModel>> {
    {
        let _w = state.write_lock.lock().await;
        if matches!(
            *state.model_job.lock().unwrap(),
            ModelJob::Downloading { .. }
        ) {
            return Err(AppError::conflict(
                "model_downloading",
                "The model is still downloading. Delete it once the download finishes.",
            ));
        }
        state.embedder.lock().await.take();
        let tools = state.source_tools.clone();
        let files = state.model_files.clone();
        tokio::task::spawn_blocking(move || tools.remove(&files))
            .await
            .map_err(AppError::internal)?
            .map_err(AppError::internal)?;
        *state.model_job.lock().unwrap() = ModelJob::Idle;
    }
    let current = model(&state).await;
    emit_model(&state, &current);
    Ok(Json(current))
}

/// Starts downloading the speech model in the background. Asking again while it
/// downloads, or once it's downloaded, changes nothing. 409 "no_speech_model" when the
/// source tools have no speech model (a test fake).
pub async fn start_speech_download(
    State(state): State<AppState>,
    _admin: RequireAdmin,
) -> AppResult<Json<EmbeddingModel>> {
    no_speech_model(&state)?;
    let started = {
        let _w = state.write_lock.lock().await;
        let downloading = matches!(
            *state.speech_job.lock().unwrap(),
            ModelJob::Downloading { .. }
        );
        if downloading || speech_on_disk(&state).await.0 {
            false
        } else {
            let total = state.speech_download_bytes.lock().unwrap().unwrap_or(0);
            *state.speech_job.lock().unwrap() = ModelJob::Downloading { received: 0, total };
            true
        }
    };
    let current = model(&state).await;
    if started {
        emit_model(&state, &current);
        tokio::spawn(run_speech_download(state.clone()));
    }
    Ok(Json(current))
}

fn no_speech_model(state: &AppState) -> AppResult<()> {
    if state.source_tools.speech_model().is_none() {
        return Err(AppError::conflict(
            "no_speech_model",
            "This service has no speech model to download.",
        ));
    }
    Ok(())
}

async fn run_speech_download(state: AppState) {
    let last_sent = Mutex::new(Instant::now());
    let progress = |received: u64, total: u64| {
        {
            let mut job = state.speech_job.lock().unwrap();
            if !matches!(*job, ModelJob::Downloading { .. }) {
                return;
            }
            *job = ModelJob::Downloading { received, total };
        }
        let mut last = last_sent.lock().unwrap();
        if last.elapsed() < PROGRESS_INTERVAL {
            return;
        }
        *last = Instant::now();
        emit_changed(&state, |m| {
            if let Some(s) = m.speech.as_mut() {
                s.state = EmbeddingModelState::Downloading { received, total };
            }
        });
    };
    let result = match state.source_tools.speech_model() {
        Some(speech) => {
            speech
                .download(&state.download_http, &state.hf_base, &progress)
                .await
        }
        None => Ok(()),
    };
    *state.speech_job.lock().unwrap() = match &result {
        Ok(()) => ModelJob::Idle,
        Err(e) => {
            tracing::warn!("speech model download failed: {e}");
            ModelJob::Failed {
                error: e.to_string(),
            }
        }
    };
    emit_model(&state, &model(&state).await);
}

/// Deletes the downloaded speech model and unloads it. Not while it downloads. Admins
/// only.
pub async fn delete_speech(
    State(state): State<AppState>,
    _admin: RequireAdmin,
) -> AppResult<Json<EmbeddingModel>> {
    no_speech_model(&state)?;
    {
        let _w = state.write_lock.lock().await;
        if matches!(
            *state.speech_job.lock().unwrap(),
            ModelJob::Downloading { .. }
        ) {
            return Err(AppError::conflict(
                "model_downloading",
                "The speech model is still downloading. Delete it once the download finishes.",
            ));
        }
        let tools = state.source_tools.clone();
        tokio::task::spawn_blocking(move || match tools.speech_model() {
            Some(m) => m.remove(),
            None => Ok(()),
        })
        .await
        .map_err(AppError::internal)?
        .map_err(AppError::internal)?;
        *state.speech_job.lock().unwrap() = ModelJob::Idle;
    }
    let current = model(&state).await;
    emit_model(&state, &current);
    Ok(Json(current))
}

/// The embedding model, loaded on first use (compiling it the first time takes a few
/// seconds). `None` while it isn't downloaded.
pub async fn embedder(state: &AppState) -> Result<Option<Arc<dyn Embedder>>, EmbedError> {
    let mut slot = state.embedder.lock().await;
    if let Some(e) = slot.as_ref() {
        return Ok(Some(e.clone()));
    }
    if matches!(
        *state.model_job.lock().unwrap(),
        ModelJob::Downloading { .. }
    ) {
        return Ok(None);
    }
    let tools = state.source_tools.clone();
    let files = state.model_files.clone();
    let loaded = tokio::task::spawn_blocking(move || {
        if !tools.is_complete(&files) {
            return Ok(None);
        }
        tools.load(&files).map(Some)
    })
    .await
    .map_err(|e| EmbedError::Load(e.to_string()))??;
    if loaded.is_some() {
        tracing::info!("embedding model loaded");
    }
    slot.clone_from(&loaded);
    Ok(loaded)
}
