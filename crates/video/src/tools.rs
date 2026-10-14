//! Finds the tools the service runs, with the commands that install them when they are
//! missing (Settings → Tools). Nothing here runs an installer.
//!
//! - macOS: `manimgx` (renders videos) and `fluidaudiocli` (narration and transcripts).
//! - Linux (`docs/plan-linux-service.md` §2.3–2.10): `manimgx`, `margin-tts` (narration),
//!   `tesseract` (OCR), `ffmpeg`/`ffprobe` (audio and video), `bwrap` (the scene sandbox)
//!   and the pdfium library (PDF text and pages).

use std::path::{Path, PathBuf};
use std::time::Duration;

use margin_shared::ToolStatus;

pub(crate) const MANIMGX: &str = "manimgx";
pub(crate) const FLUIDAUDIOCLI: &str = "fluidaudiocli";
/// Linux tool names, as the Tools rows show them.
pub const MARGIN_TTS: &str = "margin-tts";
pub const TESSERACT: &str = "tesseract";
pub const FFMPEG: &str = "ffmpeg";
pub const FFPROBE: &str = "ffprobe";
pub const BWRAP: &str = "bwrap";
pub const PDFIUM: &str = "pdfium";
/// The library file pdfium-render binds.
pub const PDFIUM_LIBRARY: &str = "libpdfium.so";

/// The FluidAudio release the install commands build.
const FLUIDAUDIO_TAG: &str = "v0.17.7";
/// The pdfium-binaries release the install commands fetch (BSD-3-Clause).
const PDFIUM_RELEASE: &str = "chromium/8100";

pub(crate) async fn detect_tools() -> Vec<ToolStatus> {
    if cfg!(target_os = "macos") {
        macos_tools().await
    } else {
        linux_tools().await
    }
}

async fn macos_tools() -> Vec<ToolStatus> {
    let fluid = find_executable(FLUIDAUDIOCLI);
    let fluid_install = if fluid.is_some() {
        Vec::new()
    } else {
        let mut cmds = Vec::new();
        if !command_line_tools_installed().await {
            cmds.push("xcode-select --install".to_string());
        }
        let checkout_exists =
            home().is_some_and(|h| h.join("Developer/FluidAudio/Package.swift").is_file());
        if !checkout_exists {
            cmds.push(format!(
                "git clone --depth 1 --branch {FLUIDAUDIO_TAG} https://github.com/FluidInference/FluidAudio.git ~/Developer/FluidAudio"
            ));
        }
        cmds.push(
            "cd ~/Developer/FluidAudio && swift build -c release --product fluidaudiocli"
                .to_string(),
        );
        cmds.push(
            "mkdir -p ~/.local/bin && cp ~/Developer/FluidAudio/.build/release/fluidaudiocli ~/.local/bin/".to_string(),
        );
        cmds
    };

    vec![
        manimgx_row().await,
        ToolStatus {
            name: FLUIDAUDIOCLI.to_string(),
            found: fluid.is_some(),
            path: fluid.map(|p| p.display().to_string()),
            // fluidaudiocli has no version flag (FluidAudioCLI.swift handles only
            // commands and help), so the version is unknown.
            version: None,
            install_commands: fluid_install,
        },
    ]
}

async fn linux_tools() -> Vec<ToolStatus> {
    let uv = find_executable("uv").is_some();
    let tts = find_executable(MARGIN_TTS);
    let tesseract = find_executable(TESSERACT);
    let tesseract_version = match &tesseract {
        Some(p) => tool_version(p, &["--version"], TESSERACT).await,
        None => None,
    };
    let ffmpeg = find_executable(FFMPEG);
    let ffprobe = find_executable(FFPROBE);
    let ffmpeg_version = match &ffmpeg {
        Some(p) => tool_version(p, &["-version"], "ffmpeg version").await,
        None => None,
    };
    let bwrap = find_bwrap();
    let bwrap_version = match &bwrap {
        Some(p) => tool_version(p, &["--version"], "bubblewrap").await,
        None => None,
    };
    vec![
        manimgx_row().await,
        margin_tts_row(tts, uv, margin_tts_script()),
        tesseract_row(tesseract, tesseract_version),
        ffmpeg_row(ffmpeg, ffprobe, ffmpeg_version),
        bwrap_row(bwrap, bwrap_version),
        pdfium_row(find_pdfium(&pdfium_dirs())),
    ]
}

