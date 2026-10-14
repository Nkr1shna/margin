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
//! Files in `work_dir` after a render:
//! - `video.mp4`, `storyboard.png` (more sheets: `storyboard-2.png`, …) and, when the
//!   scene speaks, `captions.srt`: the outputs.
//! - `scene.py`: the scene as given, plus two appended lines that install the voice
//!   (`import margin_voice …; _margin_voice.attach(globals())`).
//! - `margin_voice.py`: the voice module (see that file).
//! - `.margin/`: the renderer's state: `pass.json` (which pass, and the audio of each
//!   line), `lines.json` (the lines pass 1 found) and `narration/*.wav`. PocketTTS WAVs
//!   are kept between renders and reused for a line with the same text and voice.
//! - `voice/`: ManimGX's own cache of what the voice said; cleared before each pass.

mod mp4;
mod process;
mod sandbox;
mod scene_error;
mod tools;
mod wav;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use margin_shared::ToolStatus;
use tokio::process::Command;

use crate::process::{Stream, run_streaming};

/// Finds `manimgx` and `fluidaudiocli` on PATH (and in `~/.local/bin`), with versions
/// and the install commands to show when missing.
pub async fn detect_tools() -> Vec<ToolStatus> {
    tools::detect_tools().await
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
    Voicing {
        done: u32,
        total: u32,
    },
    Rendering {
        progress: Option<f32>,
    },
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

/// File names of the outputs in `work_dir`.
pub const VIDEO_FILE: &str = "video.mp4";
pub const STORYBOARD_FILE: &str = "storyboard.png";
pub const CAPTIONS_FILE: &str = "captions.srt";

/// The scene's file and class, the voice module, and the renderer's state folder.
const SCENE_FILE: &str = "scene.py";
const SCENE_CLASS: &str = "LessonScene";
const VOICE_MODULE_FILE: &str = "margin_voice.py";
const VOICE_MODULE: &str = include_str!("margin_voice.py");
const STATE_DIR: &str = ".margin";
const NARRATION_DIR: &str = "narration";
const MANIMGX_VOICE_CACHE: &str = "voice";

/// Appended to the scene: installs the pass's voice on `LessonScene`.
const SCENE_SUFFIX: &str = "\n\n# Added by Margin's renderer: the narration voice (see margin_voice.py).\nimport margin_voice as _margin_voice\n_margin_voice.attach(globals())\n";

/// How long one ManimGX pass, or voicing one line, may take before it is stopped.
const PASS_LIMIT: Duration = Duration::from_secs(30 * 60);
const LINE_LIMIT: Duration = Duration::from_secs(15 * 60);

/// Runs the whole pipeline. `on_progress` is called from the rendering task in order.
pub async fn render(
    job: &RenderJob,
    config: &RenderConfig,
    mut on_progress: impl FnMut(RenderProgress) + Send,
) -> Result<RenderOutput, RenderError> {
    if config.sandbox == SandboxMode::MacosSandboxExec && !cfg!(target_os = "macos") {
        return Err(RenderError::Sandbox);
    }
    let manimgx = std::fs::canonicalize(&config.manimgx)
        .ok()
        .filter(|p| p.is_file())
        .ok_or_else(|| RenderError::ToolMissing(tools::MANIMGX.to_string()))?;
    if let Voice::PocketTts { cli, .. } = &config.voice
        && !cli.is_file()
    {
        return Err(RenderError::ToolMissing(tools::FLUIDAUDIOCLI.to_string()));
    }

    // 1. The scene, the voice module and the state folder.
    std::fs::create_dir_all(&job.work_dir).map_err(io("create the video's folder"))?;
    let work_dir = std::fs::canonicalize(&job.work_dir).map_err(io("find the video's folder"))?;
    let state = work_dir.join(STATE_DIR);
    std::fs::create_dir_all(state.join(NARRATION_DIR))
        .map_err(io("create the renderer's folder"))?;
    let mut scene = job.scene_code.clone();
    scene.push_str(SCENE_SUFFIX);
    write(&work_dir.join(SCENE_FILE), scene.as_bytes())?;
    write(&work_dir.join(VOICE_MODULE_FILE), VOICE_MODULE.as_bytes())?;
    remove_file(&state.join("lines.json"))?;
    remove_old_outputs(&work_dir)?;

    // 2. Pass 1: find the lines.
    write(&state.join("pass.json"), br#"{"mode": "collect"}"#)?;
    on_progress(RenderProgress::Log(
        "margin: pass 1, manimgx inspect (collecting the narration)".into(),
    ));
    let collect_png = state.join("collect.png");
    let inspect_args = vec![
        "inspect".to_string(),
        SCENE_FILE.to_string(),
        SCENE_CLASS.to_string(),
        "-o".to_string(),
        collect_png.display().to_string(),
    ];
    let pass1 = run_manimgx(
        &manimgx,
        &work_dir,
        config.sandbox,
        inspect_args,
        &mut on_progress,
    )
    .await?;
    // `inspect` exits 1 for layout problems, which don't stop a render.
    let layout_only = pass1.status.as_ref().is_some_and(|s| s.code() == Some(1))
        && pass1.stderr.iter().all(|l| l.trim().is_empty())
        && pass1.stdout.iter().any(|l| l.starts_with("layout:"));
    if !layout_only {
        check_pass(&pass1)?;
    }
    remove_sheets(&state, "collect")?;
    let lines: Vec<String> = match std::fs::read(state.join("lines.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| RenderError::Io(format!("couldn't read the narration lines: {e}")))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            return Err(RenderError::Io(format!(
                "couldn't read the narration lines: {e}"
            )));
        }
    };

    // 3. Voice each line.
    let audio = voice_lines(&lines, &config.voice, &state, &mut on_progress).await?;

    // 4. Pass 2: the video.
    let pass = serde_json::json!({ "mode": "render", "audio": audio });
    write(&state.join("pass.json"), pass.to_string().as_bytes())?;
    on_progress(RenderProgress::Rendering { progress: None });
    on_progress(RenderProgress::Log("margin: pass 2, manimgx render".into()));
    let video = work_dir.join(VIDEO_FILE);
    let render_args = vec![
        "render".to_string(),
        SCENE_FILE.to_string(),
        SCENE_CLASS.to_string(),
        "-o".to_string(),
        video.display().to_string(),
        "-r".to_string(),
        format!("{}x{}", config.width, config.height),
        "--fps".to_string(),
        config.fps.to_string(),
    ];
    let pass2 = run_manimgx(
        &manimgx,
        &work_dir,
        config.sandbox,
        render_args,
        &mut on_progress,
    )
    .await?;
    check_pass(&pass2)?;

    // The outputs, under the names the service serves.
    if !video.is_file() {
        return Err(RenderError::Io(
            "ManimGX finished without writing the video".into(),
        ));
    }
    let stem = VIDEO_FILE.trim_end_matches(".mp4");
    rename_sheets(&work_dir, &format!("{stem}.storyboard"), "storyboard")?;
    let storyboard = work_dir.join(STORYBOARD_FILE);
    if !storyboard.is_file() {
        return Err(RenderError::Io(
            "ManimGX finished without writing the storyboard".into(),
        ));
    }
    let srt = work_dir.join(format!("{stem}.srt"));
    let captions = if srt.is_file() {
        let to = work_dir.join(CAPTIONS_FILE);
        std::fs::rename(&srt, &to).map_err(io("rename the captions"))?;
        Some(to)
    } else {
        None
    };
    let duration_seconds =
        printed_duration(&pass2.stdout, VIDEO_FILE).or_else(|| mp4::duration_seconds(&video));
    Ok(RenderOutput {
        mp4: video,
        storyboard,
        captions,
        duration_seconds,
    })
}

