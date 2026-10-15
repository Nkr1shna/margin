//! What the app leaves behind for diagnosing a problem: a rotating log of its `tracing`
//! output, and a crash report for each panic. A release app has no terminal, so without
//! these a panic (which ends the app when it happens on the main thread) leaves no trace.
//!
//! Everything goes to `log_dir()`: `~/Library/Logs/<app name>` on macOS (Console.app
//! shows it under Log Reports), a `Logs` folder in the app's data directory on iOS and
//! Android, and in the data directory elsewhere. There:
//! - `margin.log`, `margin.1.log` … `margin.4.log`: the log, newest first, at most
//!   `LOG_MAX_BYTES` each (warnings and up from libraries, info and up from the app);
//! - `crash-<time>.log`: one per panic, the last `KEEP_CRASHES` kept;
//! - `last-crash.log`: a copy of the report of a panic on the main thread, which quit
//!   the app; the next launch takes it (`take_last_crash`) and says so (`CrashNotice`).
//!
//! In release builds such a panic then aborts (`ABORT_ON_QUIT`), so macOS keeps a crash
//! report of its own in `~/Library/Logs/DiagnosticReports` as well.
//!
//! Reports carry the panic's message, place, thread and backtrace, the app's version and
//! build, the platform and the screen. Never a token or document text: the app logs
//! neither, and `scrub` takes out quoted text and anything token-shaped from a panic
//! message (a string slicing panic quotes the string it was slicing).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::config::APP_NAME;

/// The app's version and the commit it was built from (build.rs).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const BUILD: &str = env!("MARGIN_BUILD_SHA");

/// The log rotates at this size; `LOG_FILES` files are kept in all.
const LOG_MAX_BYTES: u64 = 1024 * 1024;
const LOG_FILES: usize = 5;
const LOG_NAME: &str = "margin";
const KEEP_CRASHES: usize = 5;
const LAST_CRASH: &str = "last-crash.log";

/// Where the log and crash reports go (module docs).
pub fn log_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir().map(|h| h.join("Library").join("Logs").join(APP_NAME))
    }
    #[cfg(any(target_os = "ios", target_os = "android"))]
    {
        crate::config::data_dir().map(|d| d.join("Logs"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "android")))]
    {
        crate::config::data_dir().map(|d| d.join("logs"))
    }
}

/// The log file, shared by the `tracing` output and the panic hook.
static LOG: OnceLock<Mutex<Rotating>> = OnceLock::new();
/// The screen on show, for crash reports (`set_screen`).
static SCREEN: Mutex<String> = Mutex::new(String::new());

/// Start the log and the panic hook. Call once, first thing in `main`.
pub fn init() {
    let Some(dir) = log_dir() else {
        install_panic_hook(None, ABORT_ON_QUIT);
        return;
    };
    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!("couldn't create {}: {error}", dir.display());
    }
    let _ = LOG.set(Mutex::new(Rotating::new(
        &dir,
        LOG_NAME,
        LOG_MAX_BYTES,
        LOG_FILES,
    )));
    init_tracing();
    install_panic_hook(Some(dir), ABORT_ON_QUIT);
    tracing::info!(
        "{APP_NAME} {VERSION} (build {BUILD}) on {} {} started",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
}

/// Record which screen is on show (a route's path, or "sign-in"), for crash reports.
pub fn set_screen(screen: &str) {
    if let Ok(mut s) = SCREEN.try_lock()
        && *s != screen
    {
        s.clear();
        s.push_str(screen);
    }
}

/// The report of a panic that quit the app last time, if there was one. The marker is
/// removed as it's read, so the notice shows once; the `crash-*.log` copy stays.
pub fn take_last_crash() -> Option<String> {
    take_last_crash_in(&log_dir()?)
}

fn take_last_crash_in(dir: &Path) -> Option<String> {
    let path = dir.join(LAST_CRASH);
    let report = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    (!report.trim().is_empty()).then_some(report)
}

