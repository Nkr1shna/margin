//! The Linux sandbox (bubblewrap). The argument tests build and check `bwrap`'s
//! arguments on any OS; the `real_bwrap_*` tests run `bwrap` itself and only run on
//! Linux (ignored elsewhere). Without `bwrap` installed they skip, unless
//! `MARGIN_REAL_TOOLS=1` (the Linux runner) makes a missing `bwrap` a failure.

use std::path::{Path, PathBuf};

use margin_video::bubblewrap::{
    self, Host, RootEntry, args, parse_version, passed_env, tool_dirs, userns_blocker,
};

fn temp(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("margin-bwrap")
        .join(name);
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(dir).unwrap()
}

fn host() -> Host {
    Host {
        root_entries: vec![
            RootEntry::Link {
                path: "/bin".into(),
                target: "usr/bin".into(),
            },
            RootEntry::Link {
                path: "/lib".into(),
                target: "usr/lib".into(),
            },
            RootEntry::Dir("/lib64".into()),
        ],
        read_only: vec![
            "/home/margin/.local/share/uv/tools/manimgx".into(),
            "/home/margin/.local/share/uv/python/cpython-3.14.8-linux-aarch64-gnu".into(),
            "/home/margin/.local/share/fonts".into(),
            "/var/cache/fontconfig".into(),
        ],
        hidden: vec!["/etc/margin".into()],
        env: vec![("LP_NUM_THREADS".into(), "3".into())],
        disable_userns: true,
    }
}

const PROGRAM: &str = "/home/margin/.local/share/uv/tools/manimgx/bin/manimgx";
const WORK: &str = "/var/lib/margin/videos/v1";

/// The position of the option `flag` followed by `values`, if present.
fn find(a: &[String], flag: &str, values: &[&str]) -> Option<usize> {
    a.windows(values.len() + 1)
        .position(|w| w[0] == flag && w[1..].iter().zip(values).all(|(x, y)| x == y))
}

fn has(a: &[String], flag: &str, values: &[&str]) -> bool {
    find(a, flag, values).is_some()
}

fn std_args() -> Vec<String> {
    args(&host(), Path::new(PROGRAM), Path::new(WORK))
}

#[test]
fn namespaces_are_all_unshared_and_the_network_with_them() {
    let a = std_args();
    assert_eq!(a[0], "--unshare-all");
    assert_eq!(a[1], "--unshare-user");
    assert!(a.iter().any(|x| x == "--disable-userns"));
    for flag in ["--die-with-parent", "--new-session"] {
        assert!(a.iter().any(|x| x == flag), "{flag} missing: {a:?}");
    }
    assert!(has(&a, "--cap-drop", &["ALL"]));
    // Nothing shares a namespace back, the network least of all.
    assert!(!a.iter().any(|x| x.starts_with("--share")), "{a:?}");
    // No GPU device: lavapipe renders on the CPU.
    assert!(!a.iter().any(|x| x.contains("/dev/dri")), "{a:?}");
}

#[test]
fn old_bubblewrap_gets_no_disable_userns() {
    let mut h = host();
    h.disable_userns = false;
    let a = args(&h, Path::new(PROGRAM), Path::new(WORK));
    assert!(!a.iter().any(|x| x == "--disable-userns"));
    assert_eq!(&a[..2], ["--unshare-all", "--unshare-user"]);
}

#[test]
fn the_system_is_read_only_and_the_root_entries_kept() {
    let a = std_args();
    assert!(has(&a, "--ro-bind", &["/usr", "/usr"]));
    assert!(has(&a, "--ro-bind", &["/etc", "/etc"]));
    assert!(has(&a, "--symlink", &["usr/bin", "/bin"]));
    assert!(has(&a, "--symlink", &["usr/lib", "/lib"]));
    assert!(has(&a, "--ro-bind", &["/lib64", "/lib64"]));
    // The service's config is covered, after `/etc` is bound.
    let etc = find(&a, "--ro-bind", &["/etc", "/etc"]).unwrap();
    let hidden = find(&a, "--tmpfs", &["/etc/margin"]).unwrap();
    assert!(etc < hidden);
    assert!(has(&a, "--proc", &["/proc"]));
    assert!(has(&a, "--dev", &["/dev"]));
}

