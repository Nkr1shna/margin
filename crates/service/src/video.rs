//! Video jobs: one render at a time, through a queue.
//!
//! Queued → Scripting (`write_scene`) → Voicing/Rendering (`render`) → Ready. When the
//! scene fails to run, the model gets one chance to repair it (`FailedScene`).

use std::collections::VecDeque;
use std::path::{Path as FsPath, PathBuf};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, State};
use chrono::Utc;
use margin_ai::{CallOptions, FailedScene, Generated, SceneRequest};
use margin_shared::context::TraceKind;
use margin_shared::pipeline::Route;
use margin_shared::{Id, LessonStatus, ToolStatus, Video, VideoStatus};
use margin_video::{
    RenderConfig, RenderError, RenderJob, RenderOutput, RenderProgress, SandboxMode, Voice,
};
use tokio::sync::mpsc;

use crate::db;
use crate::error::{AppError, AppResult};
use crate::library::{self, video_for_lesson};
use crate::state::{
    AppState, CAPTIONS_FILE, MP4_FILE, POSTER_FILE, STORYBOARD_FILE, StoredVideo, new_id,
};
use crate::trace::{self, SpanGuard};

pub const LOG_LINES: usize = 200;
/// Progress-only updates are saved and sent at most this often.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);
/// Log-only updates are saved and sent at most this often.
const LOG_INTERVAL: Duration = Duration::from_secs(2);

pub fn in_progress(status: &VideoStatus) -> bool {
    matches!(
        status,
        VideoStatus::Queued
            | VideoStatus::Scripting
            | VideoStatus::Voicing { .. }
            | VideoStatus::Rendering { .. }
    )
}

pub async fn request_video(
    State(state): State<AppState>,
    Path(lesson_id): Path<Id>,
) -> AppResult<Json<Video>> {
    let lesson = library::lesson(&state, &lesson_id).await?;
    if lesson.lesson.status != LessonStatus::Ready {
        return Err(AppError::conflict(
            "lesson_not_ready",
            "Make a video once the lesson is ready.",
        ));
    }
    state.ai_config().await?;
    let video = enqueue(&state, &lesson_id).await?;
    Ok(Json(state.public_video(&video)))
}

pub async fn get_video(
    State(state): State<AppState>,
    Path(id): Path<Id>,
) -> AppResult<Json<Video>> {
    let v: StoredVideo = state
        .store
        .get(db::VIDEO, &id)
        .await?
        .ok_or_else(|| AppError::not_found("video"))?;
    Ok(Json(state.public_video(&v)))
}

/// Creates the lesson's video or re-queues it. A video already in the pipeline is
/// returned unchanged.
pub async fn enqueue(state: &AppState, lesson_id: &str) -> AppResult<StoredVideo> {
    let (video, queued) = {
        let _w = state.write_lock.lock().await;
        library::lesson(state, lesson_id).await?;
        let now = Utc::now();
        match video_for_lesson(state, lesson_id).await? {
            Some(v) if in_progress(&v.video.status) => (v, false),
            Some(mut v) => {
                v.video.status = VideoStatus::Queued;
                v.video.log = String::new();
                v.video.duration_seconds = None;
                v.video.updated_at = now;
                v.has_captions = false;
                v.has_poster = false;
                state.store.put(db::VIDEO, &v.video.id, &v).await?;
                (v, true)
            }
            None => {
                let v = StoredVideo {
                    video: Video {
                        id: new_id(),
                        lesson_id: lesson_id.to_string(),
                        status: VideoStatus::Queued,
                        scene_code: None,
                        log: String::new(),
                        duration_seconds: None,
                        media: None,
                        created_at: now,
                        updated_at: now,
                    },
                    has_captions: false,
                    has_poster: false,
                };
                state.store.put(db::VIDEO, &v.video.id, &v).await?;
                (v, true)
            }
        }
    };
    if queued {
        state.emit_video(&video);
        let _ = state.video_queue.send(video.video.id.clone());
    }
    Ok(video)
}

/// Renders queued videos one at a time, forever.
pub async fn run_worker(state: AppState, mut queue: mpsc::UnboundedReceiver<Id>) {
    while let Some(id) = queue.recv().await {
        let task = {
            let mut slot = state.render_job.lock().unwrap();
            let task = tokio::spawn(process(state.clone(), id.clone()));
            *slot = Some((id.clone(), task.abort_handle()));
            task
        };
        match task.await {
            Ok(()) => {}
            Err(e) if e.is_cancelled() => tracing::info!("render of video {id} was stopped"),
            Err(e) => tracing::error!("render of video {id} panicked: {e}"),
        }
        let mut slot = state.render_job.lock().unwrap();
        if slot.as_ref().is_some_and(|(cur, _)| cur == &id) {
            *slot = None;
        }
    }
}

