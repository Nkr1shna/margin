//! Adding files as sources: which files qualify, the limits on one add, the upload
//! queue's records, and how sizes and statuses read. (Folders aren't uploaded as a pick;
//! they are attached and synced, `crate::folders`.) Pure data: no Dioxus here,
//! so it is unit-tested directly.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use margin_shared::{Id, Source, SourceKind, SourceStatus, routes};

/// At most this many files in one add (a pick of files, or a folder).
pub const MAX_FILES_PER_ADD: usize = 300;
/// At most this many bytes in one add: room for four recordings of the largest size a
/// source may be (`routes::MAX_MEDIA_SOURCE_BYTES`, 500 MB), or a course folder of
/// slides, notes and a few lectures, while a folder picked by mistake (a whole drive,
/// a photo library) stops well short of filling the service Mac's disk. Any one file
/// a source may be always fits, so the cap never stands in the way of a single file.
pub const MAX_BYTES_PER_ADD: u64 = 2 * 1024 * 1024 * 1024;

/// A file chosen to upload. `P` is how the file is reached: a local path, or (while a
/// mobile picker works out what to copy) the platform's handle for it.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate<P = PathBuf> {
    pub path: P,
    /// The file name, or its path inside the added folder with `/` separators.
    pub name: String,
    /// The added folder's name.
    pub folder: Option<String>,
    pub kind: SourceKind,
    pub bytes: u64,
}

/// The files one add uploads, and how many it left out and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection<P = PathBuf> {
    pub files: Vec<Candidate<P>>,
    /// Files Margin can't read (archives, binaries, lock files…).
    pub unsupported: usize,
    /// Files over their kind's limit (`routes::max_source_bytes`).
    pub too_large: usize,
    /// Files past `MAX_FILES_PER_ADD` or `MAX_BYTES_PER_ADD`.
    pub over_limit: usize,
    /// Files whose size couldn't be read, or that couldn't be copied.
    pub unreadable: usize,
    /// The folder's name, for a folder add.
    pub folder: Option<String>,
}

impl<P> Default for Selection<P> {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            unsupported: 0,
            too_large: 0,
            over_limit: 0,
            unreadable: 0,
            folder: None,
        }
    }
}

