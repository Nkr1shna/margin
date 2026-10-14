//! Finds `manimgx` and `fluidaudiocli` on the service Mac, with the commands that install
//! them when they are missing. Nothing here runs an installer.

use std::path::{Path, PathBuf};
use std::time::Duration;

use margin_shared::ToolStatus;

pub(crate) const MANIMGX: &str = "manimgx";
pub(crate) const FLUIDAUDIOCLI: &str = "fluidaudiocli";

/// The FluidAudio release the install commands build.
const FLUIDAUDIO_TAG: &str = "v0.17.7";

pub(crate) async fn detect_tools() -> Vec<ToolStatus> {
    let manimgx = find_executable(MANIMGX);
    let manimgx_version = match &manimgx {
        Some(path) => manimgx_version(path).await,
        None => None,
    };
    let fluid = find_executable(FLUIDAUDIOCLI);

    let manimgx_install = if manimgx.is_some() {
        Vec::new()
    } else {
        let mut cmds = Vec::new();
        if find_executable("uv").is_none() {
            cmds.push("curl -LsSf https://astral.sh/uv/install.sh | sh".to_string());
            cmds.push("source $HOME/.local/bin/env".to_string());
        }
        cmds.push("uv tool install --python 3.14 manimgx".to_string());
        cmds
    };

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
        ToolStatus {
            name: MANIMGX.to_string(),
            found: manimgx.is_some(),
            path: manimgx.map(|p| p.display().to_string()),
            version: manimgx_version,
            install_commands: manimgx_install,
        },
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

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// The directories searched: `$PATH`, then `~/.local/bin` (where `uv tool install` and
/// the FluidAudio install commands put their tools, and which a service started by
/// launchd usually lacks on its PATH).
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
    let run = tokio::process::Command::new(path)
        .arg("--version")
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
    parse_version(&String::from_utf8_lossy(&out.stdout), MANIMGX)
}

fn parse_version(stdout: &str, name: &str) -> Option<String> {
    let line = stdout.lines().map(str::trim).find(|l| !l.is_empty())?;
    let version = line.strip_prefix(name).map(str::trim).unwrap_or(line);
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
    }
}
