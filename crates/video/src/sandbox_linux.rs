//! The Linux sandbox each ManimGX pass runs under: bubblewrap (`bwrap`).
//!
//! The scene sees a new, mostly empty root built from bind mounts, in its own user,
//! PID, network, IPC, UTS and cgroup namespaces (`--unshare-all --unshare-user`):
//!
//! - `/usr` and `/etc` read-only, and `/bin`, `/sbin`, `/lib`, `/lib64`, … as they are on
//!   the host (symlinks into `/usr` on a merged-`/usr` system, read-only binds otherwise).
//!   `/etc/margin` (the service's `service.toml`) is covered by an empty tmpfs.
//! - The ManimGX tool read-only: the uv tool venv `manimgx` lives in (the folder holding
//!   `pyvenv.cfg`), the Python install the venv was made from (`pyvenv.cfg`'s `home`) and
//!   the interpreter named on the script's `#!` line. Never `/`, the user's home or one of
//!   its ancestors: a program outside a venv gets only its own file bound.
//! - Fonts and caches read-only when present: `~/.local/share/fonts`, `~/.fonts`,
//!   `/var/cache/fontconfig`, and `/sys/devices/system/cpu` (Mesa and LLVM size their
//!   thread pools from it).
//! - Writes only in the video's folder (`--bind <work_dir>`) and a private tmpfs `/tmp`;
//!   the root itself is remounted read-only (`--remount-ro /`) once everything is bound.
//!   `HOME` is the video's folder and `XDG_CACHE_HOME` its `.margin/cache`, so Mesa's
//!   shader cache persists between renders of the same video and nowhere else.
//! - A minimal `/dev` (`--dev`: null, zero, random, urandom, tty, shm) and a new `/proc`.
//!   No `/dev/dri`: manimgx's `manylinux_2_28` wheels bundle Mesa **lavapipe**, a CPU
//!   Vulkan driver, so the scene needs no GPU device.
//! - No network: `--unshare-all` includes `--unshare-net`, so the scene has only a
//!   loopback interface that is down. Name lookups and connections by address both fail.
//! - A new PID namespace, `--die-with-parent` and `--new-session` (no `TIOCSTI` into the
//!   service's terminal): a scene may start processes, but only inside the sandbox, and
//!   they die with the pass. All capabilities are dropped (`--cap-drop ALL`), and with
//!   bubblewrap 0.8 or later `--disable-userns` stops the scene making namespaces of its
//!   own.
//! - The environment is cleared (`--clearenv`) and rebuilt: `HOME`, `TMPDIR`, `PATH`,
//!   `LANG`, `PYTHONDONTWRITEBYTECODE`, `XDG_CACHE_HOME`, plus the service's own `MESA_*`,
//!   `LP_*` (lavapipe, e.g. `LP_NUM_THREADS`), `VK_*`, `GALLIUM_*`, `LIBGL_*`, `MANIMGX_*`,
//!   `LC_ALL` and `TZ`, so an operator can tune the renderer from the systemd unit.
//!   API keys and other secrets in the service's environment never reach the scene.
//!
//! Landlock is not added on top: bubblewrap already confines files and network.
//!
//! # Unprivileged user namespaces (AppArmor)
//!
//! bubblewrap is not setuid on current distributions; it needs the kernel to let an
//! ordinary user create a user namespace. Three switches can forbid that:
//!
//! - Ubuntu 23.10 and later (24.04 included): `kernel.apparmor_restrict_unprivileged_userns
//!   = 1` by default, so an unconfined program may only create a user namespace if an
//!   AppArmor profile grants it `userns`. Fix, preferred (only `bwrap` gains the right):
//!
//!   ```text
//!   sudo tee /etc/apparmor.d/bwrap <<'EOF'
//!   abi <abi/4.0>,
//!   include <tunables/global>
//!   profile bwrap /usr/bin/bwrap flags=(unconfined) {
//!     userns,
//!     include if exists <local/bwrap>
//!   }
//!   EOF
//!   sudo apparmor_parser -r /etc/apparmor.d/bwrap
//!   ```
//!
//!   The blunt alternative turns the restriction off for every program:
//!   `echo kernel.apparmor_restrict_unprivileged_userns=0 | sudo tee
//!   /etc/sysctl.d/60-margin-userns.conf && sudo sysctl --system`.
//!   Either way, a program that can run `bwrap` can get a user namespace through it;
//!   that is the trade bubblewrap makes everywhere it is used (Flatpak, GNOME).
//! - `user.max_user_namespaces = 0` disables user namespaces outright; it must be > 0.
//! - Older Debian kernels: `kernel.unprivileged_userns_clone = 0`; it must be 1.
//!
//! The systemd unit must not filter `unshare`/`clone` with namespace flags
//! (`RestrictNamespaces=`, a `SystemCallFilter=` without `@mount`/namespaces) or bwrap
//! fails the same way. `check` runs `bwrap … -- true` before the first pass and, when
//! it fails, names whichever switch above is set, so a guest set up without them
//! renders nothing, loudly (`RenderError::Sandbox`, with the reason in the video's log).

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// The bubblewrap executable's name.
pub const BWRAP: &str = "bwrap";