async fn process(state: AppState, id: Id) {
    if let Err(e) = pipeline(&state, &id).await {
        tracing::error!("video {id}: {e:#}");
        let _ = fail(&state, &id, &format!("{e:#}"), None).await;
    }
}

/// Applies `f` to the stored video under the write lock and emits it. Returns false
/// when the video no longer exists.
async fn update(
    state: &AppState,
    id: &str,
    f: impl FnOnce(&mut StoredVideo),
) -> anyhow::Result<bool> {
    let video = {
        let _w = state.write_lock.lock().await;
        let Some(mut v) = state.store.get::<StoredVideo>(db::VIDEO, id).await? else {
            return Ok(false);
        };
        f(&mut v);
        v.video.updated_at = Utc::now();
        state.store.put(db::VIDEO, id, &v).await?;
        v
    };
    state.emit_video(&video);
    Ok(true)
}

async fn fail(state: &AppState, id: &str, error: &str, log: Option<String>) -> anyhow::Result<()> {
    tracing::warn!("video {id} failed: {error}");
    let error = error.to_string();
    update(state, id, move |v| {
        v.video.status = VideoStatus::Failed { error };
        if let Some(log) = log {
            v.video.log = log;
        }
    })
    .await?;
    Ok(())
}

fn tool<'a>(tools: &'a [ToolStatus], name: &str) -> Option<&'a ToolStatus> {
    tools.iter().find(|t| t.name == name && t.found)
}

fn tool_path(t: &ToolStatus) -> PathBuf {
    PathBuf::from(t.path.clone().unwrap_or_else(|| t.name.clone()))
}

/// Scene code renders under macOS `sandbox-exec` unless the service was started with
/// `--allow-unsandboxed-render` (development and tests only). Where `sandbox-exec`
/// isn't available, `margin_video` refuses with `RenderError::Sandbox`.
pub fn sandbox_mode(allow_unsandboxed: bool) -> SandboxMode {
    if allow_unsandboxed {
        SandboxMode::Unsandboxed
    } else {
        SandboxMode::MacosSandboxExec
    }
}

