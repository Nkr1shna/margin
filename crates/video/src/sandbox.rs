//! The sandbox each ManimGX pass runs under, chosen by `SandboxMode`: on macOS the
//! `sandbox-exec` profile below, on Linux bubblewrap (`sandbox_linux.rs`, the `linux`
//! module). `SandboxMode::native()` names the one this build's OS has; asking for the
//! other one fails with `RenderError::Sandbox` before anything runs.
//!
//! # macOS: `sandbox-exec`
//!
//! The profile denies everything, then allows only what a ManimGX render was found to
//! need on macOS 27 (Apple silicon), by removing one allowance at a time and rendering:
//!
//! - `process-exec` of the `manimgx` script and its Python interpreter (the shebang
//!   path and the file it links to). `sandbox-exec` execs the script; the kernel then
//!   execs the interpreter. No `process-fork`: a scene cannot start other programs.
//! - `sysctl-read`: Python and Metal read hardware facts at start-up.
//! - `file-read*`: reads anywhere (Python, ManimGX, fonts, the system's frameworks).
//! - `file-write*` only in the video's folder, the temp dirs (`$TMPDIR`, the user's
//!   `DARWIN_USER_TEMP_DIR`, `/private/tmp`), Metal's shader caches in the user's
//!   `DARWIN_USER_CACHE_DIR` (`com.apple.metal`, `com.apple.metalfe`,
//!   `com.apple.gpuarchiver`; without them every render recompiles its shaders, about
//!   4 s slower) and `/dev/null`.
//! - `mach-lookup` of two services: `com.apple.bsd.dirhelper` (finds the user's cache
//!   dir, so Metal can use its shader cache) and the XPC service
//!   `com.apple.MTLCompilerService` (compiles shaders on a cache miss). Without both,
//!   Metal finds no GPU ("no GPU device: Parent device is lost").
//! - `iokit-open` of the GPU's user clients, `AGXDeviceUserClient` and
//!   `IOSurfaceRootUserClient`. Without them: "metal found no adapters".
//!
//! Everything else is denied, network included: no `network*` rule, and no mach-lookup
//! of DNS (`mDNSResponder`) or of services that could fetch for the scene
//! (LaunchServices, `nsurlsessiond`).

use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::SandboxMode;

#[path = "sandbox_linux.rs"]
pub mod linux;

impl SandboxMode {
    /// The sandbox of the OS this was built for: `LinuxBubblewrap` on Linux,
    /// `MacosSandboxExec` everywhere else (where rendering is then refused, as no
    /// sandbox exists there).
    pub const fn native() -> SandboxMode {
        if cfg!(target_os = "linux") {
            SandboxMode::LinuxBubblewrap
        } else {
            SandboxMode::MacosSandboxExec
        }
    }
}

/// Can `mode` run on this machine? Checked before the first pass. `Err` says why not
/// (and how to fix it) for the video's log; the render then fails with
/// `RenderError::Sandbox`.
pub(crate) async fn check(mode: SandboxMode) -> Result<(), String> {
    match mode {
        SandboxMode::Unsandboxed => Ok(()),
        SandboxMode::MacosSandboxExec => {
            if !cfg!(target_os = "macos") {
                Err("the sandbox-exec sandbox is only available on macOS".into())
            } else if !Path::new(SANDBOX_EXEC).is_file() {
                Err(format!("{SANDBOX_EXEC} is missing"))
            } else {
                Ok(())
            }
        }
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
        SandboxMode::MacosSandboxExec => {
            let mut cmd = Command::new(SANDBOX_EXEC);
            cmd.args(sandbox_args(program, work_dir).await?);
            Ok(cmd)
        }
        SandboxMode::LinuxBubblewrap => linux::command(program, work_dir).await,
    }
}

/// The error for a pass the sandbox itself failed to start, from the pass's stderr: a
/// line from `sandbox-exec` or `bwrap` rather than from the scene.
pub(crate) fn startup_failure(stderr: &[String]) -> Option<String> {
    stderr.iter().find_map(|line| {
        if line.starts_with("sandbox-exec:") {
            Some(format!("the sandbox couldn't start manimgx: {line}"))
        } else if line.starts_with("bwrap:") {
            Some(linux::startup_failure(line))
        } else {
            None
        }
    })
}

pub(crate) const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

const PROFILE: &str = r#"(version 1)
(deny default)
(allow process-exec
  (literal (param "EXEC_SCRIPT"))
  (literal (param "EXEC_INTERPRETER"))
  (literal (param "EXEC_INTERPRETER_REAL")))
(allow sysctl-read)
(allow file-read*)
(allow file-write*
  (subpath (param "WORK_DIR"))
  (subpath (param "TMPDIR"))
  (subpath (param "USER_TEMP_DIR"))
  (subpath "/private/tmp")
  (subpath (string-append (param "USER_CACHE_DIR") "/com.apple.metal"))
  (subpath (string-append (param "USER_CACHE_DIR") "/com.apple.metalfe"))
  (subpath (string-append (param "USER_CACHE_DIR") "/com.apple.gpuarchiver"))
  (literal "/dev/null"))
(allow mach-lookup
  (global-name "com.apple.bsd.dirhelper")
  (xpc-service-name "com.apple.MTLCompilerService"))
(allow iokit-open
  (iokit-user-client-class "AGXDeviceUserClient" "IOSurfaceRootUserClient"))
"#;

/// Arguments for `sandbox-exec` that run `program` (the canonical `manimgx` path) with
/// writes confined to `work_dir` (canonical). The caller appends the program's own
/// arguments.
async fn sandbox_args(program: &Path, work_dir: &Path) -> Result<Vec<String>, String> {
    check(SandboxMode::MacosSandboxExec).await?;
    let interpreter = shebang_interpreter(program).unwrap_or_else(|| program.to_path_buf());
    let interpreter_real =
        std::fs::canonicalize(&interpreter).unwrap_or_else(|_| interpreter.clone());

    let user_temp = darwin_dir("DARWIN_USER_TEMP_DIR").await;
    let user_cache = darwin_dir("DARWIN_USER_CACHE_DIR").await;
    let tmpdir = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .and_then(|p| std::fs::canonicalize(p).ok());
    // An unknown directory is replaced by the work dir, which is allowed anyway.
    let or_work = |p: Option<PathBuf>| p.unwrap_or_else(|| work_dir.to_path_buf());

    let params = [
        ("EXEC_SCRIPT", program.to_path_buf()),
        ("EXEC_INTERPRETER", interpreter),
        ("EXEC_INTERPRETER_REAL", interpreter_real),
        ("WORK_DIR", work_dir.to_path_buf()),
        ("TMPDIR", or_work(tmpdir)),
        ("USER_TEMP_DIR", or_work(user_temp)),
        ("USER_CACHE_DIR", or_work(user_cache)),
    ];
    let mut args = vec!["-p".to_string(), PROFILE.to_string()];
    for (key, value) in params {
        args.push("-D".to_string());
        args.push(format!("{key}={}", value.display()));
    }
    args.push(program.display().to_string());
    Ok(args)
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

/// `getconf DARWIN_USER_*_DIR`, canonical (under `/private/var/folders`).
async fn darwin_dir(name: &str) -> Option<PathBuf> {
    let out = tokio::process::Command::new("/usr/bin/getconf")
        .arg(name)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    std::fs::canonicalize(text).ok()
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
    fn profile_denies_by_default_and_has_no_network_rule() {
        let p = PROFILE;
        assert!(p.contains("(deny default)"));
        assert!(!p.contains("network"));
        assert!(!p.contains("process-fork"));
    }
}