async fn manimgx_row() -> ToolStatus {
    let manimgx = find_executable(MANIMGX);
    let version = match &manimgx {
        Some(path) => manimgx_version(path).await,
        None => None,
    };
    let install = if manimgx.is_some() {
        Vec::new()
    } else {
        let mut cmds = uv_install_commands(find_executable("uv").is_some());
        cmds.push("uv tool install --python 3.14 manimgx".to_string());
        cmds
    };
    ToolStatus {
        name: MANIMGX.to_string(),
        found: manimgx.is_some(),
        path: manimgx.map(|p| p.display().to_string()),
        version,
        install_commands: install,
    }
}

/// Installs `uv` first when it's missing.
fn uv_install_commands(uv_found: bool) -> Vec<String> {
    if uv_found {
        Vec::new()
    } else {
        vec![
            "curl -LsSf https://astral.sh/uv/install.sh | sh".to_string(),
            "source $HOME/.local/bin/env".to_string(),
        ]
    }
}

/// The `margin-tts` script's text when this build carries it (LX-speech's
/// `MARGIN_TTS_SCRIPT`, `include_str!("margin_tts.py")`); `None` installs it from a
/// checkout of the repository instead.
fn margin_tts_script() -> Option<&'static str> {
    None
}

/// The narration voice on Linux (Kyutai's pocket-tts behind `margin-tts`, §2.7): a uv
/// script (its `#!/usr/bin/env -S uv run` line fetches pocket-tts on first use) written
/// to `~/.local/bin/margin-tts`.
pub(crate) fn margin_tts_row(
    found: Option<PathBuf>,
    uv_found: bool,
    script: Option<&str>,
) -> ToolStatus {
    let install = if found.is_some() {
        Vec::new()
    } else {
        let mut cmds = uv_install_commands(uv_found);
        cmds.push("mkdir -p ~/.local/bin".to_string());
        match script {
            Some(text) => cmds.push(format!(
                "cat > ~/.local/bin/{MARGIN_TTS} <<'MARGIN_TTS_EOF'\n{}\nMARGIN_TTS_EOF",
                text.trim_end()
            )),
            None => cmds.push(format!(
                "cp <margin checkout>/crates/video/src/margin_tts.py ~/.local/bin/{MARGIN_TTS}"
            )),
        }
        cmds.push(format!("chmod +x ~/.local/bin/{MARGIN_TTS}"));
        cmds
    };
    ToolStatus {
        name: MARGIN_TTS.to_string(),
        found: found.is_some(),
        path: found.map(|p| p.display().to_string()),
        version: None,
        install_commands: install,
    }
}

pub(crate) fn tesseract_row(found: Option<PathBuf>, version: Option<String>) -> ToolStatus {
    apt_row(
        TESSERACT,
        found,
        version,
        "sudo apt install tesseract-ocr tesseract-ocr-eng",
    )
}

/// `ffmpeg` and `ffprobe` come in one package and are needed together: found only
/// when both are.
pub(crate) fn ffmpeg_row(
    ffmpeg: Option<PathBuf>,
    ffprobe: Option<PathBuf>,
    version: Option<String>,
) -> ToolStatus {
    let both = ffmpeg.is_some() && ffprobe.is_some();
    apt_row(
        FFMPEG,
        if both { ffmpeg } else { None },
        version,
        "sudo apt install ffmpeg",
    )
}

/// The scene sandbox. It also needs unprivileged user namespaces, which Ubuntu 24.04
/// restricts through AppArmor: the setup commands are shown with the install line.
pub(crate) fn bwrap_row(found: Option<PathBuf>, version: Option<String>) -> ToolStatus {
    let mut row = apt_row(BWRAP, found, version, BWRAP_INSTALL);
    if !row.found {
        row.install_commands
            .extend(BWRAP_USERNS_SETUP.iter().map(|c| c.to_string()));
    }
    row
}

/// `apt install bubblewrap`. With LX-sandbox merged these are
/// `crate::bubblewrap::{INSTALL_COMMAND, USERNS_SETUP}` and `find_bwrap` is
/// `crate::bubblewrap::find_bwrap`.
const BWRAP_INSTALL: &str = "sudo apt install bubblewrap";
const BWRAP_USERNS_SETUP: &[&str] = &[
    "echo 'kernel.apparmor_restrict_unprivileged_userns = 0' | sudo tee /etc/sysctl.d/60-margin-userns.conf",
    "sudo sysctl --system",
];

fn find_bwrap() -> Option<PathBuf> {
    find_executable(BWRAP)
}