async fn pipeline(state: &AppState, id: &str) -> anyhow::Result<()> {
    let Some(stored) = state.store.get::<StoredVideo>(db::VIDEO, id).await? else {
        return Ok(());
    };
    if stored.video.status != VideoStatus::Queued {
        return Ok(());
    }
    let Ok(lesson) = library::lesson(state, &stored.video.lesson_id).await else {
        return Ok(());
    };
    if lesson.lesson.status != LessonStatus::Ready {
        return fail(state, id, "The lesson isn't ready yet.", None).await;
    }

    // Tools first, so a missing tool costs no model call.
    let tools = state.renderer.detect_tools().await;
    let Some(manimgx) = tool(&tools, "manimgx") else {
        return fail(
            state,
            id,
            &RenderError::ToolMissing("manimgx".into()).to_string(),
            None,
        )
        .await;
    };
    let Some(fluidaudio) = tool(&tools, "fluidaudiocli") else {
        return fail(
            state,
            id,
            &RenderError::ToolMissing("fluidaudiocli".into()).to_string(),
            None,
        )
        .await;
    };
    let ai = match state.route_ai_config(Route::Write).await {
        Ok(ai) => ai,
        Err(e) => return fail(state, id, &e.message, None).await,
    };
    let settings = state.settings_row().await?;
    let config = RenderConfig {
        manimgx: tool_path(manimgx),
        voice: Voice::PocketTts {
            cli: tool_path(fluidaudio),
            voice: settings.voice.clone(),
        },
        sandbox: sandbox_mode(state.allow_unsandboxed_render),
        width: settings.render_width,
        height: settings.render_height,
        fps: settings.render_fps,
    };

    let mut log = LogTail::default();
    log.push("Writing the scene…".into());
    let log_text = log.text();
    if !update(state, id, move |v| {
        v.video.status = VideoStatus::Scripting;
        v.video.log = log_text;
        v.video.duration_seconds = None;
        v.has_captions = false;
        v.has_poster = false;
    })
    .await?
    {
        return Ok(());
    }

    let mut request = SceneRequest {
        lesson_title: lesson.lesson.title.clone(),
        lesson_body_md: lesson.lesson.body_md.clone(),
        previous_attempt: None,
    };
    let work_dir = state.video_dir(id);
    for attempt in 1..=2 {
        let mut call = SpanGuard::new(
            trace::model_call(
                state,
                id,
                Route::Write,
                &ai.model,
                trace::provider_of(&ai.base_url),
                SCENE_PROMPT_VERSION.to_string(),
            ),
            state,
        );
        call.row().job_id = Some(id.to_string());
        call.row().step = Some(attempt);
        let opts = CallOptions::new(Route::Write);
        let code = match state.generator.write_scene(&ai, &request, &opts).await {
            Ok(Generated { value: code, usage }) => {
                call.row().usage = Some(trace::trace_usage(&usage));
                call.finish(true);
                code
            }
            Err(e) => {
                call.fail(trace::ai_error_code(&e));
                return fail(state, id, &e.to_string(), Some(log.text())).await;
            }
        };
        log.push(if attempt == 1 {
            "Scene written. Rendering…".into()
        } else {
            "Repaired scene written. Rendering again…".into()
        });
        let (scene, log_text) = (code.clone(), log.text());
        if !update(state, id, move |v| {
            v.video.scene_code = Some(scene);
            v.video.log = log_text;
        })
        .await?
        {
            return Ok(());
        }

        reset_dir(&work_dir).await?;
        let job = RenderJob {
            work_dir: work_dir.clone(),
            scene_code: code.clone(),
        };
        let mut render = SpanGuard::new(state.tracer.span(TraceKind::JobStep, id), state);
        render.row().job_id = Some(id.to_string());
        render.row().tool = Some("render".into());
        render.row().step = Some(attempt);
        render.row().pipeline_hash = Some(state.pipeline.load().hash.clone());
        let (result, tail) = render_with_progress(state, id, &job, &config, log).await;
        match &result {
            Ok(_) => render.finish(true),
            Err(e) => render.fail(render_error_code(e)),
        }
        log = tail;
        match result {
            Ok(out) => return finish(state, id, &work_dir, out, log).await,
            Err(RenderError::Scene { message }) if attempt == 1 => {
                log.push(format!("The scene failed: {message}"));
                log.push("Asking the model to repair the scene…".into());
                let log_text = log.text();
                if !update(state, id, move |v| {
                    v.video.status = VideoStatus::Scripting;
                    v.video.log = log_text;
                })
                .await?
                {
                    return Ok(());
                }
                request.previous_attempt = Some(FailedScene {
                    code,
                    error: message,
                });
            }
            Err(e) => {
                let error = e.to_string();
                log.push(error.clone());
                return fail(state, id, &error, Some(log.text())).await;
            }
        }
    }
    Ok(())
}

/// The scene prompt's version as traces record it (`margin_ai`'s constant-first prompt).
const SCENE_PROMPT_VERSION: &str = margin_ai::prompts::SCENE_VERSION;

/// A render failure as a trace code (never its message).
fn render_error_code(e: &RenderError) -> &'static str {
    match e {
        RenderError::ToolMissing(_) => "tool_missing",
        RenderError::Scene { .. } => "scene_failed",
        RenderError::Voice(_) => "voice_failed",
        RenderError::Sandbox => "sandbox",
        RenderError::Io(_) => "io",
    }
}

async fn reset_dir(dir: &FsPath) -> anyhow::Result<()> {
    match tokio::fs::remove_dir_all(dir).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(anyhow::anyhow!("couldn't clear {}: {e}", dir.display())),
    }
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| anyhow::anyhow!("couldn't create {}: {e}", dir.display()))
}

/// Moves an output to the name the media route serves.
async fn place(src: &FsPath, dst: &FsPath) -> anyhow::Result<()> {
    if src == dst {
        return Ok(());
    }
    if tokio::fs::rename(src, dst).await.is_err() {
        tokio::fs::copy(src, dst)
            .await
            .map_err(|e| anyhow::anyhow!("couldn't copy {}: {e}", src.display()))?;
        let _ = tokio::fs::remove_file(src).await;
    }
    Ok(())
}