impl<P> Selection<P> {
    fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.bytes).sum()
    }

    /// Add `path` (known as `name`) if it's a source and fits the limits. `size` reads
    /// its size, and is only asked for files Margin can read.
    fn offer(&mut self, path: P, name: String, size: impl FnOnce(&P) -> Option<u64>) {
        let Some(kind) = SourceKind::for_name(&name) else {
            self.unsupported += 1;
            return;
        };
        let Some(bytes) = size(&path) else {
            self.unreadable += 1;
            return;
        };
        if bytes > routes::max_source_bytes(kind) {
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

    /// Copy each chosen file into `dir` with `copy(from, to)` so it can be read from there
    /// until it's uploaded: a mobile picker's files are only reachable while its grant
    /// lasts. A file that fails to copy counts as unreadable.
    #[cfg_attr(not(any(target_os = "ios", target_os = "android")), allow(dead_code))]
    pub fn stage(
        self,
        dir: &Path,
        mut copy: impl FnMut(&P, &Path) -> Result<(), String>,
    ) -> Selection<PathBuf> {
        let mut staged = Selection {
            files: Vec::with_capacity(self.files.len()),
            unsupported: self.unsupported,
            too_large: self.too_large,
            over_limit: self.over_limit,
            unreadable: self.unreadable,
            folder: self.folder,
        };
        for (i, file) in self.files.into_iter().enumerate() {
            let to = dir.join(staged_name(i, &file.name));
            match copy(&file.path, &to) {
                Ok(()) => staged.files.push(Candidate {
                    path: to,
                    name: file.name,
                    folder: file.folder,
                    kind: file.kind,
                    bytes: file.bytes,
                }),
                Err(error) => {
                    eprintln!("couldn't copy {}: {error}", file.name);
                    let _ = std::fs::remove_file(&to);
                    staged.unreadable += 1;
                }
            }
        }
        staged
    }

    /// What was left out, as a sentence; `None` when nothing was.
    pub fn summary(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.unsupported > 0 {
            parts.push(format!("{} Margin can't read", files(self.unsupported)));
        }
        if self.too_large > 0 {
            parts.push(format!(
                "{} over the size limit ({} for audio and video, {} for other files)",
                files(self.too_large),
                human_bytes(routes::MAX_MEDIA_SOURCE_BYTES),
                human_bytes(routes::MAX_OTHER_SOURCE_BYTES),
            ));
        }
        if self.over_limit > 0 {
            parts.push(format!(
                "{} past the limit of {MAX_FILES_PER_ADD} files or {} per add",
                files(self.over_limit),
                human_bytes(MAX_BYTES_PER_ADD)
            ));
        }
        if self.unreadable > 0 {
            parts.push(format!(
                "{} that couldn't be opened",
                files(self.unreadable)
            ));
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

/// A staged copy's file name: its place in the add (names inside a folder repeat once
/// they're flattened) and the original file name, which keeps its extension.
#[cfg_attr(not(any(target_os = "ios", target_os = "android")), allow(dead_code))]
fn staged_name(index: usize, name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    let safe: String = base
        .chars()
        .map(|c| if c == '\\' || c.is_control() { '_' } else { c })
        .collect();
    format!("{index:04}-{safe}")
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
// Android picks documents through the Storage Access Framework, not local paths.
#[cfg_attr(target_os = "android", allow(dead_code))]
pub fn select_files(paths: Vec<PathBuf>) -> Selection {
    let named = paths
        .into_iter()
        .map(|path| {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            (path, name)
        })
        .collect();
    select_named(named, |path| local_size(path))
}

/// Files picked one by one, each with its name; `size` reads a file's size.
pub fn select_named<P>(files: Vec<(P, String)>, size: impl Fn(&P) -> Option<u64>) -> Selection<P> {
    let mut selection = Selection::default();
    for (path, name) in files {
        selection.offer(path, name, &size);
    }
    selection
}

// Android picks documents through the Storage Access Framework, not local paths.
#[cfg_attr(target_os = "android", allow(dead_code))]
fn local_size(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|meta| meta.len())
}

/// What an entry in a folder is. (The walk machinery is kept for the folder sync,
/// 4-core; nothing walks a folder until then.)
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Folder,
    File,
    /// Symbolic links and other special files: passed over.
    Other,
    /// Its type couldn't be read: counted as unreadable.
    Unknown,
}

/// An entry in a folder being walked.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry<N> {
    pub name: String,
    pub node: N,
    pub kind: EntryKind,
}

/// Folders a walk can list (a held folder, through `crate::folders::Dir`).
pub trait Tree {
    /// A folder or file in the tree.
    type Node;
    /// The entries of the folder `dir`, in any order; `None` when it can't be listed.
    fn entries(&self, dir: &Self::Node) -> Option<Vec<Entry<Self::Node>>>;
    /// A file's size in bytes; `None` when it can't be read.
    fn size(&self, file: &Self::Node) -> Option<u64>;
}

/// Every source file under `root` in `tree`, which the user knows as `name`: hidden
/// entries are passed over, folders are walked whatever their name, files are visited in
/// name order so the limits keep the same files every time. Kept for the folder sync
/// (4-core), which queues a folder's uploads through `Selection` and `Upload`.
#[allow(dead_code)]
pub fn select_tree<T: Tree>(tree: &T, root: &T::Node, name: String) -> Selection<T::Node> {
    let mut selection = Selection {
        folder: Some(name),
        ..Selection::default()
    };
    walk(tree, root, "", &mut selection);
    selection
}

fn walk<T: Tree>(tree: &T, dir: &T::Node, prefix: &str, selection: &mut Selection<T::Node>) {
    let Some(mut entries) = tree.entries(dir) else {
        return;
    };
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    for entry in entries {
        if entry.name.starts_with('.') {
            continue;
        }
        let rel = format!("{prefix}{}", entry.name);
        match entry.kind {
            EntryKind::Folder => walk(tree, &entry.node, &format!("{rel}/"), selection),
            EntryKind::File => selection.offer(entry.node, rel, |node| tree.size(node)),
            EntryKind::Other => {}
            EntryKind::Unknown => selection.unreadable += 1,
        }
    }
}

static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

/// Where added files go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// A topic's sources (its Sources dialog).
    Topic(Id),
    /// The files a chat's composer sends with its next message: that chat's composer, or
    /// a new chat's (`None`). They belong to no topic; the message attaches them to the
    /// chat.
    Chat(Option<Id>),
}

/// A file on its way to the service.
#[derive(Debug, Clone, PartialEq)]
pub struct Upload {
    /// Local identity, unique in this run of the app.
    pub key: u64,
    /// The topic it's added to; `None` for a file added in a chat's composer.
    pub topic_id: Option<Id>,
    /// For a file added in a chat's composer: the chat (`None` for a new chat's composer,
    /// and for a topic's file).
    pub chat_id: Option<Id>,
    pub path: PathBuf,
    pub name: String,
    pub folder: Option<String>,
    pub kind: SourceKind,
    pub bytes: u64,
    /// How much of a file sent in parts the service has so far.
    pub sent: u64,
    pub state: UploadState,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UploadState {
    Waiting,
    Uploading,
    /// Uploaded; the service's record at upload time (the store has the live one).
    Done(Box<Source>),
    Failed(String),
}

impl Upload {
    /// A file for topic `topic_id`, or (`None`) for a new chat's composer.
    pub fn new(candidate: Candidate, topic_id: Option<Id>) -> Self {
        Self::to(
            candidate,
            topic_id.map_or(Destination::Chat(None), Destination::Topic),
        )
    }

    pub fn to(candidate: Candidate, to: Destination) -> Self {
        let (topic_id, chat_id) = match to {
            Destination::Topic(id) => (Some(id), None),
            Destination::Chat(chat) => (None, chat),
        };
        Self {
            key: NEXT_KEY.fetch_add(1, Ordering::Relaxed),
            topic_id,
            chat_id,
            path: candidate.path,
            name: candidate.name,
            folder: candidate.folder,
            kind: candidate.kind,
            bytes: candidate.bytes,
            sent: 0,
            state: UploadState::Waiting,
        }
    }

    /// Whether it was added in the composer of chat `chat_id` (`None`: a new chat's).
    pub fn in_composer(&self, chat_id: Option<&str>) -> bool {
        self.topic_id.is_none() && self.chat_id.as_deref() == chat_id
    }

    /// The source it became, once uploaded.
    pub fn source_id(&self) -> Option<&Id> {
        match &self.state {
            UploadState::Done(source) => Some(&source.id),
            _ => None,
        }
    }

    /// Still waiting for, or in the middle of, its upload.
    pub fn pending(&self) -> bool {
        matches!(self.state, UploadState::Waiting | UploadState::Uploading)
    }

    /// "Uploading…", with how far along a file sent in parts is.
    pub fn uploading_text(&self) -> String {
        if self.bytes > 0 && self.sent > 0 && self.sent < self.bytes {
            format!("Uploading… {}%", self.sent * 100 / self.bytes)
        } else {
            "Uploading…".into()
        }
    }
}

/// How a source's status reads, and the dot that goes with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Waiting,
    Working,
    Ready,
    /// Ready, but not all of it was read (`SourceStatus::Ready::warning`).
    Warning,
    Failed,
}

impl Tone {
    pub fn dot_class(self) -> &'static str {
        match self {
            Tone::Waiting => "dot",
            Tone::Working => "dot run",
            Tone::Ready => "dot ok",
            Tone::Warning => "dot warn",
            Tone::Failed => "dot bad",
        }
    }

    /// The class of a source's size-and-status line (long messages wrap).
    pub fn meta_class(self) -> &'static str {
        match self {
            Tone::Failed => "src-meta bad",
            Tone::Warning => "src-meta warn",
            _ => "src-meta",
        }
    }
}