#[test]
fn writes_go_only_to_the_work_dir_and_tmp() {
    let a = std_args();
    let rw: Vec<&String> = a
        .iter()
        .zip(a.iter().skip(1))
        .filter(|(flag, _)| *flag == "--bind" || *flag == "--bind-try")
        .map(|(_, path)| path)
        .collect();
    assert_eq!(rw, [WORK], "{a:?}");
    let tmpfs: Vec<&String> = a
        .iter()
        .zip(a.iter().skip(1))
        .filter(|(flag, _)| *flag == "--tmpfs")
        .map(|(_, path)| path)
        .collect();
    assert_eq!(tmpfs, ["/etc/margin", "/tmp"]);
    // The root goes read-only after the last bind.
    let work = find(&a, "--bind", &[WORK, WORK]).unwrap();
    let remount = find(&a, "--remount-ro", &["/"]).unwrap();
    assert!(work < remount, "{a:?}");
    // The tool is read-only.
    assert!(has(
        &a,
        "--ro-bind",
        &[
            "/home/margin/.local/share/uv/tools/manimgx",
            "/home/margin/.local/share/uv/tools/manimgx"
        ]
    ));
    // Nothing from the home beyond the tool and fonts; never the home itself.
    assert!(!has(&a, "--ro-bind", &["/home/margin", "/home/margin"]));
    assert!(!a.iter().any(|x| x == "/home"));
    for flag in ["--ro-bind", "--bind", "--dev-bind"] {
        assert!(!has(&a, flag, &["/", "/"]), "{a:?}");
    }
}

#[test]
fn binds_under_tmp_come_after_the_tmpfs() {
    let mut h = host();
    h.read_only = vec!["/tmp/tool".into()];
    let a = args(
        &h,
        Path::new("/tmp/tool/bin/manimgx"),
        Path::new("/tmp/work"),
    );
    let tmp = find(&a, "--tmpfs", &["/tmp"]).unwrap();
    let tool = find(&a, "--ro-bind", &["/tmp/tool", "/tmp/tool"]).unwrap();
    let work = find(&a, "--bind", &["/tmp/work", "/tmp/work"]).unwrap();
    assert!(tmp < tool && tool < work, "{a:?}");
}

#[test]
fn read_only_paths_are_deduplicated() {
    let mut h = host();
    h.read_only = vec![
        "/usr/lib/python3".into(),             // under /usr
        "/opt/tool".into(),                    // kept
        "/opt/tool".into(),                    // twice
        "/opt/tool/lib".into(),                // inside the one before
        "relative/dir".into(),                 // not absolute
        "/opt/../etc/shadow".into(),           // `..`
        "/var/lib/margin".into(),              // a parent of the work dir
        "/etc/fonts".into(),                   // under /etc
        "/lib64/ld-linux-aarch64.so.1".into(), // under a root dir
    ];
    let a = args(&h, Path::new("/opt/tool/bin/manimgx"), Path::new(WORK));
    let ro: Vec<&String> = a
        .iter()
        .zip(a.iter().skip(1))
        .filter(|(flag, _)| *flag == "--ro-bind")
        .map(|(_, path)| path)
        .collect();
    assert_eq!(ro, ["/usr", "/lib64", "/etc", "/opt/tool"], "{a:?}");
}

#[test]
fn the_environment_is_cleared_and_rebuilt() {
    let a = std_args();
    let clear = a.iter().position(|x| x == "--clearenv").unwrap();
    let first_set = a.iter().position(|x| x == "--setenv").unwrap();
    assert!(clear < first_set);
    assert!(has(&a, "--setenv", &["HOME", WORK]));
    assert!(has(&a, "--setenv", &["TMPDIR", "/tmp"]));
    assert!(has(&a, "--setenv", &["PYTHONDONTWRITEBYTECODE", "1"]));
    assert!(has(&a, "--setenv", &["LANG", "C.UTF-8"]));
    assert!(has(
        &a,
        "--setenv",
        &["XDG_CACHE_HOME", "/var/lib/margin/videos/v1/.margin/cache"]
    ));
    assert!(has(
        &a,
        "--setenv",
        &[
            "PATH",
            "/home/margin/.local/share/uv/tools/manimgx/bin:/usr/local/bin:/usr/bin:/bin"
        ]
    ));
    assert!(has(&a, "--setenv", &["LP_NUM_THREADS", "3"]));
    // A passed variable can't replace one the sandbox sets.
    let mut h = host();
    h.env.push(("HOME".into(), "/root".into()));
    let a = args(&h, Path::new(PROGRAM), Path::new(WORK));
    assert!(!has(&a, "--setenv", &["HOME", "/root"]));
}

