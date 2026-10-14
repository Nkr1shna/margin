//! Renders real ManimGX scenes. Needs `manimgx` (on PATH or in `~/.local/bin`); the
//! PocketTTS test also needs `fluidaudiocli` and is skipped without it.

use std::path::PathBuf;

use margin_video::{
    RenderConfig, RenderError, RenderJob, RenderOutput, RenderProgress, SandboxMode, Voice,
    detect_tools, render,
};

const NARRATED: &str = r#"import manimgx as m


class LessonScene(m.Scene):
    def construct(self) -> None:
        circle = m.Circle(color=m.BLUE)
        self.say("Here is [a circle].", m.Create(circle))
        self.say("It is [round].", m.Indicate(circle))
        self.wait(0.2)
"#;

async fn tool(name: &str) -> Option<PathBuf> {
    let tools = detect_tools().await;
    tools
        .into_iter()
        .find(|t| t.name == name && t.found)
        .and_then(|t| t.path)
        .map(PathBuf::from)
}

async fn manimgx() -> PathBuf {
    tool("manimgx")
        .await
        .expect("manimgx must be installed for these tests (uv tool install --python 3.14 manimgx)")
}

fn work_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("margin-video")
        .join(name);
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn config(sandbox: SandboxMode, voice: Voice) -> RenderConfig {
    RenderConfig {
        manimgx: manimgx().await,
        voice,
        sandbox,
        width: 320,
        height: 180,
        fps: 15,
    }
}

async fn run(
    name: &str,
    code: &str,
    config: &RenderConfig,
) -> (Result<RenderOutput, RenderError>, Vec<RenderProgress>) {
    let job = RenderJob {
        work_dir: work_dir(name),
        scene_code: code.to_string(),
    };
    let mut events = Vec::new();
    let result = render(&job, config, |p| events.push(p)).await;
    (result, events)
}

fn assert_rendered(out: &RenderOutput, events: &[RenderProgress]) {
    assert!(out.mp4.is_file(), "no video at {}", out.mp4.display());
    assert!(std::fs::metadata(&out.mp4).unwrap().len() > 1000);
    assert!(
        out.storyboard.is_file(),
        "no storyboard at {}",
        out.storyboard.display()
    );
    assert_eq!(out.mp4.file_name().unwrap(), "video.mp4");
    assert_eq!(out.storyboard.file_name().unwrap(), "storyboard.png");
    let captions = out
        .captions
        .as_ref()
        .expect("a narrated scene has captions");
    assert!(captions.is_file());
    let srt = std::fs::read_to_string(captions).unwrap();
    assert!(srt.contains("Here is a circle."), "captions: {srt}");
    let duration = out.duration_seconds.expect("a duration");
    assert!(duration > 1.0, "duration {duration}");
    assert!(
        events.contains(&RenderProgress::Voicing { done: 2, total: 2 }),
        "{events:?}"
    );
    assert!(events.contains(&RenderProgress::Rendering { progress: None }));
    let voiced = events
        .iter()
        .position(|e| *e == RenderProgress::Voicing { done: 2, total: 2 })
        .unwrap();
    let rendering = events
        .iter()
        .position(|e| *e == RenderProgress::Rendering { progress: None })
        .unwrap();
    assert!(voiced < rendering);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, RenderProgress::Log(l) if l.contains("storyboard")))
    );
}

fn scene_message(result: Result<RenderOutput, RenderError>) -> String {
    match result {
        Err(RenderError::Scene { message }) => message,
        other => panic!("expected a scene error, got {other:?}"),
    }
}

#[tokio::test]
async fn detect_tools_finds_manimgx() {
    let tools = detect_tools().await;
    let m = tools.iter().find(|t| t.name == "manimgx").unwrap();
    assert!(m.found, "{m:?}");
    assert!(m.path.as_deref().is_some_and(|p| p.ends_with("manimgx")));
    assert!(
        m.version
            .as_deref()
            .is_some_and(|v| v.chars().next().is_some_and(|c| c.is_ascii_digit())),
        "{m:?}"
    );
    assert!(m.install_commands.is_empty());
    let f = tools.iter().find(|t| t.name == "fluidaudiocli").unwrap();
    assert_eq!(f.found, f.install_commands.is_empty(), "{f:?}");
    if !f.found {
        assert!(
            f.install_commands
                .iter()
                .any(|c| c.contains("swift build -c release --product fluidaudiocli"))
        );
    }
}

