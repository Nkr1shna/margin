//! Adding files and folders as sources: which files qualify, the limits on one add, the
//! upload queue's records, and how sizes and statuses read. Pure data: no Dioxus here,
//! so it is unit-tested directly.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use margin_shared::{Id, Source, SourceKind, SourceStatus, routes};

/// At most this many files in one add (a pick of files, or a folder).
pub const MAX_FILES_PER_ADD: usize = 300;
/// At most this many bytes in one add.
pub const MAX_BYTES_PER_ADD: u64 = 200 * 1024 * 1024;

/// Folders a walk never enters (besides hidden ones): version control, dependencies,
/// build output and caches.
const SKIPPED_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "build",
    "dist",
    "venv",
    "__pycache__",
];

/// A file chosen to upload.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub path: PathBuf,
    /// The file name, or its path inside the added folder with `/` separators.
    pub name: String,
    /// The added folder's name.
    pub folder: Option<String>,
    pub kind: SourceKind,
    pub bytes: u64,
}

/// The files one add uploads, and how many it left out and why.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Selection {
    pub files: Vec<Candidate>,
    /// Files Margin can't read (images, archives, binaries…).
    pub unsupported: usize,
    /// Files over `routes::MAX_SOURCE_BYTES`.
    pub too_large: usize,
    /// Files past `MAX_FILES_PER_ADD` or `MAX_BYTES_PER_ADD`.
    pub over_limit: usize,
    /// Files whose size couldn't be read.
    pub unreadable: usize,
    /// The folder's name, for a folder add.
    pub folder: Option<String>,
}

impl Selection {
    fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.bytes).sum()
    }

    /// Add `path` (known as `name`) if it's a source and fits the limits.
    fn offer(&mut self, path: PathBuf, name: String) {
        let Some(kind) = SourceKind::for_name(&name) else {
            self.unsupported += 1;
            return;
        };
        let bytes = match std::fs::metadata(&path) {
            Ok(meta) => meta.len(),
            Err(_) => {
                self.unreadable += 1;
                return;
            }
        };
        if bytes > routes::MAX_SOURCE_BYTES {
            self.too_large += 1;
        } else if self.files.len() >= MAX_FILES_PER_ADD
            || self.total_bytes() + bytes > MAX_BYTES_PER_ADD
        {
            self.over_limit += 1;
        } else {
            self.files.push(Candidate {
                path,
                name,
                folder: self.folder.clone(),
                kind,
                bytes,
            });
        }
    }

    /// What was left out, as a sentence; `None` when nothing was.
    pub fn summary(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.unsupported > 0 {
            parts.push(format!(
                "{} Margin can't read",
                files(self.unsupported)
            ));
        }
        if self.too_large > 0 {
            parts.push(format!(
                "{} over {} MB",
                files(self.too_large),
                routes::MAX_SOURCE_BYTES / (1024 * 1024)
            ));
        }
        if self.over_limit > 0 {
            parts.push(format!(
                "{} past the limit of {MAX_FILES_PER_ADD} files or {} MB per add",
                files(self.over_limit),
                MAX_BYTES_PER_ADD / (1024 * 1024)
            ));
        }
        if self.unreadable > 0 {
            parts.push(format!("{} that couldn't be opened", files(self.unreadable)));
        }
        if parts.is_empty() {
            return None;
        }
        let added = match self.files.len() {
            0 => "Added nothing".to_string(),
            n => format!("Added {}", files(n)),
        };
        Some(format!("{added}. Skipped {}.", join_and(&parts)))
    }
}

fn files(n: usize) -> String {
    if n == 1 {
        "1 file".into()
    } else {
        format!("{n} files")
    }
}

/// "a", "a and b", "a, b and c".
fn join_and(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Files picked one by one: each is named by its file name.
pub fn select_files(paths: Vec<PathBuf>) -> Selection {
    let mut selection = Selection::default();
    for path in paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        selection.offer(path, name);
    }
    selection
}