/// `tracing` to stderr (as Dioxus's own logger would) and to the log file. Set before
/// Dioxus launches, so its logger stands aside (it checks for a global subscriber).
fn init_tracing() {
    use tracing::level_filters::LevelFilter;
    use tracing_subscriber::filter::Targets;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = Targets::new()
        .with_default(LevelFilter::WARN)
        .with_target("margin_app", LevelFilter::INFO);
    let file = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(|| LogWriter);
    let stderr = tracing_subscriber::fmt::layer().with_writer(io::stderr);
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(file)
        .with(stderr)
        .try_init();
}

/// A `tracing` writer onto `LOG`.
struct LogWriter;

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(log) = LOG.get() {
            let mut log = log.lock().unwrap_or_else(|e| e.into_inner());
            log.write_all(buf)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Release builds end the process with `abort` after a panic that quits the app, so the
/// system records a crash too (macOS: a report in Console, under Crash Reports), not the
/// quiet exit tao makes of a rethrown panic. Only the app: `panic = "abort"` in the
/// release profile would apply to the whole workspace, the service included.
const ABORT_ON_QUIT: bool = !cfg!(debug_assertions);

fn install_panic_hook(dir: Option<PathBuf>, abort_on_quit: bool) {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let thread = thread.name().unwrap_or("unnamed").to_string();
        // The main thread's panic ends the app (tao stops the event loop and rethrows it;
        // on iOS it aborts); a background thread's doesn't.
        let quits = thread == "main";
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "(no message)".into());
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".into());
        // Not `lock`: the panic may have come from code holding it.
        let screen = SCREEN
            .try_lock()
            .map(|s| s.clone())
            .unwrap_or_else(|_| "unknown".into());
        let report = Panic {
            message: &message,
            location: &location,
            thread: &thread,
            screen: &screen,
            backtrace: &std::backtrace::Backtrace::force_capture().to_string(),
            time: &chrono::Utc::now().to_rfc3339(),
        }
        .report();
        if let Some(dir) = &dir
            && let Err(error) = save_crash(dir, &report, quits)
        {
            eprintln!("couldn't save the crash report: {error}");
        }
        if let Some(log) = LOG.get()
            && let Ok(mut log) = log.try_lock()
        {
            let _ = log.write_all(format!("PANIC\n{report}\n").as_bytes());
        }
        default(info);
        if quits && abort_on_quit {
            std::process::abort();
        }
    }));
}

/// A panic, for its report.
struct Panic<'a> {
    message: &'a str,
    location: &'a str,
    thread: &'a str,
    screen: &'a str,
    backtrace: &'a str,
    time: &'a str,
}

impl Panic<'_> {
    fn report(&self) -> String {
        let screen = if self.screen.is_empty() {
            "starting"
        } else {
            self.screen
        };
        format!(
            "{APP_NAME} {VERSION} (build {BUILD}) on {os} {arch}\n\
             Time: {time}\n\
             Thread: {thread}\n\
             Screen: {screen}\n\
             Panicked at {location}:\n\
             {message}\n\n\
             Backtrace:\n{backtrace}",
            os = std::env::consts::OS,
            arch = std::env::consts::ARCH,
            time = self.time,
            thread = self.thread,
            location = self.location,
            message = scrub(self.message),
            backtrace = self.backtrace,
        )
    }
}

/// Write `report` as a new `crash-*.log` (dropping the oldest past `KEEP_CRASHES`), and as
/// `last-crash.log` when the panic `quit` the app.
fn save_crash(dir: &Path, report: &str, quit: bool) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S%.3f");
    std::fs::write(dir.join(format!("crash-{stamp}.log")), report)?;
    let mut crashes: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("crash-") && n.ends_with(".log"))
        })
        .collect();
    crashes.sort();
    let excess = crashes.len().saturating_sub(KEEP_CRASHES);
    for old in &crashes[..excess] {
        let _ = std::fs::remove_file(old);
    }
    if quit {
        std::fs::write(dir.join(LAST_CRASH), report)?;
    }
    Ok(())
}