/// Where `bwrap` is looked for before `PATH`.
const BWRAP_DIRS: &[&str] = &["/usr/bin", "/bin", "/usr/local/bin"];

/// The command that installs bubblewrap on Debian and Ubuntu.
pub const INSTALL_COMMAND: &str = "sudo apt install bubblewrap";

/// Commands that let `bwrap` create user namespaces on Ubuntu 24.04 (see the module
/// documentation): an AppArmor profile granting `userns` to `/usr/bin/bwrap` only.
pub const USERNS_SETUP: &[&str] = &[
    "printf 'abi <abi/4.0>,\\ninclude <tunables/global>\\nprofile bwrap /usr/bin/bwrap flags=(unconfined) {\\n  userns,\\n  include if exists <local/bwrap>\\n}\\n' | sudo tee /etc/apparmor.d/bwrap",
    "sudo apparmor_parser -r /etc/apparmor.d/bwrap",
];

/// Top-level entries that are symlinks into `/usr` on a merged-`/usr` system and real
/// directories on older ones.
const ROOT_ENTRIES: &[&str] = &["/bin", "/sbin", "/lib", "/lib64", "/lib32", "/libx32"];

/// Bound read-only when present.
const EXTRA_READ_ONLY: &[&str] = &["/var/cache/fontconfig", "/sys/devices/system/cpu"];

/// Under `HOME`, bound read-only when present: the user's own fonts.
const HOME_FONTS: &[&str] = &[".local/share/fonts", ".fonts"];

/// Covered by an empty tmpfs: the service's configuration.
const HIDDEN: &[&str] = &["/etc/margin"];

/// Variables passed from the service's environment into the sandbox, by prefix and by
/// exact name.
const ENV_PREFIXES: &[&str] = &["MESA_", "LP_", "VK_", "GALLIUM_", "LIBGL_", "MANIMGX_"];
const ENV_NAMES: &[&str] = &["LC_ALL", "TZ"];

/// The oldest bubblewrap with `--disable-userns`.
const DISABLE_USERNS_SINCE: (u32, u32, u32) = (0, 8, 0);

/// How long the preflight `bwrap … true` may take.
const PREFLIGHT_LIMIT: Duration = Duration::from_secs(10);

/// One of `/bin`, `/lib`, … as it is on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootEntry {
    /// A symlink (`/lib` → `usr/lib`), recreated inside with `--symlink`.
    Link { path: PathBuf, target: PathBuf },
    /// A real directory, bound read-only.
    Dir(PathBuf),
}

/// What the arguments depend on about this machine, gathered by [`Host::probe`].
/// Kept apart from [`args`] so the arguments can be built and checked anywhere.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Host {
    pub root_entries: Vec<RootEntry>,
    /// Bound read-only at the same path, in order (the tool first, then fonts and caches).
    /// [`args`] drops paths already covered by `/usr`, `/etc` or an earlier entry.
    pub read_only: Vec<PathBuf>,
    /// Covered by an empty tmpfs.
    pub hidden: Vec<PathBuf>,
    /// Passed into the sandbox from the service's environment.
    pub env: Vec<(String, String)>,
    /// bubblewrap is new enough for `--disable-userns`.
    pub disable_userns: bool,
}

impl Host {
    /// Looks at this machine for `program` (the canonical `manimgx` path), given the
    /// bubblewrap version `bwrap --version` printed.
    pub fn probe(program: &Path, bwrap_version: Option<(u32, u32, u32)>) -> Host {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .and_then(|h| std::fs::canonicalize(h).ok());
        let root_entries = ROOT_ENTRIES
            .iter()
            .filter_map(|p| root_entry(Path::new(p)))
            .collect();
        let mut read_only = tool_dirs(program, home.as_deref());
        if let Some(home) = &home {
            read_only.extend(HOME_FONTS.iter().map(|f| home.join(f)));
        }
        read_only.extend(EXTRA_READ_ONLY.iter().map(PathBuf::from));
        read_only.retain(|p| p.exists());
        let hidden = HIDDEN
            .iter()
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .collect();
        Host {
            root_entries,
            read_only,
            hidden,
            env: passed_env(std::env::vars()),
            disable_userns: bwrap_version.is_some_and(|v| v >= DISABLE_USERNS_SINCE),
        }
    }
}

