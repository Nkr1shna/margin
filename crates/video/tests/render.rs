//! Renders real ManimGX scenes. Needs `manimgx` (on PATH or in `~/.local/bin`); the
//! PocketTTS test also needs `margin-tts` and is skipped without it. The sandboxed tests
//! run on Linux only, under bubblewrap (`SandboxMode::LinuxBubblewrap`, which needs
//! `bwrap` and user namespaces).

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

/// Ends the way the prompt asks every lesson scene to: the title and the key picture
/// brought on together after the last line, then held with `self.wait(2)`.
const POSTER_SHOT: &str = r#"import manimgx as m


class LessonScene(m.Scene):
    def construct(self) -> None:
        circle = m.Circle(radius=1.5, color=m.BLUE, fill_opacity=0.6)
        self.say("Here is [a circle].", m.Create(circle))
        self.say("[Clear the board].", m.FadeOut(circle))
        title = m.Text("Circles", font_size=56).to_edge(m.UP)
        formula = m.MathTex(r"A = \pi r^2", font_size=72).move_to([2.5, -0.5, 0])
        disc = m.Circle(radius=1.5, color=m.BLUE, fill_opacity=0.6).move_to([-3, -0.5, 0])
        self.play(m.FadeIn(title), m.FadeIn(disc), m.Write(formula))
        self.wait(2)
"#;

/// A decoded 8-bit RGBA PNG: width, height and pixels (rows from the top).
fn decode_png(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "not a PNG");
    let mut at = 8;
    let (mut width, mut height) = (0, 0);
    let mut idat = Vec::new();
    let mut ended = false;
    while at + 12 <= bytes.len() {
        let len = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        let kind = &bytes[at + 4..at + 8];
        let data = &bytes[at + 8..at + 8 + len];
        let crc = u32::from_be_bytes(bytes[at + 8 + len..at + 12 + len].try_into().unwrap());
        assert_eq!(crc, crc32(&bytes[at + 4..at + 8 + len]), "bad CRC");
        match kind {
            b"IHDR" => {
                width = u32::from_be_bytes(data[0..4].try_into().unwrap());
                height = u32::from_be_bytes(data[4..8].try_into().unwrap());
                assert_eq!(&data[8..], &[8, 6, 0, 0, 0], "8-bit RGBA, no interlace");
            }
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => ended = true,
            _ => {}
        }
        at += 12 + len;
    }
    assert!(ended && width > 0 && height > 0);
    let raw = miniz_oxide::inflate::decompress_to_vec_zlib(&idat).expect("IDAT inflates");
    let stride = width as usize * 4;
    assert_eq!(raw.len(), height as usize * (stride + 1));
    let mut pixels = Vec::with_capacity(height as usize * stride);
    for row in raw.chunks(stride + 1) {
        assert_eq!(row[0], 0, "rows are unfiltered");
        pixels.extend_from_slice(&row[1..]);
    }
    (width, height, pixels)
}

fn crc32(bytes: &[u8]) -> u32 {
    !bytes.iter().fold(!0u32, |mut c, &b| {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 == 1 {
                (c >> 1) ^ 0xEDB8_8320
            } else {
                c >> 1
            };
        }
        c
    })
}

/// The poster is a PNG at the render size showing the poster shot, not a blank frame.
fn assert_poster(out: &RenderOutput, config: &RenderConfig, events: &[RenderProgress]) {
    let poster = out
        .poster
        .as_ref()
        .unwrap_or_else(|| panic!("no poster: {events:?}"));
    assert_eq!(poster.file_name().unwrap(), "poster.png");
    let (width, height, pixels) = decode_png(&std::fs::read(poster).unwrap());
    assert_eq!((width, height), (config.width, config.height));
    let background = &pixels[..4];
    let drawn = pixels.chunks(4).filter(|p| *p != background).count();
    let total = (width * height) as usize;
    assert!(drawn * 20 > total, "{drawn} of {total} pixels drawn");
    // The title is in the top quarter and the disc (blue) below it.
    let top = &pixels[..stride(width) * (height as usize / 4)];
    assert!(top.chunks(4).any(|p| p != background), "no title");
    assert!(
        pixels.chunks(4).any(|p| p[2] > 150 && p[0] < 120),
        "no blue disc"
    );
}