/// Runs one ManimGX pass in `work_dir`, sandboxed or not, streaming its output as logs.
async fn run_manimgx(
    manimgx: &Path,
    work_dir: &Path,
    sandbox: SandboxMode,
    args: Vec<String>,
    on_progress: &mut (impl FnMut(RenderProgress) + Send),
) -> Result<process::Finished, RenderError> {
    // ManimGX's cache of what the voice said: each pass has its own voice.
    remove_dir(&work_dir.join(MANIMGX_VOICE_CACHE))?;
    let mut cmd = match sandbox {
        SandboxMode::Unsandboxed => Command::new(manimgx),
        SandboxMode::MacosSandboxExec => {
            let mut cmd = Command::new(sandbox::SANDBOX_EXEC);
            cmd.args(sandbox::sandbox_args(manimgx, work_dir).await?);
            cmd
        }
    };
    tracing::debug!(?sandbox, ?args, dir = %work_dir.display(), "running manimgx");
    cmd.args(&args)
        .current_dir(work_dir)
        .env("PYTHONDONTWRITEBYTECODE", "1");
    let finished = run_streaming(cmd, PASS_LIMIT, |_, line| {
        on_progress(RenderProgress::Log(line.to_string()))
    })
    .await
    .map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => RenderError::ToolMissing(tools::MANIMGX.to_string()),
        _ => RenderError::Io(format!("couldn't run manimgx: {e}")),
    })?;
    if finished.status.is_none() {
        return Err(RenderError::Scene {
            message: format!(
                "the scene ran for more than {} minutes and was stopped",
                PASS_LIMIT.as_secs() / 60
            ),
        });
    }
    Ok(finished)
}