pub(crate) fn pdfium_row(found: Option<PathBuf>) -> ToolStatus {
    let install = if found.is_some() {
        Vec::new()
    } else {
        let arch = if cfg!(target_arch = "x86_64") {
            "x64"
        } else {
            "arm64"
        };
        vec![
            format!(
                "curl -L -o /tmp/pdfium.tgz https://github.com/bblanchon/pdfium-binaries/releases/download/{}/pdfium-linux-{arch}.tgz",
                PDFIUM_RELEASE.replace('/', "%2F")
            ),
            "mkdir -p /tmp/pdfium && tar -xzf /tmp/pdfium.tgz -C /tmp/pdfium".to_string(),
            format!("sudo install -m 0644 /tmp/pdfium/lib/{PDFIUM_LIBRARY} /usr/local/lib/"),
            "sudo ldconfig".to_string(),
        ]
    };
    ToolStatus {
        name: PDFIUM.to_string(),
        found: found.is_some(),
        path: found.map(|p| p.display().to_string()),
        // pdfium-binaries ship no version flag on the library.
        version: None,
        install_commands: install,
    }
}

fn apt_row(
    name: &str,
    found: Option<PathBuf>,
    version: Option<String>,
    install: &str,
) -> ToolStatus {
    ToolStatus {
        name: name.to_string(),
        found: found.is_some(),
        install_commands: if found.is_some() {
            Vec::new()
        } else {
            vec![install.to_string()]
        },
        path: found.map(|p| p.display().to_string()),
        version,
    }
}

/// Where `libpdfium.so` is looked for: `MARGIN_PDFIUM_DIR` when set, `~/.local/lib`,
/// then the system library folders.
pub fn pdfium_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os("MARGIN_PDFIUM_DIR").filter(|d| !d.is_empty()) {
        dirs.push(PathBuf::from(d));
    }
    if let Some(h) = home() {
        dirs.push(h.join(".local/lib"));
    }
    for d in [
        "/usr/local/lib",
        "/usr/lib",
        "/usr/lib/aarch64-linux-gnu",
        "/usr/lib/x86_64-linux-gnu",
    ] {
        dirs.push(PathBuf::from(d));
    }
    dirs
}

/// The first `libpdfium.so` in `dirs`.
pub fn find_pdfium(dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter()
        .map(|d| d.join(PDFIUM_LIBRARY))
        .find(|p| p.is_file())
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// The directories searched: `$PATH`, then `~/.local/bin` (where `uv tool install` and
/// the FluidAudio install commands put their tools, and which a service started by
/// launchd or systemd usually lacks on its PATH).
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(home) = home() {
        dirs.push(home.join(".local/bin"));
    }
    dirs
}

/// The first executable named `name` in the search directories.
pub(crate) fn find_executable(name: &str) -> Option<PathBuf> {
    search_dirs()
        .into_iter()
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| d.join(name))
        .find(|p| is_executable(p))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `manimgx --version` prints "manimgx 0.1.1".
async fn manimgx_version(path: &Path) -> Option<String> {
    tool_version(path, &["--version"], MANIMGX).await
}

/// Runs `path args` and reads the version from the first line of its output, after
/// `prefix` ("tesseract 5.3.4" → "5.3.4"; "ffmpeg version 6.1.1-3ubuntu5 Copyright …"
/// → "6.1.1-3ubuntu5").
async fn tool_version(path: &Path, args: &[&str], prefix: &str) -> Option<String> {
    let run = tokio::process::Command::new(path)
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // tesseract 4 printed its version on stderr.
    let text = if out.stdout.iter().any(|b| !b.is_ascii_whitespace()) {
        out.stdout
    } else {
        out.stderr
    };
    parse_version(&String::from_utf8_lossy(&text), prefix)
}

fn parse_version(stdout: &str, name: &str) -> Option<String> {
    let line = stdout.lines().map(str::trim).find(|l| !l.is_empty())?;
    let version = line.strip_prefix(name).map(str::trim).unwrap_or(line);
    let version = if name == MANIMGX {
        version
    } else {
        // Only the version word: "6.1.1-3ubuntu5 Copyright (c) …" → "6.1.1-3ubuntu5".
        version.split_whitespace().next().unwrap_or("")
    };
    (!version.is_empty()).then(|| version.to_string())
}