#[test]
fn the_program_runs_last_in_the_work_dir() {
    let a = std_args();
    let n = a.len();
    assert_eq!(&a[n - 4..], ["--chdir", WORK, "--", PROGRAM]);
}

#[test]
fn only_renderer_variables_pass_into_the_sandbox() {
    let vars = [
        ("OPENAI_API_KEY", "sk-secret"),
        ("ANTHROPIC_API_KEY", "secret"),
        ("MARGIN_CONFIG", "/etc/margin/service.toml"),
        ("HOME", "/home/margin"),
        ("LP_NUM_THREADS", "3"),
        ("MESA_SHADER_CACHE_MAX_SIZE", "1G"),
        ("VK_ICD_FILENAMES", "/x.json"),
        ("MANIMGX_QUALITY", "high"),
        ("TZ", "UTC"),
        ("LC_ALL", "C.UTF-8"),
    ]
    .map(|(k, v)| (k.to_string(), v.to_string()));
    let names: Vec<String> = passed_env(vars).into_iter().map(|(k, _)| k).collect();
    assert_eq!(
        names,
        [
            "LC_ALL",
            "LP_NUM_THREADS",
            "MANIMGX_QUALITY",
            "MESA_SHADER_CACHE_MAX_SIZE",
            "TZ",
            "VK_ICD_FILENAMES"
        ]
    );
}

/// A uv tool install: `tools/manimgx/{bin/manimgx, bin/python → python/…/bin/python3.14,
/// pyvenv.cfg}`.
fn uv_layout(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let python = root.join("python/cpython-3.14.8");
    std::fs::create_dir_all(python.join("bin")).unwrap();
    std::fs::write(python.join("bin/python3.14"), "").unwrap();
    let venv = root.join("tools/manimgx");
    std::fs::create_dir_all(venv.join("bin")).unwrap();
    std::fs::write(
        venv.join("pyvenv.cfg"),
        format!(
            "home = {}\nimplementation = CPython\nversion_info = 3.14.8\n",
            python.join("bin").display()
        ),
    )
    .unwrap();
    std::os::unix::fs::symlink(python.join("bin/python3.14"), venv.join("bin/python")).unwrap();
    let program = venv.join("bin/manimgx");
    std::fs::write(
        &program,
        format!("#!{}\nimport manimgx\n", venv.join("bin/python").display()),
    )
    .unwrap();
    (program, venv, python)
}

#[test]
fn tool_dirs_of_a_uv_tool_are_the_venv_and_its_python() {
    let root = temp("uv");
    let (program, venv, python) = uv_layout(&root);
    assert_eq!(
        tool_dirs(&program, Some(Path::new("/nonexistent-home"))),
        [venv.clone(), python.clone()]
    );
    // The home above them is fine (it isn't bound itself).
    assert_eq!(tool_dirs(&program, Some(&root)), [venv, python]);
}

#[test]
fn a_program_outside_a_venv_is_bound_alone() {
    let root = temp("bare");
    let program = root.join("manimgx");
    std::fs::write(&program, "\x7fELF").unwrap();
    assert_eq!(tool_dirs(&program, None), std::slice::from_ref(&program));
    // A script whose interpreter sits in the home: the home isn't bound.
    std::fs::write(&program, format!("#!{}/python\n", root.display())).unwrap();
    assert_eq!(
        tool_dirs(&program, Some(&root)),
        std::slice::from_ref(&program)
    );
}

#[test]
fn probe_finds_the_tool_and_the_version_switch() {
    let root = temp("probe");
    let (program, venv, python) = uv_layout(&root);
    let h = Host::probe(&program, Some((0, 9, 0)));
    assert!(h.disable_userns);
    assert!(h.read_only.starts_with(&[venv, python]), "{h:?}");
    assert!(h.read_only.iter().all(|p| p.exists()));
    assert!(!Host::probe(&program, Some((0, 7, 1))).disable_userns);
    assert!(!Host::probe(&program, None).disable_userns);
    // The probe never passes secrets.
    assert!(
        h.env
            .iter()
            .all(|(k, _)| !k.contains("KEY") && !k.contains("TOKEN"))
    );
}