/// Maps a failed pass to the error it means.
fn check_pass(pass: &process::Finished) -> Result<(), RenderError> {
    let Some(status) = pass.status else {
        return Ok(());
    };
    if status.success() {
        return Ok(());
    }
    if let Some(line) = pass.stderr.iter().find(|l| l.starts_with("sandbox-exec:")) {
        return Err(RenderError::Io(format!(
            "the sandbox couldn't start manimgx: {line}"
        )));
    }
    let message = scene_error::scene_message(&pass.stderr, &pass.stdout);
    if scene_error::is_gpu_failure(&message) {
        return Err(RenderError::Io(format!(
            "ManimGX couldn't use the GPU: {message}"
        )));
    }
    if message.is_empty() {
        return Err(RenderError::Scene {
            message: format!("manimgx exited with {status} and printed nothing"),
        });
    }
    Err(RenderError::Scene { message })
}

/// Makes a WAV for each line; returns each line's file, relative to the state folder.
async fn voice_lines(
    lines: &[String],
    voice: &Voice,
    state: &Path,
    on_progress: &mut (impl FnMut(RenderProgress) + Send),
) -> Result<BTreeMap<String, String>, RenderError> {
    let total = u32::try_from(lines.len()).unwrap_or(u32::MAX);
    let mut audio = BTreeMap::new();
    let mut keep = BTreeSet::new();
    if total > 0 {
        let what = match voice {
            Voice::PocketTts { .. } => "with PocketTTS",
            Voice::Silent => "as silence",
        };
        on_progress(RenderProgress::Log(format!(
            "margin: voicing {total} lines {what}"
        )));
        on_progress(RenderProgress::Voicing { done: 0, total });
    }
    for (i, line) in lines.iter().enumerate() {
        let speakable = !line.trim().is_empty();
        let name = match voice {
            Voice::PocketTts { voice: name, .. } if speakable => {
                let key = format!("{}\0{line}", name.as_deref().unwrap_or(""));
                format!("pocket-{:016x}.wav", fnv1a(key.as_bytes()))
            }
            _ => format!("silent-{:016x}.wav", fnv1a(line.as_bytes())),
        };
        let path = state.join(NARRATION_DIR).join(&name);
        match voice {
            Voice::PocketTts { cli, voice: name } if speakable => {
                let cached = std::fs::read(&path).is_ok_and(|b| wav::is_wav_with_audio(&b));
                if !cached {
                    pocket_tts(cli, name.as_deref(), line, &path, on_progress).await?;
                }
            }
            _ => write(&path, &wav::silent_wav(wav::estimated_seconds(line)))?,
        }
        audio.insert(line.clone(), format!("{NARRATION_DIR}/{name}"));
        keep.insert(name);
        on_progress(RenderProgress::Voicing {
            done: u32::try_from(i + 1).unwrap_or(u32::MAX),
            total,
        });
    }
    // Forget narration no line uses any more.
    if let Ok(entries) = std::fs::read_dir(state.join(NARRATION_DIR)) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !keep.contains(&name) {
                std::fs::remove_file(entry.path()).ok();
            }
        }
    }
    Ok(audio)
}

/// `fluidaudiocli tts --backend pocket --text <line> --output <wav> [--voice <name>]`,
/// unsandboxed. Writes to a temporary name first, so a failed run leaves no WAV behind.
async fn pocket_tts(
    cli: &Path,
    voice: Option<&str>,
    line: &str,
    out: &Path,
    on_progress: &mut (impl FnMut(RenderProgress) + Send),
) -> Result<(), RenderError> {
    tracing::debug!(line, "voicing with PocketTTS");
    let partial = out.with_extension("partial.wav");
    remove_file(&partial)?;
    let mut cmd = Command::new(cli);
    cmd.arg("tts")
        .arg("--backend")
        .arg("pocket")
        .arg("--text")
        .arg(line)
        .arg("--output")
        .arg(&partial);
    if let Some(voice) = voice {
        cmd.arg("--voice").arg(voice);
    }
    if let Some(dir) = out.parent() {
        cmd.current_dir(dir);
    }
    let mut tail: Vec<String> = Vec::new();
    let finished = run_streaming(cmd, LINE_LIMIT, |stream, l| {
        if stream == Stream::Stderr || !l.trim().is_empty() {
            tail.push(l.to_string());
            if tail.len() > 8 {
                tail.remove(0);
            }
        }
        on_progress(RenderProgress::Log(l.to_string()));
    })
    .await
    .map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => RenderError::ToolMissing(tools::FLUIDAUDIOCLI.to_string()),
        _ => RenderError::Voice(format!("couldn't run fluidaudiocli: {e}")),
    })?;
    let Some(status) = finished.status else {
        return Err(RenderError::Voice(format!(
            "fluidaudiocli took more than {} minutes for one line",
            LINE_LIMIT.as_secs() / 60
        )));
    };
    let made = std::fs::read(&partial).is_ok_and(|b| wav::is_wav_with_audio(&b));
    if !status.success() || !made {
        remove_file(&partial)?;
        let said = tail.join("\n");
        return Err(RenderError::Voice(if said.trim().is_empty() {
            format!("fluidaudiocli exited with {status} without writing audio for {line:?}")
        } else {
            format!("fluidaudiocli exited with {status} for {line:?}: {said}")
        }));
    }
    std::fs::rename(&partial, out).map_err(io("keep the narration"))?;
    Ok(())
}