/// A panic message without what might be private: text in backticks past a few words
/// (`str` slicing panics quote the string, `unwrap` on an `Err` prints the error), and
/// anything token-shaped. Capped in length.
pub fn scrub(message: &str) -> String {
    const MAX_QUOTE: usize = 48;
    const MAX_LEN: usize = 2000;
    let mut out = String::with_capacity(message.len().min(MAX_LEN));
    let mut parts = message.split('`');
    if let Some(first) = parts.next() {
        out.push_str(first);
    }
    // Odd parts are inside backticks; a trailing unmatched quote counts as inside.
    for (i, part) in parts.enumerate() {
        out.push('`');
        if i % 2 == 0 && part.chars().count() > MAX_QUOTE {
            out.push_str("[text removed]");
        } else {
            out.push_str(part);
        }
    }
    let scrubbed: Vec<String> = out.split(' ').map(scrub_word).collect();
    let joined = scrubbed.join(" ");
    if joined.chars().count() > MAX_LEN {
        let cut: String = joined.chars().take(MAX_LEN).collect();
        format!("{cut}…")
    } else {
        joined
    }
}

/// A word that looks like a secret (an API key, a bearer token, a long random run).
fn scrub_word(word: &str) -> String {
    let bare = word.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    let keyish = ["sk-", "sk_", "mgn_", "ghp_", "gho_", "xox"]
        .iter()
        .any(|p| bare.starts_with(p));
    let long_random = bare.len() >= 32
        && bare
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && bare.chars().any(|c| c.is_ascii_digit())
        && bare.chars().any(|c| c.is_ascii_alphabetic());
    if keyish || long_random {
        "[secret removed]".into()
    } else if let Some((key, _)) = word.split_once('=').filter(|(k, _)| {
        let k = k.to_ascii_lowercase();
        k.contains("token") || k.contains("key") || k.contains("secret")
    }) {
        format!("{key}=[secret removed]")
    } else {
        word.to_string()
    }
}

/// A log file that moves aside at `max_bytes`: `name.log` → `name.1.log` → … up to
/// `files` in all, the oldest dropped.
struct Rotating {
    dir: PathBuf,
    name: String,
    max_bytes: u64,
    files: usize,
    file: Option<File>,
    written: u64,
}

impl Rotating {
    fn new(dir: &Path, name: &str, max_bytes: u64, files: usize) -> Self {
        let mut log = Self {
            dir: dir.to_path_buf(),
            name: name.to_string(),
            max_bytes,
            files: files.max(1),
            file: None,
            written: 0,
        };
        log.open();
        log
    }

    fn path(&self, n: usize) -> PathBuf {
        if n == 0 {
            self.dir.join(format!("{}.log", self.name))
        } else {
            self.dir.join(format!("{}.{n}.log", self.name))
        }
    }

    fn open(&mut self) {
        let path = self.path(0);
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();
        self.written = self
            .file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map_or(0, |m| m.len());
    }