async fn finish(
    state: &AppState,
    id: &str,
    dir: &FsPath,
    out: RenderOutput,
    mut log: LogTail,
) -> anyhow::Result<()> {
    place(&out.mp4, &dir.join(MP4_FILE)).await?;
    place(&out.storyboard, &dir.join(STORYBOARD_FILE)).await?;
    let has_captions = match &out.captions {
        Some(c) => {
            place(c, &dir.join(CAPTIONS_FILE)).await?;
            true
        }
        None => false,
    };
    let has_poster = match &out.poster {
        Some(p) => {
            place(p, &dir.join(POSTER_FILE)).await?;
            true
        }
        None => false,
    };
    log.push("Done.".into());
    let log_text = log.text();
    let exists = update(state, id, move |v| {
        v.video.status = VideoStatus::Ready;
        v.video.duration_seconds = out.duration_seconds;
        v.video.log = log_text;
        v.has_captions = has_captions;
        v.has_poster = has_poster;
    })
    .await?;
    if !exists {
        // Deleted while rendering: don't leave files behind.
        let _ = tokio::fs::remove_dir_all(dir).await;
    }
    Ok(())
}

/// The last `LOG_LINES` lines of the render log.
#[derive(Default)]
pub struct LogTail {
    lines: VecDeque<String>,
}

impl LogTail {
    pub fn push(&mut self, line: String) {
        for l in line.lines() {
            if self.lines.len() == LOG_LINES {
                self.lines.pop_front();
            }
            self.lines.push_back(l.to_string());
        }
    }
    pub fn text(&self) -> String {
        self.lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Runs the render while a companion loop saves progress and the log, throttled.
async fn render_with_progress(
    state: &AppState,
    id: &str,
    job: &RenderJob,
    config: &RenderConfig,
    log: LogTail,
) -> (Result<RenderOutput, RenderError>, LogTail) {
    let (tx, rx) = mpsc::unbounded_channel::<RenderProgress>();
    let saver = tokio::spawn(save_progress(state.clone(), id.to_string(), rx, log));
    let on_progress = Box::new(move |p: RenderProgress| {
        let _ = tx.send(p);
    });
    let result = state.renderer.render(job, config, on_progress).await;
    // `on_progress` (and the sender) is dropped once render returns, ending the saver.
    let log = saver.await.unwrap_or_default();
    (result, log)
}

async fn save_progress(
    state: AppState,
    id: Id,
    mut rx: mpsc::UnboundedReceiver<RenderProgress>,
    mut log: LogTail,
) -> LogTail {
    let mut status: Option<VideoStatus> = None;
    let mut saved_status: Option<VideoStatus> = None;
    let mut log_dirty = false;
    let mut last_save = Instant::now() - LOG_INTERVAL;
    loop {
        let next = tokio::time::timeout(PROGRESS_INTERVAL, rx.recv()).await;
        let closed = matches!(next, Ok(None));
        match next {
            Ok(Some(RenderProgress::Log(line))) => {
                log.push(line);
                log_dirty = true;
            }
            Ok(Some(RenderProgress::Voicing { done, total })) => {
                status = Some(VideoStatus::Voicing { done, total })
            }
            Ok(Some(RenderProgress::Rendering { progress })) => {
                status = Some(VideoStatus::Rendering {
                    progress: progress.map(|p| p.clamp(0.0, 1.0)),
                })
            }
            Ok(None) | Err(_) => {}
        }
        let phase_changed = match (&status, &saved_status) {
            (Some(a), Some(b)) => std::mem::discriminant(a) != std::mem::discriminant(b),
            (Some(_), None) => true,
            _ => false,
        };
        let progress_due = status != saved_status && last_save.elapsed() >= PROGRESS_INTERVAL;
        let log_due = log_dirty && last_save.elapsed() >= LOG_INTERVAL;
        let due = closed || phase_changed || progress_due || log_due;
        if due && (status != saved_status || log_dirty) {
            let (s, text) = (status.clone(), log.text());
            let saved = update(&state, &id, move |v| {
                if let Some(s) = s {
                    v.video.status = s;
                }
                v.video.log = text;
            })
            .await;
            if let Err(e) = saved {
                tracing::warn!("couldn't save render progress: {e:#}");
            }
            saved_status = status.clone();
            log_dirty = false;
            last_save = Instant::now();
        }
        if closed {
            return log;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_tail_keeps_last_lines() {
        let mut t = LogTail::default();
        for i in 0..250 {
            t.push(format!("line {i}"));
        }
        let text = t.text();
        assert_eq!(text.lines().count(), LOG_LINES);
        assert!(text.starts_with("line 50\n"));
        assert!(text.ends_with("line 249"));
    }

    #[test]
    fn unsandboxed_only_with_the_flag() {
        assert_eq!(sandbox_mode(true), SandboxMode::Unsandboxed);
        assert_eq!(sandbox_mode(false), SandboxMode::MacosSandboxExec);
    }
}