/// ManimGX prints `video.mp4  5.28 s, 320x180 at 15 fps, …` after a render.
fn printed_duration(stdout: &[String], file: &str) -> Option<f32> {
    stdout.iter().find_map(|line| {
        let mut words = line.split_whitespace();
        if words.next()? != file {
            return None;
        }
        let seconds: f32 = words.next()?.parse().ok()?;
        words.next()?.starts_with('s').then_some(seconds)
    })
}

/// Removes outputs of an earlier render, so a failed render never leaves stale ones.
fn remove_old_outputs(work_dir: &Path) -> Result<(), RenderError> {
    remove_file(&work_dir.join(VIDEO_FILE))?;
    remove_file(&work_dir.join(CAPTIONS_FILE))?;
    remove_file(&work_dir.join("video.srt"))?;
    remove_sheets(work_dir, "storyboard")?;
    remove_sheets(work_dir, "video.storyboard")
}

/// `<stem>.png`, `<stem>-2.png`, … in `dir`: the sheets of a storyboard.
fn sheets(dir: &Path, stem: &str) -> Vec<(PathBuf, String)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let rest = name.strip_prefix(stem)?.strip_suffix(".png")?;
            let numbered = rest
                .strip_prefix('-')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
            (rest.is_empty() || numbered).then(|| (e.path(), rest.to_string()))
        })
        .collect()
}

fn remove_sheets(dir: &Path, stem: &str) -> Result<(), RenderError> {
    for (path, _) in sheets(dir, stem) {
        remove_file(&path)?;
    }
    Ok(())
}

fn rename_sheets(dir: &Path, from_stem: &str, to_stem: &str) -> Result<(), RenderError> {
    for (path, suffix) in sheets(dir, from_stem) {
        std::fs::rename(&path, dir.join(format!("{to_stem}{suffix}.png")))
            .map_err(io("rename the storyboard"))?;
    }
    Ok(())
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), RenderError> {
    std::fs::write(path, bytes)
        .map_err(|e| RenderError::Io(format!("couldn't write {}: {e}", path.display())))
}

fn remove_file(path: &Path) -> Result<(), RenderError> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(RenderError::Io(format!(
            "couldn't remove {}: {e}",
            path.display()
        ))),
        _ => Ok(()),
    }
}

fn remove_dir(path: &Path) -> Result<(), RenderError> {
    match std::fs::remove_dir_all(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(RenderError::Io(format!(
            "couldn't remove {}: {e}",
            path.display()
        ))),
        _ => Ok(()),
    }
}

fn io(what: &'static str) -> impl Fn(std::io::Error) -> RenderError {
    move |e| RenderError::Io(format!("couldn't {what}: {e}"))
}

/// FNV-1a, 64 bits: stable names for narration files.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_from_the_render_report() {
        let out = vec![
            "video.mp4  5.28 s, 320x180 at 15 fps, 0.2 MB (rendered in 0.1 s)".to_string(),
            "storyboard: video.storyboard.png".to_string(),
        ];
        assert_eq!(printed_duration(&out, "video.mp4"), Some(5.28));
        assert_eq!(printed_duration(&out[1..], "video.mp4"), None);
    }

    #[test]
    fn storyboard_sheets() {
        let dir = std::env::temp_dir().join(format!("margin-video-sheets-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "video.storyboard.png",
            "video.storyboard-2.png",
            "video.storyboard-x.png",
            "video.mp4",
        ] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        rename_sheets(&dir, "video.storyboard", "storyboard").unwrap();
        assert!(dir.join("storyboard.png").is_file());
        assert!(dir.join("storyboard-2.png").is_file());
        assert!(dir.join("video.storyboard-x.png").is_file());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn render_future_is_send() {
        fn assert_send<T: Send>(_: T) {}
        let job = RenderJob {
            work_dir: PathBuf::new(),
            scene_code: String::new(),
        };
        let config = RenderConfig {
            manimgx: PathBuf::new(),
            voice: Voice::Silent,
            sandbox: SandboxMode::Unsandboxed,
            width: 1,
            height: 1,
            fps: 1,
        };
        assert_send(render(&job, &config, |_| {}));
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