#[test]
fn versions() {
    assert_eq!(parse_version("bubblewrap 0.9.0\n"), Some((0, 9, 0)));
    assert_eq!(parse_version("bubblewrap 0.11.0-rc1"), Some((0, 11, 0)));
    assert_eq!(parse_version("bubblewrap 0.4"), Some((0, 4, 0)));
    assert_eq!(parse_version("bwrap: unknown option"), None);
}

#[test]
fn the_userns_blocker_is_named_with_its_fix() {
    assert_eq!(userns_blocker(None, None, None), None);
    assert_eq!(
        userns_blocker(Some("0\n"), Some("63413\n"), Some("1\n")),
        None
    );
    let apparmor = userns_blocker(Some("1\n"), Some("63413\n"), None).unwrap();
    assert!(apparmor.contains("kernel.apparmor_restrict_unprivileged_userns = 1"));
    assert!(apparmor.contains("/etc/apparmor.d/bwrap"));
    assert!(apparmor.contains("apparmor_parser -r"));
    assert!(apparmor.contains("userns,"));
    let off = userns_blocker(Some("1\n"), Some("0\n"), None).unwrap();
    assert!(off.contains("user.max_user_namespaces = 0"), "{off}");
    let debian = userns_blocker(None, Some("100"), Some("0")).unwrap();
    assert!(
        debian.contains("kernel.unprivileged_userns_clone = 0"),
        "{debian}"
    );
    assert!(bubblewrap::is_namespace_failure(
        "bwrap: setting up uid map: Permission denied"
    ));
    assert!(bubblewrap::is_namespace_failure(
        "bwrap: No permissions to create a new namespace, likely because the kernel does not allow non-privileged user namespaces."
    ));
    assert!(!bubblewrap::is_namespace_failure(
        "bwrap: Can't find source path /x: No such file or directory"
    ));
}

/// Off Linux bubblewrap is refused before anything runs (no manimgx needed), and the log
/// says why.
#[cfg(not(target_os = "linux"))]
#[tokio::test]
async fn bubblewrap_is_refused_off_linux() {
    use margin_video::{RenderError, RenderJob, RenderProgress, SandboxMode, Voice, render};
    let job = RenderJob {
        work_dir: temp("off-linux"),
        scene_code: String::new(),
    };
    let config = margin_video::RenderConfig {
        manimgx: PathBuf::from("/nonexistent/manimgx"),
        voice: Voice::Silent,
        sandbox: SandboxMode::LinuxBubblewrap,
        width: 320,
        height: 180,
        fps: 15,
    };
    let mut events = Vec::new();
    let result = render(&job, &config, |p| events.push(p)).await;
    assert!(matches!(result, Err(RenderError::Sandbox)), "{result:?}");
    let message = RenderError::Sandbox.to_string();
    assert!(message.contains("bubblewrap, on Linux"), "{message}");
    assert!(
        events.iter().any(|e| matches!(e, RenderProgress::Log(l)
            if l.starts_with("margin: ") && l.contains("only available on"))),
        "{events:?}"
    );
}

/// `bwrap` on this machine, or `None` to skip (a failure under `MARGIN_REAL_TOOLS=1`).
fn real_bwrap(test: &str) -> Option<PathBuf> {
    let found = bubblewrap::find_bwrap();
    if found.is_none() {
        assert!(
            std::env::var("MARGIN_REAL_TOOLS").as_deref() != Ok("1"),
            "MARGIN_REAL_TOOLS=1 but bwrap isn't installed ({})",
            bubblewrap::INSTALL_COMMAND
        );
        eprintln!("SKIPPED {test}: bwrap isn't installed");
    }
    found
}