fn root_entry(path: &Path) -> Option<RootEntry> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path).ok()?;
        Some(RootEntry::Link {
            path: path.to_path_buf(),
            target,
        })
    } else if meta.is_dir() {
        Some(RootEntry::Dir(path.to_path_buf()))
    } else {
        None
    }
}

/// The variables from `vars` that go into the sandbox, sorted by name.
pub fn passed_env(vars: impl IntoIterator<Item = (String, String)>) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = vars
        .into_iter()
        .filter(|(k, _)| {
            ENV_NAMES.contains(&k.as_str()) || ENV_PREFIXES.iter().any(|p| k.starts_with(p))
        })
        .collect();
    env.sort();
    env
}

/// What of the tool must be readable for `program` (canonical) to run: the venv it is
/// in, the Python the venv was made from and the `#!` interpreter's install. A program
/// in no venv gets only its own file. `/`, `home` and the folders above `home` are
/// never returned, so a script sitting in the home folder doesn't expose the home.
pub fn tool_dirs(program: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let venv = program
        .parent()
        .filter(|bin| bin.file_name().is_some_and(|n| n == "bin"))
        .and_then(Path::parent)
        .filter(|root| root.join("pyvenv.cfg").is_file());
    match venv {
        Some(root) => {
            dirs.push(root.to_path_buf());
            if let Some(base) = venv_home(&root.join("pyvenv.cfg")) {
                dirs.extend(install_prefix(&base));
            }
        }
        None => dirs.push(program.to_path_buf()),
    }
    if let Some(interpreter) = crate::sandbox::shebang_interpreter(program) {
        let real = std::fs::canonicalize(&interpreter).unwrap_or(interpreter);
        match real.parent() {
            Some(bin) => dirs.extend(install_prefix(bin)),
            None => dirs.push(real),
        }
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        let too_broad = dir.parent().is_none() || home.is_some_and(|h| h.starts_with(&dir));
        if !too_broad && !out.contains(&dir) {
            out.push(dir);
        }
    }
    out
}

/// `home = …` in a `pyvenv.cfg`: the `bin` folder of the Python the venv was made from.
fn venv_home(cfg: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(cfg).ok()?;
    let home = text.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == "home").then(|| value.trim().to_string())
    })?;
    let home = PathBuf::from(home);
    Some(std::fs::canonicalize(&home).unwrap_or(home))
}

/// The install a `bin` folder belongs to (`…/cpython-3.14/bin` → `…/cpython-3.14`), or
/// the folder itself when it isn't called `bin`.
fn install_prefix(bin: &Path) -> Option<PathBuf> {
    if bin.file_name().is_some_and(|n| n == "bin") {
        bin.parent().map(Path::to_path_buf)
    } else {
        Some(bin.to_path_buf())
    }
}

/// `true` when `path` is `base` or inside it, comparing whole components.
fn within(path: &Path, base: &Path) -> bool {
    path.starts_with(base)
}

