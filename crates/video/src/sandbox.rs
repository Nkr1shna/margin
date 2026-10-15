//! The sandbox each ManimGX pass runs under, chosen by `SandboxMode`: bubblewrap
//! (`sandbox_linux.rs`, the `linux` module), or none when the service is told to render
//! unsandboxed (tests). Bubblewrap needs Linux: elsewhere `check` says so and the render
//! fails with `RenderError::Sandbox` before anything runs.

use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::SandboxMode;

#[path = "sandbox_linux.rs"]
pub mod linux;

/// Can `mode` run on this machine? Checked before the first pass. `Err` says why not
/// (and how to fix it) for the video's log; the render then fails with
/// `RenderError::Sandbox`.
pub(crate) async fn check(mode: SandboxMode) -> Result<(), String> {
    match mode {
        SandboxMode::Unsandboxed => Ok(()),
        SandboxMode::LinuxBubblewrap => linux::check().await,
    }
}

/// The command for one pass of `program` (canonical) in `work_dir` (canonical) under
/// `mode`. The caller adds the program's arguments.
pub(crate) async fn command(
    mode: SandboxMode,
    program: &Path,
    work_dir: &Path,
) -> Result<Command, String> {
    match mode {
        SandboxMode::Unsandboxed => Ok(Command::new(program)),
        SandboxMode::LinuxBubblewrap => linux::command(program, work_dir).await,
    }
}

/// The error for a pass the sandbox itself failed to start, from the pass's stderr: a
/// line from `bwrap` rather than from the scene.
pub(crate) fn startup_failure(stderr: &[String]) -> Option<String> {
    stderr
        .iter()
        .find(|line| line.starts_with("bwrap:"))
        .map(|line| linux::startup_failure(line))
}

/// The interpreter named on the script's `#!` line, if it is a script.
fn shebang_interpreter(program: &Path) -> Option<PathBuf> {
    let mut head = [0u8; 512];
    let n = {
        use std::io::Read;
        std::fs::File::open(program).ok()?.read(&mut head).ok()?
    };
    let head = &head[..n];
    let rest = head.strip_prefix(b"#!")?;
    let line = rest.split(|&b| b == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?.trim();
    let interpreter = line.split_whitespace().next()?;
    Some(PathBuf::from(interpreter))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_shebang() {
        let dir = std::env::temp_dir().join(format!("margin-video-shebang-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("tool");
        std::fs::write(&script, "#!/opt/py/bin/python -E\nprint()\n").unwrap();
        assert_eq!(
            shebang_interpreter(&script),
            Some(PathBuf::from("/opt/py/bin/python"))
        );
        std::fs::write(&script, b"\x7fELF").unwrap();
        assert_eq!(shebang_interpreter(&script), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_bwrap_line_is_a_startup_failure() {
        let lines = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(startup_failure(&lines(&["Traceback", "bwrap: setting up uid map"])).is_some());
        assert!(startup_failure(&lines(&["Traceback (most recent call last):"])).is_none());
    }
}