/// Runs `sh -c <script> sh <args…>` under the sandbox's arguments, `work` as the work dir.
fn run_sh(bwrap: &Path, work: &Path, script: &str, script_args: &[&str]) -> (bool, String, String) {
    let sh = std::fs::canonicalize("/bin/sh").unwrap();
    let host = Host::probe(&sh, None);
    let mut cmd = std::process::Command::new(bwrap);
    cmd.args(args(&host, &sh, work))
        .arg("-c")
        .arg(script)
        .arg("sh")
        .args(script_args)
        .env("MARGIN_TEST_SECRET", "do-not-pass");
    let out = cmd.output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[cfg_attr(not(target_os = "linux"), ignore = "bubblewrap runs on Linux only")]
#[test]
fn real_bwrap_confines_files_network_env_and_processes() {
    let Some(bwrap) = real_bwrap("real_bwrap_confines_files_network_env_and_processes") else {
        return;
    };
    let work = temp("real/work");
    let outside = temp("real/outside");
    let secret = outside.join("secret.txt");
    std::fs::write(&secret, "secret").unwrap();
    let script = r#"
echo "home=$HOME"
echo "tmpdir=$TMPDIR"
echo "secret_env=${MARGIN_TEST_SECRET:-unset}"
echo inside > inside.txt && echo "inside=ok"
if echo escaped > "$1/escaped.txt" 2>/dev/null; then echo "outside=written"; else echo "outside=blocked"; fi
if cat "$1/secret.txt" >/dev/null 2>&1; then echo "read=yes"; else echo "read=no"; fi
if echo x > /usr/margin-escape 2>/dev/null; then echo "usr=written"; else echo "usr=blocked"; fi
if echo x > /margin-escape 2>/dev/null; then echo "root=written"; else echo "root=blocked"; fi
if echo x > "$(dirname "$PWD")/margin-escape" 2>/dev/null; then echo "parent=written"; else echo "parent=blocked"; fi
echo "tmp=$(echo t > /tmp/t && cat /tmp/t)"
echo "ifaces=$(tail -n +3 /proc/net/dev | cut -d: -f1 | tr -d ' ' | tr '\n' ',')"
echo "pids=$(ls /proc | grep -c '^[0-9]')"
"#;
    let (ok, out, err) = run_sh(&bwrap, &work, script, &[&outside.display().to_string()]);
    assert!(ok, "bwrap failed: {err}\n{out}");
    let line = |key: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix(&format!("{key}=")))
            .unwrap_or_else(|| panic!("no {key} in {out}\n{err}"))
            .to_string()
    };
    assert_eq!(line("home"), work.display().to_string());
    assert_eq!(line("tmpdir"), "/tmp");
    assert_eq!(line("secret_env"), "unset");
    assert_eq!(line("inside"), "ok");
    assert!(work.join("inside.txt").is_file());
    assert_eq!(line("outside"), "blocked");
    assert!(!outside.join("escaped.txt").exists());
    assert_eq!(line("read"), "no");
    assert_eq!(line("usr"), "blocked");
    assert_eq!(line("root"), "blocked");
    assert_eq!(line("parent"), "blocked");
    assert_eq!(line("tmp"), "t");
    // A new network namespace: loopback only.
    assert_eq!(line("ifaces"), "lo,", "{out}");
    // A new PID namespace: bwrap, sh and the few commands above.
    let pids: u32 = line("pids").parse().unwrap();
    assert!(pids < 8, "{pids} processes visible");
}

#[cfg_attr(not(target_os = "linux"), ignore = "bubblewrap runs on Linux only")]
#[test]
fn real_bwrap_blocks_connections() {
    let Some(bwrap) = real_bwrap("real_bwrap_blocks_connections") else {
        return;
    };
    let work = temp("real/net");
    // `/dev/tcp` is bash's; with only sh, use the kernel directly through python3 if the
    // host has it (Ubuntu does), else fall back to a lookup through getent.
    let script = r#"
if command -v python3 >/dev/null; then
  python3 -c '
import socket
try:
    socket.create_connection(("1.1.1.1", 443), timeout=5).close()
    print("connect=ok")
except OSError as e:
    print("connect=blocked", e)
try:
    socket.getaddrinfo("example.com", 443)
    print("lookup=ok")
except OSError as e:
    print("lookup=blocked", e)
'
else
  echo "connect=blocked no python3"
  if getent hosts example.com >/dev/null; then echo "lookup=ok"; else echo "lookup=blocked"; fi
fi
"#;
    let (ok, out, err) = run_sh(&bwrap, &work, script, &[]);
    assert!(ok, "bwrap failed: {err}\n{out}");
    assert!(out.contains("connect=blocked"), "{out}");
    assert!(out.contains("lookup=blocked"), "{out}");
}