pub fn status_text(status: &SourceStatus) -> (Tone, String) {
    match status {
        SourceStatus::Queued => (Tone::Waiting, "Queued".into()),
        SourceStatus::Processing { done, total } => {
            (Tone::Working, format!("Processing {done}/{total}"))
        }
        SourceStatus::Ready { pages, warning, .. } => {
            let mut text = match pages {
                Some(n) => format!("Ready · {n} page{}", if *n == 1 { "" } else { "s" }),
                None => "Ready".into(),
            };
            match warning {
                Some(w) => {
                    text.push_str(" · ");
                    text.push_str(w);
                    (Tone::Warning, text)
                }
                None => (Tone::Ready, text),
            }
        }
        SourceStatus::Failed { error } => (Tone::Failed, format!("Failed: {error}")),
    }
}

/// What can be added as a source, and how big it can be: said by the composer's
/// Sources menu and a topic's Sources dialog alike.
pub fn kinds_text() -> String {
    format!(
        "Add {} files. {}",
        margin_shared::SOURCE_KINDS,
        routes::source_limits_text()
    )
}

/// What can be attached to a chat (files only: a folder is attached as a workspace, read
/// in place), and how big it can be: said by the chat composer's Files menu.
pub fn chat_files_text() -> String {
    format!(
        "Attach {} files for the answer to draw on. {}",
        margin_shared::SOURCE_KINDS,
        routes::source_limits_text()
    )
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
    fn limits_cap_files_and_bytes_per_add() {
        let many: Vec<(String, String)> = (0..MAX_FILES_PER_ADD + 5)
            .map(|i| ("1".to_string(), format!("n{i:04}.txt")))
            .collect();
        let selection = select_named(many, |size: &String| size.parse().ok());
        assert_eq!(selection.files.len(), MAX_FILES_PER_ADD);
        assert_eq!(selection.files[0].name, "n0000.txt");
        assert_eq!(selection.over_limit, 5);
        assert_eq!(
            selection.summary().as_deref(),
            Some("Added 300 files. Skipped 5 files past the limit of 300 files or 2.0 GB per add.")
        );

        // Sizes, read without writing gigabytes: per kind, 50 MB or 500 MB; per add, 2 GB.
        let mb: u64 = 1024 * 1024;
        let picked: Vec<(String, String)> = [
            ("notes.txt", 50 * mb),
            ("huge.txt", 50 * mb + 1),
            ("scan.pdf", 51 * mb),
            ("lecture1.mp4", 500 * mb),
            ("lecture2.mov", 501 * mb),
            ("talk1.m4a", 499 * mb),
            ("talk2.m4a", 480 * mb),
            ("talk3.m4a", 450 * mb),
            ("talk4.m4a", 100 * mb),
            ("bundle.zip", 1),
        ]
        .iter()
        .map(|(name, bytes)| (bytes.to_string(), name.to_string()))
        .collect();
        let selection = select_named(picked, |size: &String| size.parse().ok());
        let names: Vec<&str> = selection.files.iter().map(|f| f.name.as_str()).collect();
        // 50 + 500 + 499 + 480 + 450 MB fit in 2 GB; another 100 MB doesn't.
        assert_eq!(
            names,
            vec![
                "notes.txt",
                "lecture1.mp4",
                "talk1.m4a",
                "talk2.m4a",
                "talk3.m4a"
            ]
        );
        assert_eq!(selection.files[0].folder, None);
        assert_eq!(selection.too_large, 3);
        assert_eq!(selection.over_limit, 1);
        assert_eq!(selection.unsupported, 1);
        assert_eq!(
            selection.summary().as_deref(),
            Some(
                "Added 5 files. Skipped 1 file Margin can't read, 3 files over the size limit \
                 (500 MB for audio and video, 50 MB for other files) and 1 file past the \
                 limit of 300 files or 2.0 GB per add."
            )
        );
        // The per-add cap never stops a single file a source may be.
        const { assert!(routes::MAX_MEDIA_SOURCE_BYTES <= MAX_BYTES_PER_ADD) };
        let one = select_named(
            vec![((500 * mb).to_string(), "film.mov".to_string())],
            |size: &String| size.parse().ok(),
        );
        assert_eq!(one.files.len(), 1);
    }

    /// A document tree like Android's: nodes are document ids, sizes come with the
    /// listing, and some can't be read.
    struct Docs(Vec<(&'static str, &'static str, EntryKind, Option<u64>)>);

    impl Tree for Docs {
        type Node = String;

        fn entries(&self, dir: &String) -> Option<Vec<Entry<String>>> {
            if dir == "locked" {
                return None;
            }
            Some(
                self.0
                    .iter()
                    .filter(|(parent, ..)| parent == dir)
                    .map(|(parent, name, kind, _)| Entry {
                        name: name.to_string(),
                        node: format!("{parent}/{name}"),
                        kind: *kind,
                    })
                    .collect(),
            )
        }

        fn size(&self, file: &String) -> Option<u64> {
            self.0
                .iter()
                .find(|(parent, name, ..)| format!("{parent}/{name}") == *file)
                .and_then(|(.., size)| *size)
        }
    }

    #[test]
    fn document_trees_walk_like_folders() {
        use EntryKind::*;
        let tree = Docs(vec![
            ("root", "zeta.md", File, Some(5)),
            ("root", "Alpha", Folder, None),
            ("root", ".trash", Folder, None),
            ("root", "node_modules", Folder, None),
            ("root", "locked", Folder, None),
            ("root", "link", Other, None),
            ("root", "mystery", Unknown, None),
            ("root", "photo.png", File, Some(7)),
            ("root", "gone.txt", File, None),
            ("root", "song.zip", File, Some(1)),
            ("root/Alpha", "b.rs", File, Some(3)),
            ("root/Alpha", "a.pdf", File, Some(2)),
            ("root/.trash", "old.md", File, Some(1)),
            ("root/node_modules", "x.js", File, Some(1)),
        ]);
        let selection = select_tree(&tree, &"root".to_string(), "Notes".into());
        let names: Vec<_> = selection.files.iter().map(|f| f.name.as_str()).collect();
        // Hidden folders are skipped; others are walked whatever their name.
        assert_eq!(
            names,
            vec![
                "Alpha/a.pdf",
                "Alpha/b.rs",
                "node_modules/x.js",
                "photo.png",
                "zeta.md"
            ]
        );
        assert_eq!(selection.files[0].path, "root/Alpha/a.pdf");
        assert_eq!(selection.files[0].kind, SourceKind::Pdf);
        assert_eq!(selection.files[3].kind, SourceKind::Image);
        assert_eq!(selection.files[4].bytes, 5);
        assert!(
            selection
                .files
                .iter()
                .all(|f| f.folder.as_deref() == Some("Notes"))
        );
        // `mystery` (type unknown) and `gone.txt` (no size) couldn't be read.
        assert_eq!(selection.unreadable, 2);
        assert_eq!(selection.unsupported, 1);
    }

    #[test]
    fn named_picks_use_their_names_not_their_handles() {
        let picked = vec![
            ("content://docs/1".to_string(), "Lecture.mp3".to_string()),
            ("content://docs/2".to_string(), "slides.key".to_string()),
        ];
        let selection = select_named(picked, |_| Some(10));
        assert_eq!(selection.files.len(), 1);
        assert_eq!(selection.files[0].name, "Lecture.mp3");
        assert_eq!(selection.files[0].path, "content://docs/1");
        assert_eq!(selection.files[0].kind, SourceKind::Audio);
        assert_eq!(selection.unsupported, 1);
    }

    #[test]
    fn staging_copies_chosen_files_and_counts_failures() {
        let dir = TempDir::new("stage");
        let picked = vec![
            ("one".to_string(), "Notes/ch1.md".to_string()),
            ("two".to_string(), "Notes/ch1.md".to_string()),
            ("bad".to_string(), "broken.txt".to_string()),
        ];
        let mut selection = select_named(picked, |_| Some(3));
        selection.folder = Some("Notes".into());
        let staged = selection.stage(&dir.0, |from, to| {
            if from == "bad" {
                std::fs::write(to, "partial").unwrap();
                return Err("gone".into());
            }
            std::fs::write(to, from).map_err(|e| e.to_string())
        });
        assert_eq!(staged.files.len(), 2);
        assert_eq!(staged.unreadable, 1);
        assert_eq!(staged.folder.as_deref(), Some("Notes"));
        // Same names inside a folder still get separate copies.
        assert_eq!(staged.files[0].path, dir.0.join("0000-ch1.md"));
        assert_eq!(staged.files[1].path, dir.0.join("0001-ch1.md"));
        assert_eq!(staged.files[0].name, "Notes/ch1.md");
        assert_eq!(
            std::fs::read_to_string(&staged.files[1].path).unwrap(),
            "two"
        );
        // A failed copy leaves nothing behind.
        assert!(!dir.0.join("0002-broken.txt").exists());
        assert_eq!(
            staged.summary().as_deref(),
            Some("Added 2 files. Skipped 1 file that couldn't be opened.")
        );
        assert_eq!(staged_name(7, "a\\b\n.md"), "0007-a_b_.md");
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
    fn the_copy_lists_every_kind_and_the_limits() {
        assert_eq!(
            kinds_text(),
            "Add text, code, PDFs, images, audio and video files. \
             Audio and video files can be up to 500 MB each; other files up to 50 MB."
        );
        assert_eq!(
            chat_files_text(),
            "Attach text, code, PDFs, images, audio and video files for the answer to draw \
             on. Audio and video files can be up to 500 MB each; other files up to 50 MB."
        );
    }

    #[test]
    fn uploads_know_whose_composer_they_are_in() {
        let candidate = || Candidate {
            path: PathBuf::from("/tmp/a.md"),
            name: "a.md".into(),
            folder: None,
            kind: SourceKind::Text,
            bytes: 3,
        };
        let topic = Upload::new(candidate(), Some("t1".into()));
        assert_eq!(topic.topic_id.as_deref(), Some("t1"));
        assert!(!topic.in_composer(None));
        let new_chat = Upload::new(candidate(), None);
        assert!(new_chat.in_composer(None));
        assert!(!new_chat.in_composer(Some("c1")));
        let chat = Upload::to(candidate(), Destination::Chat(Some("c1".into())));
        assert!(chat.in_composer(Some("c1")));
        assert!(!chat.in_composer(None));
        assert_ne!(chat.key, new_chat.key);
        assert_eq!(chat.source_id(), None);
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
                pages: Some(1),
                warning: None,
            })
            .1,
            "Ready · 1 page"
        );
        assert_eq!(
            status_text(&SourceStatus::Ready {
                chunks: 4,
                pages: None,
                warning: Some("It wasn't transcribed.".into()),
            }),
            (Tone::Warning, "Ready · It wasn't transcribed.".into())
        );
        assert_eq!(Tone::Warning.dot_class(), "dot warn");
        assert_eq!(
            status_text(&SourceStatus::Failed {
                error: "Bad".into()
            }),
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