/// Whether Apple's command line tools (needed by `swift build`) are installed.
async fn command_line_tools_installed() -> bool {
    let run = tokio::process::Command::new("/usr/bin/xcode-select")
        .arg("-p")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .status();
    matches!(tokio::time::timeout(Duration::from_secs(10), run).await, Ok(Ok(s)) if s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line() {
        assert_eq!(
            parse_version("manimgx 0.1.1\n", MANIMGX).as_deref(),
            Some("0.1.1")
        );
        assert_eq!(parse_version("\n2.0\n", MANIMGX).as_deref(), Some("2.0"));
        assert_eq!(parse_version("", MANIMGX), None);
        assert_eq!(
            parse_version("tesseract 5.3.4\n leptonica-1.82.0\n", TESSERACT).as_deref(),
            Some("5.3.4")
        );
        assert_eq!(
            parse_version(
                "ffmpeg version 6.1.1-3ubuntu5 Copyright (c) 2000-2023\n",
                "ffmpeg version"
            )
            .as_deref(),
            Some("6.1.1-3ubuntu5")
        );
        assert_eq!(
            parse_version("bubblewrap 0.9.0\n", "bubblewrap").as_deref(),
            Some("0.9.0")
        );
    }

    #[test]
    fn linux_rows_show_install_lines_only_when_missing() {
        let p = Some(PathBuf::from("/usr/bin/x"));
        let t = tesseract_row(None, None);
        assert_eq!((t.name.as_str(), t.found), (TESSERACT, false));
        assert_eq!(
            t.install_commands,
            ["sudo apt install tesseract-ocr tesseract-ocr-eng"]
        );
        let t = tesseract_row(p.clone(), Some("5.3.4".into()));
        assert!(t.found && t.install_commands.is_empty());
        assert_eq!(t.version.as_deref(), Some("5.3.4"));
        assert_eq!(t.path.as_deref(), Some("/usr/bin/x"));

        // ffmpeg without ffprobe isn't enough.
        assert!(!ffmpeg_row(p.clone(), None, None).found);
        assert!(ffmpeg_row(p.clone(), p.clone(), None).found);
        assert_eq!(
            ffmpeg_row(None, p.clone(), None).install_commands,
            ["sudo apt install ffmpeg"]
        );

        let b = bwrap_row(None, None);
        assert_eq!(b.install_commands[0], "sudo apt install bubblewrap");
        assert!(b.install_commands.len() > 1, "the user-namespace setup too");
        assert!(bwrap_row(p.clone(), None).install_commands.is_empty());

        let tts = margin_tts_row(None, false, None);
        assert!(tts.install_commands[0].contains("astral.sh/uv"));
        assert_eq!(
            tts.install_commands.last().unwrap(),
            "chmod +x ~/.local/bin/margin-tts"
        );
        let script = "#!/usr/bin/env -S uv run --script\nprint('hi')\n";
        let tts = margin_tts_row(None, true, Some(script));
        assert_eq!(tts.install_commands.len(), 3);
        assert_eq!(
            tts.install_commands[1],
            "cat > ~/.local/bin/margin-tts <<'MARGIN_TTS_EOF'\n#!/usr/bin/env -S uv run --script\nprint('hi')\nMARGIN_TTS_EOF"
        );
        let found = margin_tts_row(p.clone(), true, Some(script));
        assert!(found.found && found.install_commands.is_empty());

        let pdf = pdfium_row(None);
        assert!(!pdf.found);
        assert!(pdf.install_commands[0].contains("pdfium-binaries"));
        assert!(
            pdf.install_commands
                .iter()
                .any(|c| c.contains(PDFIUM_LIBRARY))
        );
    }

    #[test]
    fn pdfium_is_found_in_the_first_folder_that_has_it() {
        let root = std::env::temp_dir().join(format!("margin-pdfium-{}", std::process::id()));
        let (a, b) = (root.join("a"), root.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        assert_eq!(find_pdfium(&[a.clone(), b.clone()]), None);
        std::fs::write(b.join(PDFIUM_LIBRARY), b"").unwrap();
        assert_eq!(
            find_pdfium(&[a.clone(), b.clone()]),
            Some(b.join(PDFIUM_LIBRARY))
        );
        std::fs::write(a.join(PDFIUM_LIBRARY), b"").unwrap();
        assert_eq!(find_pdfium(&[a.clone(), b]), Some(a.join(PDFIUM_LIBRARY)));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn this_os_lists_its_tools() {
        let names: Vec<String> = detect_tools().await.into_iter().map(|t| t.name).collect();
        if cfg!(target_os = "macos") {
            assert_eq!(names, [MANIMGX, FLUIDAUDIOCLI]);
        } else {
            assert_eq!(
                names,
                [MANIMGX, MARGIN_TTS, TESSERACT, FFMPEG, BWRAP, PDFIUM]
            );
        }
    }
}