    fn rotate(&mut self) {
        self.file = None;
        let _ = std::fs::remove_file(self.path(self.files - 1));
        for n in (0..self.files - 1).rev() {
            let _ = std::fs::rename(self.path(n), self.path(n + 1));
        }
        self.open();
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        if self.written > 0 && self.written + buf.len() as u64 > self.max_bytes {
            self.rotate();
        }
        if self.file.is_none() {
            // The folder may have been removed while the app ran.
            let _ = std::fs::create_dir_all(&self.dir);
            self.open();
        }
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        file.write_all(buf)?;
        self.written += buf.len() as u64;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("margin-diagnostics-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_log_rotates_and_keeps_a_few_files() {
        let dir = temp("rotate");
        let mut log = Rotating::new(&dir, "t", 100, 3);
        for i in 0..20 {
            log.write_all(format!("line {i:02} {}\n", "x".repeat(30)).as_bytes())
                .unwrap();
        }
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["t.1.log", "t.2.log", "t.log"]);
        for name in &names {
            assert!(std::fs::metadata(dir.join(name)).unwrap().len() <= 100);
        }
        // Newest last line in the current file.
        let current = std::fs::read_to_string(dir.join("t.log")).unwrap();
        assert!(current.contains("line 19"));
        // Reopening appends to the current file.
        drop(log);
        let mut log = Rotating::new(&dir, "t", 100, 3);
        assert!(log.written > 0);
        log.write_all(b"more\n").unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_crash_that_quit_is_shown_once_and_reports_are_capped() {
        let dir = temp("crash");
        save_crash(&dir, "background thread", false).unwrap();
        assert_eq!(take_last_crash_in(&dir), None);
        for i in 0..(KEEP_CRASHES + 3) {
            save_crash(&dir, &format!("report {i}"), true).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let crashes = std::fs::read_dir(&dir)
            .unwrap()
            .filter(|e| {
                let name = e.as_ref().unwrap().file_name();
                name.to_string_lossy().starts_with("crash-")
            })
            .count();
        assert_eq!(crashes, KEEP_CRASHES);
        let last = format!("report {}", KEEP_CRASHES + 2);
        assert_eq!(take_last_crash_in(&dir).as_deref(), Some(last.as_str()));
        assert_eq!(take_last_crash_in(&dir), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reports_carry_the_build_and_place_without_private_text() {
        let message = "byte index 3 is not a char boundary; it is inside 'é' (bytes 2..4) of `My private lecture notes about the merger with Acme Corp, chapter 1`";
        let report = Panic {
            message,
            location: "crates/app/src/ui/chat.rs:12:5",
            thread: "main",
            screen: "/chat/c1",
            backtrace: "0: margin_app::ui::chat::f",
            time: "2026-10-15T00:00:00Z",
        }
        .report();
        assert!(report.contains(&format!("{VERSION} (build {BUILD})")));
        assert!(report.contains("Thread: main"));
        assert!(report.contains("Screen: /chat/c1"));
        assert!(report.contains("crates/app/src/ui/chat.rs:12:5"));
        assert!(report.contains("Backtrace:\n0: margin_app"));
        assert!(report.contains("is not a char boundary"));
        assert!(!report.contains("merger"));
        assert!(!BUILD.is_empty());
    }

    /// The real hook, on a real panic. It stays installed for the rest of the test run,
    /// writing into this temporary folder, which is harmless.
    #[test]
    fn a_panic_on_the_main_thread_leaves_a_report_for_the_next_launch() {
        let dir = temp("hook");
        install_panic_hook(Some(dir.clone()), false);
        set_screen("/notes");
        let _ = std::thread::Builder::new()
            .name("main".into())
            .spawn(|| panic!("boom with sk-live0123456789"))
            .unwrap()
            .join();
        let report = take_last_crash_in(&dir).expect("a report");
        assert!(report.contains("boom with [secret removed]"), "{report}");
        assert!(report.contains("Thread: main"));
        assert!(report.contains("Screen: /notes"));
        assert!(report.contains("src/diagnostics.rs:"));
        assert!(report.contains("Backtrace:\n"));
        // A background thread's panic is kept, but didn't quit the app.
        let _ = std::thread::Builder::new()
            .name("worker".into())
            .spawn(|| panic!("background"))
            .unwrap()
            .join();
        assert_eq!(take_last_crash_in(&dir), None);
        let crashes = std::fs::read_dir(&dir).unwrap().count();
        assert!(crashes >= 2, "{crashes}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scrub_keeps_code_and_drops_secrets() {
        assert_eq!(
            scrub("called `Option::unwrap()` on a `None` value"),
            "called `Option::unwrap()` on a `None` value"
        );
        let s = scrub("bad key sk-abc123 and Bearer mgn_0123456789 and token=hunter2 ok");
        assert!(!s.contains("sk-abc123"));
        assert!(!s.contains("mgn_0123456789"));
        assert!(!s.contains("hunter2"));
        assert!(s.contains("token=[secret removed]"));
        assert!(s.ends_with(" ok"));
        let s = scrub("id a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8 end");
        assert!(s.contains("[secret removed]"));
        assert!(scrub(&"é".repeat(5000)).chars().count() <= 2001);
    }
}