/// Every source file under `root`, named by its path inside it. Hidden entries, symbolic
/// links and `SKIPPED_DIRS` are passed over; files are visited in name order, so the
/// limits keep the same files every time.
pub fn select_folder(root: &Path) -> Selection {
    let mut selection = Selection {
        folder: Some(
            root.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| root.display().to_string()),
        ),
        ..Selection::default()
    };
    walk(root, "", &mut selection);
    selection
}

fn walk(dir: &Path, prefix: &str, selection: &mut Selection) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = read.filter_map(Result::ok).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            selection.unreadable += 1;
            continue;
        };
        let rel = format!("{prefix}{name}");
        if file_type.is_dir() {
            if !SKIPPED_DIRS.contains(&name.as_str()) {
                walk(&entry.path(), &format!("{rel}/"), selection);
            }
        } else if file_type.is_file() {
            selection.offer(entry.path(), rel);
        }
    }
}

static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

/// A file on its way to the service, or one uploaded from the Ask composer.
#[derive(Debug, Clone, PartialEq)]
pub struct Upload {
    /// Local identity, unique in this run of the app.
    pub key: u64,
    /// The topic it's added to; `None` for the Ask composer.
    pub topic_id: Option<Id>,
    pub path: PathBuf,
    pub name: String,
    pub folder: Option<String>,
    pub kind: SourceKind,
    pub bytes: u64,
    pub state: UploadState,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UploadState {
    Waiting,
    Uploading,
    /// Uploaded; the service's record at upload time (the store has the live one).
    Done(Source),
    Failed(String),
}

impl Upload {
    pub fn new(candidate: Candidate, topic_id: Option<Id>) -> Self {
        Self {
            key: NEXT_KEY.fetch_add(1, Ordering::Relaxed),
            topic_id,
            path: candidate.path,
            name: candidate.name,
            folder: candidate.folder,
            kind: candidate.kind,
            bytes: candidate.bytes,
            state: UploadState::Waiting,
        }
    }

    /// Still waiting for, or in the middle of, its upload.
    pub fn pending(&self) -> bool {
        matches!(self.state, UploadState::Waiting | UploadState::Uploading)
    }
}

/// How a source's status reads, and the dot that goes with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Waiting,
    Working,
    Ready,
    Failed,
}

impl Tone {
    pub fn dot_class(self) -> &'static str {
        match self {
            Tone::Waiting => "dot",
            Tone::Working => "dot run",
            Tone::Ready => "dot ok",
            Tone::Failed => "dot bad",
        }
    }
}

pub fn status_text(status: &SourceStatus) -> (Tone, String) {
    match status {
        SourceStatus::Queued => (Tone::Waiting, "Queued".into()),
        SourceStatus::Processing { done, total } => {
            (Tone::Working, format!("Processing {done}/{total}"))
        }
        SourceStatus::Ready {
            pages: Some(pages), ..
        } => (
            Tone::Ready,
            format!("Ready · {pages} page{}", if *pages == 1 { "" } else { "s" }),
        ),
        SourceStatus::Ready { .. } => (Tone::Ready, "Ready".into()),
        SourceStatus::Failed { error } => (Tone::Failed, format!("Failed: {error}")),
    }
}