/// `bwrap` arguments that run `program` (canonical) with writes confined to `work_dir`
/// (canonical). The caller appends the program's own arguments.
pub fn args(host: &Host, program: &Path, work_dir: &Path) -> Vec<String> {
    let s = |p: &Path| p.display().to_string();
    let words = |items: &[&str]| items.iter().map(|w| w.to_string()).collect::<Vec<_>>();
    let mut a = words(&["--unshare-all", "--unshare-user"]);
    if host.disable_userns {
        a.push("--disable-userns".into());
    }
    a.extend(words(&[
        "--die-with-parent",
        "--new-session",
        "--cap-drop",
        "ALL",
    ]));
    a.extend(words(&["--ro-bind", "/usr", "/usr"]));
    for entry in &host.root_entries {
        match entry {
            RootEntry::Link { path, target } => {
                a.extend(["--symlink".into(), s(target), s(path)]);
            }
            RootEntry::Dir(dir) => a.extend(["--ro-bind".into(), s(dir), s(dir)]),
        }
    }
    a.extend(words(&["--ro-bind", "/etc", "/etc"]));
    for hidden in &host.hidden {
        a.extend(["--tmpfs".into(), s(hidden)]);
    }
    a.extend(words(&[
        "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp",
    ]));

    // After `/tmp`, so a tool or video folder under `/tmp` is bound over the tmpfs.
    let mut covered: Vec<PathBuf> = vec![PathBuf::from("/usr"), PathBuf::from("/etc")];
    covered.extend(host.root_entries.iter().filter_map(|e| match e {
        RootEntry::Dir(dir) => Some(dir.clone()),
        RootEntry::Link { .. } => None,
    }));
    for dir in &host.read_only {
        let is_plain = dir.components().all(|c| !matches!(c, Component::ParentDir));
        if !dir.is_absolute() || !is_plain || covered.iter().any(|c| within(dir, c)) {
            continue;
        }
        if within(work_dir, dir) {
            // Binding a parent of the video's folder read-only would hide nothing and
            // expose its siblings; the work dir bind below covers what is needed.
            continue;
        }
        a.extend(["--ro-bind".into(), s(dir), s(dir)]);
        covered.push(dir.clone());
    }
    a.extend(["--bind".into(), s(work_dir), s(work_dir)]);
    // The root tmpfs (with the empty folders bwrap made for the mount points, such as the
    // work dir's parents) read-only too; the remount is not recursive, so `/tmp` and the
    // work dir stay writable.
    a.extend(words(&["--remount-ro", "/"]));

    a.push("--clearenv".into());
    let bin = program.parent().map(s);
    let path = match &bin {
        Some(bin) if !bin.starts_with("/usr/") => format!("{bin}:/usr/local/bin:/usr/bin:/bin"),
        _ => "/usr/local/bin:/usr/bin:/bin".to_string(),
    };
    let fixed = [
        ("HOME", s(work_dir)),
        ("TMPDIR", "/tmp".to_string()),
        ("PATH", path),
        ("LANG", "C.UTF-8".to_string()),
        ("PYTHONDONTWRITEBYTECODE", "1".to_string()),
        ("XDG_CACHE_HOME", s(&work_dir.join(".margin/cache"))),
    ];
    for (key, value) in &fixed {
        a.extend(["--setenv".into(), key.to_string(), value.clone()]);
    }
    for (key, value) in &host.env {
        if fixed.iter().all(|(k, _)| k != key) {
            a.extend(["--setenv".into(), key.clone(), value.clone()]);
        }
    }
    a.extend(["--chdir".into(), s(work_dir), "--".into(), s(program)]);
    a
}

/// `bubblewrap 0.9.0` → `(0, 9, 0)`.
pub fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let word = text
        .split_whitespace()
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))?;
    let mut parts = word.split('.').map(|p| {
        let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<u32>().ok()
    });
    let major = parts.next()??;
    let minor = parts.next().flatten().unwrap_or(0);
    let patch = parts.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

/// Which kernel switch forbids unprivileged user namespaces, from the contents of
/// `/proc/sys/kernel/apparmor_restrict_unprivileged_userns`,
/// `/proc/sys/user/max_user_namespaces` and `/proc/sys/kernel/unprivileged_userns_clone`
/// (`None` where the file doesn't exist).
pub fn userns_blocker(
    apparmor_restrict: Option<&str>,
    max_user_namespaces: Option<&str>,
    unprivileged_clone: Option<&str>,
) -> Option<String> {
    if max_user_namespaces.is_some_and(|v| v.trim() == "0") {
        return Some(
            "user namespaces are disabled (user.max_user_namespaces = 0); set it above 0".into(),
        );
    }
    if unprivileged_clone.is_some_and(|v| v.trim() == "0") {
        return Some(
            "unprivileged user namespaces are disabled (kernel.unprivileged_userns_clone = 0); set it to 1".into(),
        );
    }
    if apparmor_restrict.is_some_and(|v| v.trim() == "1") {
        return Some(format!(
            "AppArmor restricts unprivileged user namespaces (kernel.apparmor_restrict_unprivileged_userns = 1); allow them for bwrap with an AppArmor profile: {}",
            USERNS_SETUP.join(" && ")
        ));
    }
    None
}