fn stride(width: u32) -> usize {
    width as usize * 4
}

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
    // The narration tool.
    let f = tools.iter().find(|t| t.name == "margin-tts").unwrap();
    assert_eq!(f.found, f.install_commands.is_empty(), "{f:?}");
    if !f.found {
        assert!(
            f.install_commands.iter().any(|c| c.contains("margin-tts")),
            "{f:?}"
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

#[cfg(target_os = "linux")]
#[tokio::test]
async fn silent_render_sandboxed() {
    let (result, events) = run(
        "silent-sandboxed",
        NARRATED,
        &config(SandboxMode::LinuxBubblewrap, Voice::Silent).await,
    )
    .await;
    assert_rendered(&result.unwrap(), &events);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn rerender_in_the_same_folder_replaces_the_outputs() {
    let config = config(SandboxMode::LinuxBubblewrap, Voice::Silent).await;
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
async fn poster_unsandboxed() {
    let config = config(SandboxMode::Unsandboxed, Voice::Silent).await;
    let (result, events) = run("poster-unsandboxed", POSTER_SHOT, &config).await;
    let out = result.unwrap();
    assert!(out.mp4.is_file());
    assert_poster(&out, &config, &events);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn poster_sandboxed() {
    let config = config(SandboxMode::LinuxBubblewrap, Voice::Silent).await;
    let (result, events) = run("poster-sandboxed", POSTER_SHOT, &config).await;
    let out = result.unwrap();
    assert_poster(&out, &config, &events);

    // A re-render that can't make a poster leaves no stale one behind.
    let job = RenderJob {
        work_dir: out.mp4.parent().unwrap().to_path_buf(),
        scene_code: NARRATED.to_string(),
    };
    let again = render(&job, &config, |_| {}).await.unwrap();
    assert!(again.poster.is_none());
    assert!(!out.poster.unwrap().exists());
}

/// Poster capture never fails a render: whatever goes wrong, the video is made and the
/// poster is `None`, with the reason in the log.
#[tokio::test]
async fn no_poster_still_renders() {
    let config = config(SandboxMode::Unsandboxed, Voice::Silent).await;
    let fades_out = POSTER_SHOT.replace(
        "        self.wait(2)\n",
        "        self.play(m.FadeOut(*self.mobjects))\n        self.wait(2)\n",
    );
    // Encoding the PNG raises (the scene breaks zlib, which the voice module uses).
    let broken_png = format!(
        "import zlib\n\n{}",
        POSTER_SHOT.replace(
            "        self.wait(2)\n",
            "        zlib.compress = None\n        self.wait(2)\n"
        )
    );
    let cases = [
        (
            "poster-no-hold",
            NARRATED.to_string(),
            "doesn't end on a held",
        ),
        ("poster-blank", fades_out, "single color"),
        ("poster-error", broken_png, "TypeError"),
    ];
    for (name, code, why) in cases {
        let (result, events) = run(name, &code, &config).await;
        let out = result.unwrap_or_else(|e| panic!("{name}: {e}\n{events:?}"));
        assert!(out.mp4.is_file(), "{name}");
        assert!(out.poster.is_none(), "{name}");
        assert!(!out.mp4.with_file_name("poster.png").exists(), "{name}");
        assert!(
            !out.mp4.with_file_name("poster.png.partial").exists(),
            "{name}"
        );
        assert!(
            events.iter().any(|e| matches!(e, RenderProgress::Log(l)
                if l.starts_with("margin: poster skipped:") && l.contains(why))),
            "{name}: {events:?}"
        );
    }
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

#[cfg(target_os = "linux")]
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
            &config(SandboxMode::LinuxBubblewrap, Voice::Silent).await,
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
            &config(SandboxMode::LinuxBubblewrap, Voice::Silent).await,
        )
        .await
        .0,
    );
    // In bubblewrap's empty network namespace there is no route.
    assert!(
        message.contains("PermissionError")
            || message.contains("Operation not permitted")
            || message.contains("Network is unreachable"),
        "{message}"
    );
}

#[cfg(target_os = "linux")]
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
        &config(SandboxMode::LinuxBubblewrap, Voice::Silent).await,
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
    let Some(cli) = tool("margin-tts").await else {
        eprintln!(
            "SKIPPED pocket_tts_narration: margin-tts isn't installed (see detect_tools' install commands)"
        );
        return;
    };
    let sandbox = if cfg!(target_os = "linux") {
        SandboxMode::LinuxBubblewrap
    } else {
        SandboxMode::Unsandboxed
    };
    // The poster shot scene: real speech is longer or shorter than pass 1's estimate, and
    // the poster is still the closing hold.
    let config = config(sandbox, Voice::PocketTts { cli, voice: None }).await;
    let (result, events) = run("pocket", POSTER_SHOT, &config).await;
    let out = match result {
        Ok(out) => out,
        Err(e) => panic!("{e}\n{events:?}"),
    };
    assert_rendered(&out, &events);
    assert_poster(&out, &config, &events);

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