#[tokio::test]
async fn silent_render_unsandboxed() {
    let (result, events) = run(
        "silent-unsandboxed",
        NARRATED,
        &config(SandboxMode::Unsandboxed, Voice::Silent).await,
    )
    .await;
    let out = result.unwrap();
    assert_rendered(&out, &events);
    // The scene file is the model's code plus the voice hook.
    let scene = std::fs::read_to_string(out.mp4.with_file_name("scene.py")).unwrap();
    assert!(scene.starts_with(NARRATED));
    assert!(scene.contains("_margin_voice.attach(globals())"));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn silent_render_sandboxed() {
    let (result, events) = run(
        "silent-sandboxed",
        NARRATED,
        &config(SandboxMode::MacosSandboxExec, Voice::Silent).await,
    )
    .await;
    assert_rendered(&result.unwrap(), &events);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn rerender_in_the_same_folder_replaces_the_outputs() {
    let config = config(SandboxMode::MacosSandboxExec, Voice::Silent).await;
    let (first, _) = run("rerender", NARRATED, &config).await;
    let first = first.unwrap();
    let silent = r#"import manimgx as m


class LessonScene(m.Scene):
    def construct(self) -> None:
        self.play(m.Create(m.Square()))
"#;
    let job = RenderJob {
        work_dir: first.mp4.parent().unwrap().to_path_buf(),
        scene_code: silent.to_string(),
    };
    let second = render(&job, &config, |_| {}).await.unwrap();
    assert!(
        second.captions.is_none(),
        "a scene that doesn't speak has no captions"
    );
    assert!(!first.captions.unwrap().exists());
    assert!((second.duration_seconds.unwrap() - 1.0).abs() < 0.2);
}

#[tokio::test]
async fn broken_scene_reports_the_line_and_hint() {
    let code = r#"import manimgx as m


class LessonScene(m.Scene):
    def construct(self) -> None:
        circle = m.Circle()
        self.play(m.ShowCreation(circle))
"#;
    let message = scene_message(
        run(
            "broken",
            code,
            &config(SandboxMode::Unsandboxed, Voice::Silent).await,
        )
        .await
        .0,
    );
    assert!(message.contains("scene.py:7"), "{message}");
    assert!(
        message.contains("self.play(m.ShowCreation(circle))"),
        "{message}"
    );
    assert!(message.contains("ShowCreation is Create"), "{message}");
    assert!(message.lines().count() <= 6, "{message}");
}

#[tokio::test]
async fn scene_without_lesson_scene_is_a_scene_error() {
    let code = "import manimgx as m\n\n\nclass Other(m.Scene):\n    def construct(self) -> None:\n        self.wait(0.2)\n";
    let message = scene_message(
        run(
            "no-lesson-scene",
            code,
            &config(SandboxMode::Unsandboxed, Voice::Silent).await,
        )
        .await
        .0,
    );
    assert!(message.contains("LessonScene"), "{message}");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn sandbox_blocks_the_network() {
    let code = r#"import socket
import urllib.request

import manimgx as m


class LessonScene(m.Scene):
    def construct(self) -> None:
        try:
            socket.create_connection(("1.1.1.1", 443), timeout=5).close()
            direct = "connected"
        except OSError as e:
            direct = f"blocked ({e})"
        body = urllib.request.urlopen("https://example.com", timeout=10).read()
        self.add(m.Text(direct + str(len(body))))
        self.wait(0.2)
"#;
    let message = scene_message(
        run(
            "network",
            code,
            &config(SandboxMode::MacosSandboxExec, Voice::Silent).await,
        )
        .await
        .0,
    );
    // ManimGX prints only the innermost frames, here urllib's, then the error.
    assert!(message.contains("URLError"), "{message}");

    // A direct connection by address is refused too, not only name lookups.
    let code = r#"import socket

import manimgx as m


class LessonScene(m.Scene):
    def construct(self) -> None:
        socket.create_connection(("1.1.1.1", 443), timeout=5).close()
        self.wait(0.2)
"#;
    let message = scene_message(
        run(
            "network-direct",
            code,
            &config(SandboxMode::MacosSandboxExec, Voice::Silent).await,
        )
        .await
        .0,
    );
    assert!(
        message.contains("PermissionError") || message.contains("Operation not permitted"),
        "{message}"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn sandbox_blocks_writes_outside_the_work_dir() {
    let outside = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("margin-video-outside.txt");
    std::fs::remove_file(&outside).ok();
    let home = std::env::var("HOME").unwrap();
    let in_home =
        PathBuf::from(&home).join(format!("margin-video-escape-{}.txt", std::process::id()));
    let code = format!(
        r#"import pathlib

import manimgx as m


class LessonScene(m.Scene):
    def construct(self) -> None:
        for target in ({outside:?}, {in_home:?}):
            try:
                pathlib.Path(target).write_text("escaped")
            except OSError as e:
                print("blocked", target, e)
            else:
                raise SystemExit("wrote " + target)
        pathlib.Path("inside.txt").write_text("fine")
        raise RuntimeError("both writes were blocked")
"#,
        outside = outside.display().to_string(),
        in_home = in_home.display().to_string(),
    );
    let (result, events) = run(
        "escape",
        &code,
        &config(SandboxMode::MacosSandboxExec, Voice::Silent).await,
    )
    .await;
    let message = scene_message(result);
    assert!(
        message.contains("both writes were blocked"),
        "{message}\n{events:?}"
    );
    assert!(!outside.exists());
    assert!(!in_home.exists());
    // Writes inside the work dir are allowed.
    let inside = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("margin-video/escape/inside.txt");
    assert!(inside.is_file());
}

#[tokio::test]
async fn missing_manimgx_is_reported() {
    let mut config = config(SandboxMode::Unsandboxed, Voice::Silent).await;
    config.manimgx = PathBuf::from("/nonexistent/manimgx");
    let (result, _) = run("missing-tool", NARRATED, &config).await;
    assert!(matches!(result, Err(RenderError::ToolMissing(t)) if t == "manimgx"));
}

#[tokio::test]
async fn pocket_tts_narration() {
    let Some(cli) = tool("fluidaudiocli").await else {
        eprintln!(
            "SKIPPED pocket_tts_narration: fluidaudiocli isn't installed (see detect_tools' install commands)"
        );
        return;
    };
    let sandbox = if cfg!(target_os = "macos") {
        SandboxMode::MacosSandboxExec
    } else {
        SandboxMode::Unsandboxed
    };
    let (result, events) = run(
        "pocket",
        NARRATED,
        &config(sandbox, Voice::PocketTts { cli, voice: None }).await,
    )
    .await;
    let out = match result {
        Ok(out) => out,
        Err(e) => panic!("{e}\n{events:?}"),
    };
    assert_rendered(&out, &events);

    // Each line was voiced, and the voice is not silence.
    let narration = out.mp4.with_file_name(".margin").join("narration");
    let wavs: Vec<PathBuf> = std::fs::read_dir(&narration)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("pocket-")
        })
        .collect();
    assert_eq!(wavs.len(), 2, "{wavs:?}");
    let mut spoken = 0.0;
    for wav in &wavs {
        let bytes = std::fs::read(wav).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
        let samples: Vec<i16> = bytes[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes(*c))
            .collect();
        let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(peak > 1000, "{} is silent (peak {peak})", wav.display());
        spoken += samples.len() as f32 / rate as f32;
    }
    assert!(
        out.duration_seconds.unwrap() >= spoken - 0.1,
        "{:?} < {spoken}",
        out.duration_seconds
    );
    // The MP4 has a sound track (an `hdlr` box of type `soun`).
    let mp4 = std::fs::read(&out.mp4).unwrap();
    assert!(
        mp4.windows(8).any(|w| w == b"\0\0\0\0soun"),
        "no sound track in {}",
        out.mp4.display()
    );
}

#[tokio::test]
async fn layout_problems_do_not_stop_a_render() {
    // `manimgx inspect` exits 1 for layout problems; the render still goes ahead.
    let code = r#"import manimgx as m


class LessonScene(m.Scene):
    def construct(self) -> None:
        title = m.Text("A title much too long for the frame to hold at all", font_size=60)
        self.say("A [long] title.", m.Write(title))
"#;
    let config = config(SandboxMode::Unsandboxed, Voice::Silent).await;
    let (result, events) = run("layout", code, &config).await;
    let out = result.unwrap();
    assert!(out.mp4.is_file());
    assert!(
        events.iter().any(
            |e| matches!(e, RenderProgress::Log(l) if l.starts_with("layout:") && l.contains("problem"))
        ),
        "{events:?}"
    );
}