fn read_sys(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// The blocker on this machine, if any.
fn this_userns_blocker() -> Option<String> {
    userns_blocker(
        read_sys("/proc/sys/kernel/apparmor_restrict_unprivileged_userns").as_deref(),
        read_sys("/proc/sys/user/max_user_namespaces").as_deref(),
        read_sys("/proc/sys/kernel/unprivileged_userns_clone").as_deref(),
    )
}

/// `true` when a `bwrap:` error line says it couldn't make its namespaces.
pub fn is_namespace_failure(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    ["namespace", "uid map", "setting up uid", "unshare"]
        .iter()
        .any(|w| l.contains(w))
}

/// `bwrap` on this machine: the usual places, then `PATH`.
pub fn find_bwrap() -> Option<PathBuf> {
    let path_dirs = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    BWRAP_DIRS
        .iter()
        .map(PathBuf::from)
        .chain(path_dirs)
        .map(|d| d.join(BWRAP))
        .find(|p| p.is_file())
}

/// `bwrap --version`.
async fn bwrap_version(bwrap: &Path) -> Option<(u32, u32, u32)> {
    let out = tokio::process::Command::new(bwrap)
        .arg("--version")
        .output()
        .await
        .ok()?;
    parse_version(&String::from_utf8_lossy(&out.stdout))
}

/// `true` as a program inside the sandbox, for the preflight.
fn true_program() -> PathBuf {
    ["/usr/bin/true", "/bin/true"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("/usr/bin/true"))
}

/// Can bubblewrap run here? Runs `bwrap --unshare-all --unshare-user … -- true`.
/// `Err` says why not and how to fix it, for the video's log.
pub(crate) async fn check() -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Err("bubblewrap sandboxing is only available on Linux".into());
    }
    let Some(bwrap) = find_bwrap() else {
        return Err(format!(
            "bubblewrap (bwrap) isn't installed; install it with: {INSTALL_COMMAND}"
        ));
    };
    let host = Host {
        root_entries: ROOT_ENTRIES
            .iter()
            .filter_map(|p| root_entry(Path::new(p)))
            .collect(),
        ..Host::default()
    };
    let probe_dir = std::env::temp_dir();
    let probe_dir = std::fs::canonicalize(&probe_dir).unwrap_or(probe_dir);
    let mut cmd = tokio::process::Command::new(&bwrap);
    cmd.args(args(&host, &true_program(), &probe_dir))
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = match tokio::time::timeout(PREFLIGHT_LIMIT, cmd.output()).await {
        Err(_) => return Err("bubblewrap didn't finish a trial run in 10 s".into()),
        Ok(Err(e)) => return Err(format!("couldn't run {}: {e}", bwrap.display())),
        Ok(Ok(out)) => out,
    };
    if out.status.success() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let mut why = format!(
        "bubblewrap couldn't start a sandbox ({}): {said}",
        out.status
    );
    if let Some(blocker) = this_userns_blocker() {
        why.push_str("; ");
        why.push_str(&blocker);
    } else if is_namespace_failure(&said) {
        why.push_str(
            "; bwrap needs unprivileged user namespaces, and the service's systemd unit must not restrict namespaces (RestrictNamespaces=)",
        );
    }
    Err(why)
}

/// The command for one pass: `bwrap <args> <program>`; the caller adds the program's
/// arguments.
pub(crate) async fn command(
    program: &Path,
    work_dir: &Path,
) -> Result<tokio::process::Command, String> {
    let Some(bwrap) = find_bwrap() else {
        return Err(format!(
            "bubblewrap (bwrap) isn't installed; install it with: {INSTALL_COMMAND}"
        ));
    };
    let host = Host::probe(program, bwrap_version(&bwrap).await);
    let mut cmd = tokio::process::Command::new(bwrap);
    cmd.args(args(&host, program, work_dir));
    Ok(cmd)
}

/// The error for a `bwrap:` line on a failed pass's stderr.
pub(crate) fn startup_failure(line: &str) -> String {
    let mut message = format!("the sandbox couldn't start manimgx: {line}");
    if is_namespace_failure(line) {
        match this_userns_blocker() {
            Some(blocker) => {
                message.push_str("; ");
                message.push_str(&blocker);
            }
            None => message.push_str("; bwrap needs unprivileged user namespaces"),
        }
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_and_home_are_never_tool_dirs() {
        let dir = std::env::temp_dir().join(format!("margin-bwrap-home-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let script = dir.join("manimgx");
        std::fs::write(&script, format!("#!{}/python\n", dir.display())).unwrap();
        // The interpreter's folder is the home: not bound.
        assert_eq!(tool_dirs(&script, Some(&dir)), vec![script.clone()]);
        // `/` never.
        std::fs::write(&script, "#!/python\n").unwrap();
        assert_eq!(tool_dirs(&script, None), vec![script.clone()]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
