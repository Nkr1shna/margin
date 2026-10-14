//! Renders narrated lesson videos with ManimGX, voiced by PocketTTS through the
//! FluidAudio CLI (`fluidaudiocli tts … --backend pocket`).
//!
//! Pipeline for one render, all inside `RenderJob::work_dir`:
//! 1. Write the scene (`scene.py`) plus a voice module the renderer controls.
//! 2. Pass 1 (sandboxed): `manimgx inspect` with a collecting voice that records every
//!    line the scene says (and returns silence), so the lines are known up front.
//! 3. Voice each line with `fluidaudiocli` (NOT sandboxed: it downloads models and
//!    caches them in the user's library). Report `Voicing { done, total }`.
//! 4. Pass 2 (sandboxed): `manimgx render` with a voice that returns the WAV made in
//!    step 3 for each line. ManimGX mixes audio, writes the MP4, storyboard and captions.
//!
//! Sandboxing (macOS `sandbox-exec`): no network, writes only inside `work_dir` (plus
//! the temp dirs Python needs), reads allowed. On other platforms rendering is refused
//! unless `SandboxMode::Unsandboxed` is chosen explicitly (the service exposes that only
//! through `--allow-unsandboxed-render`, for testing).
//!
//! STUB: signatures are the contract with `margin-service`.

use std::path::PathBuf;

use margin_shared::ToolStatus;

/// Finds `manimgx` and `fluidaudiocli` on PATH (and in `~/.local/bin`), with versions
/// and the install commands to show when missing.
pub async fn detect_tools() -> Vec<ToolStatus> {
    todo!()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// macOS `sandbox-exec` profile. Rendering fails with `RenderError::Sandbox` elsewhere.
    MacosSandboxExec,
    /// No sandbox. Only for development and tests.
    Unsandboxed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Voice {
    /// PocketTTS through the FluidAudio CLI. `voice` is passed as `--voice` when set.
    PocketTts { cli: PathBuf, voice: Option<String> },
    /// Silence of a plausible length per line. For tests on machines without FluidAudio.
    Silent,
}

#[derive(Debug, Clone)]
pub struct RenderConfig {
    pub manimgx: PathBuf,
    pub voice: Voice,
    pub sandbox: SandboxMode,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

#[derive(Debug, Clone)]
pub struct RenderJob {
    /// Empty or reusable directory owned by this video. Outputs land here.
    pub work_dir: PathBuf,
    /// Python source from `margin_ai::write_scene` (class `LessonScene`, no `voice`).
    pub scene_code: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RenderProgress {
    Voicing { done: u32, total: u32 },
    Rendering { progress: Option<f32> },
    /// One line of tool output, for the video's log.
    Log(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct RenderOutput {
    pub mp4: PathBuf,
    pub storyboard: PathBuf,
    pub captions: Option<PathBuf>,
    pub duration_seconds: Option<f32>,
}

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("{0} isn't installed")]
    ToolMissing(String),
    /// The scene failed to run. `message` is ManimGX's error (the failing line and its
    /// hint) and is suitable for `margin_ai::FailedScene::error`.
    #[error("the scene failed: {message}")]
    Scene { message: String },
    #[error("couldn't voice the narration: {0}")]
    Voice(String),
    #[error("rendering needs the macOS sandbox, which isn't available here")]
    Sandbox,
    #[error("{0}")]
    Io(String),
}

/// Runs the whole pipeline. `on_progress` is called from the rendering task in order.
pub async fn render(
    job: &RenderJob,
    config: &RenderConfig,
    on_progress: impl FnMut(RenderProgress) + Send,
) -> Result<RenderOutput, RenderError> {
    let _ = (job, config, on_progress);
    todo!()
}