/// "812 bytes", "5.1 KB", "2.3 MB", "640 MB", "1.2 GB" (1 KB = 1024 bytes, like the
/// upload limits).
pub fn human_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = "KB";
    for next in ["MB", "GB", "TB"] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    if value < 10.0 {
        format!("{value:.1} {unit}")
    } else {
        format!("{value:.0} {unit}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch folder removed when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "margin-sources-{tag}-{}-{}",
                std::process::id(),
                NEXT_KEY.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self, rel: &str, bytes: usize) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, vec![b'a'; bytes]).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn folder_walk_skips_hidden_and_build_folders() {
        let dir = TempDir::new("walk");
        dir.file("README", 10);
        dir.file("notes/ch1.md", 10);
        dir.file("notes/ch2.md", 10);
        dir.file("src/main.rs", 10);
        dir.file("src/logo.png", 10);
        dir.file(".hidden.md", 10);
        dir.file(".git/config.toml", 10);
        dir.file(".venv/lib.py", 10);
        dir.file("venv/lib.py", 10);
        dir.file("node_modules/x/index.js", 10);
        dir.file("target/debug/out.rs", 10);
        dir.file("build/out.txt", 10);
        dir.file("dist/app.js", 10);
        dir.file("pkg/__pycache__/m.py", 10);
        dir.file("Cargo.lock", 10);
        let selection = select_folder(&dir.0);
        let names: Vec<_> = selection.files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["README", "notes/ch1.md", "notes/ch2.md", "src/main.rs"]);
        let folder = dir.0.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            selection
                .files
                .iter()
                .all(|f| f.folder.as_deref() == Some(folder.as_str()))
        );
        assert_eq!(selection.files[3].kind, SourceKind::Code);
        assert_eq!(selection.unsupported, 2);
        assert_eq!(
            selection.summary().as_deref(),
            Some("Added 4 files. Skipped 2 files Margin can't read.")
        );
    }

    #[test]
    fn limits_cap_files_and_bytes_per_add() {
        let dir = TempDir::new("limits");
        for i in 0..MAX_FILES_PER_ADD + 5 {
            dir.file(&format!("n{i:04}.txt"), 1);
        }
        let selection = select_folder(&dir.0);
        assert_eq!(selection.files.len(), MAX_FILES_PER_ADD);
        assert_eq!(selection.files[0].name, "n0000.txt");
        assert_eq!(selection.over_limit, 5);
        assert_eq!(
            selection.summary().as_deref(),
            Some("Added 300 files. Skipped 5 files past the limit of 300 files or 200 MB per add.")
        );

        // Bytes: four 49 MB files fit 200 MB, the fifth doesn't; a 51 MB file is too large.
        let big = TempDir::new("bytes");
        let mb = 1024 * 1024;
        let paths: Vec<_> = (0..5)
            .map(|i| big.file(&format!("b{i}.txt"), 49 * mb))
            .chain([big.file("huge.txt", 51 * mb), big.file("song.mp3", 1)])
            .collect();
        let selection = select_files(paths);
        assert_eq!(selection.files.len(), 4);
        assert_eq!(selection.files[0].name, "b0.txt");
        assert_eq!(selection.files[0].folder, None);
        assert_eq!(selection.over_limit, 1);
        assert_eq!(selection.too_large, 1);
        assert_eq!(selection.unsupported, 1);
        assert_eq!(
            selection.summary().as_deref(),
            Some(
                "Added 4 files. Skipped 1 file Margin can't read, 1 file over 50 MB and \
                 1 file past the limit of 300 files or 200 MB per add."
            )
        );
    }

    #[test]
    fn nothing_skipped_has_no_summary() {
        let dir = TempDir::new("clean");
        let selection = select_files(vec![dir.file("a.md", 3)]);
        assert_eq!(selection.files.len(), 1);
        assert_eq!(selection.summary(), None);
        let missing = select_files(vec![dir.0.join("gone.md")]);
        assert_eq!(
            missing.summary().as_deref(),
            Some("Added nothing. Skipped 1 file that couldn't be opened.")
        );
    }

    #[test]
    fn statuses_and_sizes_read_well() {
        assert_eq!(
            status_text(&SourceStatus::Processing { done: 2, total: 9 }),
            (Tone::Working, "Processing 2/9".into())
        );
        assert_eq!(
            status_text(&SourceStatus::Ready {
                chunks: 4,
                pages: Some(1)
            })
            .1,
            "Ready · 1 page"
        );
        assert_eq!(
            status_text(&SourceStatus::Failed { error: "Bad".into() }),
            (Tone::Failed, "Failed: Bad".into())
        );
        assert_eq!(human_bytes(812), "812 bytes");
        assert_eq!(human_bytes(5_200), "5.1 KB");
        assert_eq!(human_bytes(2_400_000), "2.3 MB");
        assert_eq!(human_bytes(640_000_000), "610 MB");
        assert_eq!(human_bytes(50 * 1024 * 1024), "50 MB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }
}
