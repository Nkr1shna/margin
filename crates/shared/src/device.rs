//! Frames addressed to one device over its `/api/events` socket, and the device's
//! answers. These are never broadcast: the service sends a `DirectedFrame` only to the
//! device a tool call runs on, and only that device sends `DeviceFrame`s back.
//!
//! The socket carries JSON text frames both ways. Service → device: an [`Event`] (to
//! every device) or a [`DirectedFrame`]; parse with [`ServerFrame`]. Device → service:
//! [`DeviceFrame`]. The `type` tags of `Event` and `DirectedFrame` never overlap.
//!
//! [`Event`]: crate::Event

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{Event, FileStamp, Id, ManifestFile, SourceKind};

/// Anything the service sends on the events socket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
#[allow(clippy::large_enum_variant)]
pub enum ServerFrame {
    Event(Event),
    Directed(DirectedFrame),
}

/// Service → one device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DirectedFrame {
    /// Run a device tool and answer with `DeviceFrame::ToolResult` before `deadline`.
    ToolRequest { request: ToolRequest },
    /// Stop working on a request (the turn was stopped, or the deadline passed). No
    /// answer is expected; one that arrives anyway is ignored.
    ToolCancel { request_id: Id },
}

/// Device → service.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeviceFrame {
    ToolResult {
        request_id: Id,
        result: DeviceToolResult,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolRequest {
    pub request_id: Id,
    pub chat_id: Id,
    /// The model's `call_id` (matches the chat's `ToolCall` item).
    pub call_id: String,
    pub deadline: DateTime<Utc>,
    pub call: DeviceToolCall,
}

/// What a device is asked to do. Folder tools go to the device that registered the
/// workspace, which checks that the workspace is its own; app-control tools go to the
/// device whose message started the turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "tool", rename_all = "snake_case")]
pub enum DeviceToolCall {
    /// The entries of one folder (not recursive), skipping hidden and secret files.
    ListFiles {
        workspace_id: Id,
        /// Relative to the workspace root; "" is the root.
        path: String,
    },
    /// Files anywhere under `path` whose relative path matches `pattern` (a glob with
    /// `*`, `**` and `?`, case-insensitive).
    FindFiles {
        workspace_id: Id,
        path: String,
        pattern: String,
    },
    /// Lines containing `query` (case-insensitive, plain text) in text files under
    /// `path`, optionally only in files matching `pattern`.
    SearchText {
        workspace_id: Id,
        path: String,
        query: String,
        pattern: Option<String>,
    },
    /// Up to `max_lines` lines (at most `READ_FILE_MAX_LINES`) of a text file starting
    /// at `start_line` (1-based).
    ReadFile {
        workspace_id: Id,
        path: String,
        start_line: u32,
        max_lines: u32,
    },
    /// Send what changed in a folder this device holds: walk it, `POST` the manifest
    /// (`routes::workspace_sync`), answer `Done` with the plan's counts once the service
    /// has answered, and upload the plan's files in the background.
    SyncFolder { workspace_id: Id },
    /// Walk a folder this device holds and answer `DeviceToolOutput::Manifest` with every
    /// allow-listed, visible file and its stamp (the hash computed on the device).
    Manifest { workspace_id: Id },
    /// Upload one file of a folder this device holds to the service now, through
    /// `routes::SOURCES` / `routes::SOURCE_UPLOADS` with `workspace_id` and `modified`,
    /// and answer `Done` once it is received. `stamp` is what the service expects; a file
    /// whose stamp no longer matches is not sent (`Err`, the next sync catches it). No
    /// size limit (owner): large files go up in parts.
    FetchFile {
        workspace_id: Id,
        path: String,
        stamp: FileStamp,
    },
    /// Show a screen in the app.
    Navigate { to: NavTarget },
    /// Open a lesson's citation `n` in the source viewer.
    OpenCitation { lesson_id: Id, n: u32 },
    /// Open a source in the source viewer at a page or time.
    ShowPassage {
        source_id: Id,
        page: Option<u32>,
        start_ms: Option<u64>,
    },
    /// Show or hide the sidebar (`None` toggles).
    ToggleSidebar { visible: Option<bool> },
}

impl DeviceToolCall {
    /// The workspace a folder tool reads; `None` for app-control tools.
    pub fn workspace_id(&self) -> Option<&str> {
        match self {
            DeviceToolCall::ListFiles { workspace_id, .. }
            | DeviceToolCall::FindFiles { workspace_id, .. }
            | DeviceToolCall::SearchText { workspace_id, .. }
            | DeviceToolCall::ReadFile { workspace_id, .. }
            | DeviceToolCall::SyncFolder { workspace_id }
            | DeviceToolCall::Manifest { workspace_id }
            | DeviceToolCall::FetchFile { workspace_id, .. } => Some(workspace_id),
            DeviceToolCall::Navigate { .. }
            | DeviceToolCall::OpenCitation { .. }
            | DeviceToolCall::ShowPassage { .. }
            | DeviceToolCall::ToggleSidebar { .. } => None,
        }
    }
}

/// A screen the agent can show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "screen", rename_all = "snake_case")]
pub enum NavTarget {
    Lesson {
        lesson_id: Id,
        view: Option<LessonView>,
    },
    Topic {
        topic_id: Id,
    },
    Note {
        note_id: Id,
    },
    AllNotes,
    Chat {
        chat_id: Id,
    },
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LessonView {
    Read,
    Notes,
    Video,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DeviceToolResult {
    Ok {
        output: DeviceToolOutput,
    },
    /// The device couldn't do it; `message` is told to the model as is, e.g. "Margin no
    /// longer has access to this folder; attach it again on the iPhone."
    Err {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeviceToolOutput {
    /// `ListFiles` and `FindFiles`.
    Files {
        entries: Vec<FileEntry>,
        /// More entries existed than `MAX_LISTED_ENTRIES`.
        truncated: bool,
    },
    /// `SearchText`.
    Matches {
        matches: Vec<TextMatch>,
        /// The search stopped at `MAX_TEXT_MATCHES`, `SEARCH_MAX_BYTES` or
        /// `SEARCH_MAX_SECONDS`.
        truncated: bool,
    },
    /// `ReadFile`.
    FileText {
        path: String,
        start_line: u32,
        /// Last line included (inclusive).
        end_line: u32,
        total_lines: u32,
        text: String,
    },
    /// App-control tools, `SyncFolder` and `FetchFile`: what was done, e.g. "Opened
    /// “Fourier series · Lesson 2”." or "Synced “notes”: 3 files to send, 1 removed."
    Done { message: String },
    /// `Manifest`: the folder's allow-listed, visible files, in path order.
    Manifest { files: Vec<ManifestFile> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Relative to the workspace root, `/`-separated.
    pub path: String,
    pub is_dir: bool,
    /// File size; 0 for folders.
    pub bytes: u64,
    pub modified: Option<DateTime<Utc>>,
    /// Listed but not readable as text (binary, or larger than `READ_FILE_MAX_BYTES_TOTAL`).
    /// Kept for the device-side tools until 4-agent replaces them with service tools,
    /// which use `kind`.
    #[serde(default)]
    pub binary: bool,
    /// The allow-listed type of a file; `None` for folders.
    #[serde(default)]
    pub kind: Option<FolderFileType>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextMatch {
    pub path: String,
    /// 1-based.
    pub line: u32,
    /// The line, cut to `MATCH_LINE_MAX_CHARS`.
    pub text: String,
    /// The PDF page the match is on.
    #[serde(default)]
    pub page: Option<u32>,
}

// ---------------------------------------------------------------------------
// Folder rules, applied by the device that holds the folder
// ---------------------------------------------------------------------------

/// The only kinds of file a folder sync takes (owner Q2): prose documents, PDFs and
/// images. Code comes in through a repository source, not a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FolderFileType {
    Text,
    Pdf,
    Image,
}

impl FolderFileType {
    /// The kind of source the file becomes on the service.
    pub fn source_kind(self) -> SourceKind {
        match self {
            FolderFileType::Text => SourceKind::Text,
            FolderFileType::Pdf => SourceKind::Pdf,
            FolderFileType::Image => SourceKind::Image,
        }
    }
}

/// The only files a folder sync takes, by extension. One table, extended as the owner
/// allows more; nothing else anywhere decides what a folder file is.
pub const FOLDER_FILE_EXTENSIONS: &[(&str, FolderFileType)] = &[
    ("md", FolderFileType::Text),
    ("markdown", FolderFileType::Text),
    ("txt", FolderFileType::Text),
    ("text", FolderFileType::Text),
    ("rst", FolderFileType::Text),
    ("adoc", FolderFileType::Text),
    ("org", FolderFileType::Text),
    ("tex", FolderFileType::Text),
    ("bib", FolderFileType::Text),
    ("html", FolderFileType::Text),
    ("htm", FolderFileType::Text),
    ("pdf", FolderFileType::Pdf),
    ("png", FolderFileType::Image),
    ("jpg", FolderFileType::Image),
    ("jpeg", FolderFileType::Image),
    ("heic", FolderFileType::Image),
    ("heif", FolderFileType::Image),
    ("webp", FolderFileType::Image),
    ("gif", FolderFileType::Image),
    ("tif", FolderFileType::Image),
    ("tiff", FolderFileType::Image),
    ("bmp", FolderFileType::Image),
];

/// Extension-less names that are text.
pub const FOLDER_FILE_NAMES: &[&str] = &["readme", "license"];

/// What the folder sync takes, as people read it.
pub const FOLDER_FILE_TYPES_TEXT: &str =
    "text documents (.md, .txt, .rst, .tex, .html…), PDFs and images";

/// The type of a file the sync takes, by its name or path (case-insensitive); `None` is
/// invisible: not listed, not uploaded, not searched. Hidden and secret-looking names
/// (`is_skipped_name`) are always `None`.
pub fn folder_file_type(name: &str) -> Option<FolderFileType> {
    let file = name.rsplit(['/', '\\']).next().unwrap_or(name);
    if file.is_empty() || is_skipped_name(file) {
        return None;
    }
    let lower = file.to_lowercase();
    match lower.rsplit_once('.') {
        Some((_, ext)) => FOLDER_FILE_EXTENSIONS
            .iter()
            .find(|(e, _)| *e == ext)
            .map(|(_, t)| *t),
        None => FOLDER_FILE_NAMES
            .contains(&lower.as_str())
            .then_some(FolderFileType::Text),
    }
}

/// How often a device that holds a folder syncs it while Margin runs.
pub const FOLDER_SYNC_INTERVAL_SECS: u64 = 900;

/// Most lines one `ReadFile` returns.
pub const READ_FILE_MAX_LINES: u32 = 400;
/// Most bytes of text one `ReadFile` returns.
pub const READ_FILE_MAX_BYTES: usize = 64 * 1024;
/// Files larger than this are listed but never read.
pub const READ_FILE_MAX_BYTES_TOTAL: u64 = 2 * 1024 * 1024;
/// Most bytes one `SearchText` reads before stopping.
pub const SEARCH_MAX_BYTES: u64 = 50 * 1024 * 1024;
/// Longest one `SearchText` runs before stopping.
pub const SEARCH_MAX_SECONDS: u64 = 20;
/// Most entries `ListFiles` / `FindFiles` return.
pub const MAX_LISTED_ENTRIES: usize = 500;
/// Most matches `SearchText` returns.
pub const MAX_TEXT_MATCHES: usize = 200;
/// Longest matched line returned.
pub const MATCH_LINE_MAX_CHARS: usize = 300;

/// Why a path a tool was given is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    Absolute,
    /// A `..` component.
    Parent,
    Nul,
    /// A component that is hidden or looks like a secret (see `is_skipped_name`).
    Skipped,
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PathError::Absolute => "paths must be relative to the folder",
            PathError::Parent => "paths can't go above the folder (no \"..\")",
            PathError::Nul => "the path contains a NUL character",
            PathError::Skipped => "hidden files and files that may hold secrets aren't read",
        })
    }
}

/// Checks a path the model gave and returns its components. Relative only: no leading
/// `/` or `\`, no drive letter, no `..`, no NUL; `.` and empty components are dropped;
/// `\` separates like `/`. A component that `is_skipped_name` is refused. The device
/// still joins the components to its root and checks the canonical result is inside
/// the root (symlinks out are refused there).
pub fn relative_components(path: &str) -> Result<Vec<String>, PathError> {
    if path.contains('\0') {
        return Err(PathError::Nul);
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err(PathError::Absolute);
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return Err(PathError::Absolute);
    }
    let mut out = Vec::new();
    for part in path.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => return Err(PathError::Parent),
            name if is_skipped_name(name) => return Err(PathError::Skipped),
            name => out.push(name.to_string()),
        }
    }
    Ok(out)
}

/// Names a folder sync never lists, uploads or reads: hidden files and folders (`.git`,
/// `.env*`, `.ssh` …) and files that may hold secrets. Folders are walked whatever their
/// name (owner Q2); what is taken from them is decided by `folder_file_type`.
pub fn is_skipped_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    if lower.starts_with('.') {
        return true;
    }
    lower.ends_with(".pem")
        || lower.ends_with(".key")
        || lower.ends_with(".p12")
        || lower.ends_with(".keychain")
        || lower.starts_with("id_rsa")
        || lower.starts_with("id_ed25519")
        || lower.starts_with("credentials")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round_trip<T>(value: &T) -> serde_json::Value
    where
        T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_value(value).unwrap();
        let back: T = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(&back, value);
        json
    }

    fn request(call: DeviceToolCall) -> DirectedFrame {
        DirectedFrame::ToolRequest {
            request: ToolRequest {
                request_id: "r1".into(),
                chat_id: "c1".into(),
                call_id: "call_1".into(),
                deadline: DateTime::from_timestamp(1_760_000_000, 0).unwrap(),
                call,
            },
        }
    }

    #[test]
    fn directed_frames_round_trip() {
        let calls = vec![
            DeviceToolCall::ListFiles {
                workspace_id: "w".into(),
                path: String::new(),
            },
            DeviceToolCall::FindFiles {
                workspace_id: "w".into(),
                path: "src".into(),
                pattern: "**/*.rs".into(),
            },
            DeviceToolCall::SearchText {
                workspace_id: "w".into(),
                path: String::new(),
                query: "fn main".into(),
                pattern: None,
            },
            DeviceToolCall::ReadFile {
                workspace_id: "w".into(),
                path: "README.md".into(),
                start_line: 1,
                max_lines: 400,
            },
            DeviceToolCall::Navigate {
                to: NavTarget::Lesson {
                    lesson_id: "l".into(),
                    view: Some(LessonView::Video),
                },
            },
            DeviceToolCall::Navigate {
                to: NavTarget::AllNotes,
            },
            DeviceToolCall::OpenCitation {
                lesson_id: "l".into(),
                n: 2,
            },
            DeviceToolCall::ShowPassage {
                source_id: "s".into(),
                page: None,
                start_ms: Some(1500),
            },
            DeviceToolCall::ToggleSidebar { visible: None },
            DeviceToolCall::SyncFolder {
                workspace_id: "w".into(),
            },
            DeviceToolCall::Manifest {
                workspace_id: "w".into(),
            },
            DeviceToolCall::FetchFile {
                workspace_id: "w".into(),
                path: "papers/a.pdf".into(),
                stamp: FileStamp {
                    bytes: 3,
                    modified: None,
                    sha256: "00".repeat(32),
                },
            },
        ];
        for call in calls {
            let frame = request(call);
            round_trip(&frame);
            // On the socket it arrives as a ServerFrame.
            let text = serde_json::to_string(&frame).unwrap();
            assert_eq!(
                serde_json::from_str::<ServerFrame>(&text).unwrap(),
                ServerFrame::Directed(frame)
            );
        }
        let json = round_trip(&DirectedFrame::ToolCancel {
            request_id: "r1".into(),
        });
        assert_eq!(json, json!({"type": "tool_cancel", "request_id": "r1"}));
    }

    #[test]
    fn events_still_parse_as_server_frames() {
        let hello = Event::Hello {
            server_version: "1".into(),
        };
        let text = serde_json::to_string(&hello).unwrap();
        assert_eq!(
            serde_json::from_str::<ServerFrame>(&text).unwrap(),
            ServerFrame::Event(hello)
        );
    }

    #[test]
    fn device_frames_round_trip() {
        let outputs = vec![
            DeviceToolResult::Ok {
                output: DeviceToolOutput::Files {
                    entries: vec![FileEntry {
                        path: "src".into(),
                        is_dir: true,
                        bytes: 0,
                        modified: None,
                        binary: false,
                        kind: None,
                    }],
                    truncated: false,
                },
            },
            DeviceToolResult::Ok {
                output: DeviceToolOutput::Matches {
                    matches: vec![TextMatch {
                        path: "a.rs".into(),
                        line: 3,
                        text: "fn main() {}".into(),
                        page: None,
                    }],
                    truncated: true,
                },
            },
            DeviceToolResult::Ok {
                output: DeviceToolOutput::FileText {
                    path: "a.rs".into(),
                    start_line: 1,
                    end_line: 1,
                    total_lines: 1,
                    text: "fn main() {}".into(),
                },
            },
            DeviceToolResult::Ok {
                output: DeviceToolOutput::Done {
                    message: "Opened.".into(),
                },
            },
            DeviceToolResult::Ok {
                output: DeviceToolOutput::Manifest {
                    files: vec![ManifestFile {
                        path: "notes/a.md".into(),
                        kind: FolderFileType::Text,
                        stamp: FileStamp {
                            bytes: 1,
                            modified: None,
                            sha256: "ff".repeat(32),
                        },
                    }],
                },
            },
            DeviceToolResult::Err {
                message: "no access".into(),
            },
        ];
        for result in outputs {
            round_trip(&DeviceFrame::ToolResult {
                request_id: "r1".into(),
                result,
            });
        }
    }

    #[test]
    fn folder_tools_name_their_workspace() {
        let read = DeviceToolCall::ReadFile {
            workspace_id: "w".into(),
            path: "a".into(),
            start_line: 1,
            max_lines: 1,
        };
        assert_eq!(read.workspace_id(), Some("w"));
        assert_eq!(
            DeviceToolCall::SyncFolder {
                workspace_id: "w2".into()
            }
            .workspace_id(),
            Some("w2")
        );
        assert_eq!(
            DeviceToolCall::ToggleSidebar {
                visible: Some(true)
            }
            .workspace_id(),
            None
        );
    }

    #[test]
    fn relative_paths_only() {
        assert_eq!(relative_components("").unwrap(), Vec::<String>::new());
        assert_eq!(
            relative_components("./src//main.rs").unwrap(),
            vec!["src", "main.rs"]
        );
        assert_eq!(
            relative_components("src\\lib.rs").unwrap(),
            vec!["src", "lib.rs"]
        );
        assert_eq!(relative_components("/etc/passwd"), Err(PathError::Absolute));
        assert_eq!(
            relative_components("\\\\server\\x"),
            Err(PathError::Absolute)
        );
        assert_eq!(relative_components("C:\\x"), Err(PathError::Absolute));
        assert_eq!(relative_components("a/../../b"), Err(PathError::Parent));
        assert_eq!(relative_components("a\0b"), Err(PathError::Nul));
        assert_eq!(relative_components("a/.env"), Err(PathError::Skipped));
        assert_eq!(relative_components(".git/config"), Err(PathError::Skipped));
        assert_eq!(
            relative_components("keys/server.PEM"),
            Err(PathError::Skipped)
        );
        // Look-alike dots are just names, not parents.
        assert_eq!(
            relative_components("a/\u{2024}\u{2024}/b").unwrap().len(),
            3
        );
    }

    #[test]
    fn skipped_names() {
        for name in [
            ".git",
            ".env.local",
            "id_rsa.pub",
            "credentials.json",
            "credentials.txt",
            "cert.p12",
        ] {
            assert!(is_skipped_name(name), "{name}");
        }
        // Folders are walked whatever their name.
        for name in [
            "src",
            "README.md",
            "notes.txt",
            "keyboard.md",
            "build",
            "target",
            "node_modules",
        ] {
            assert!(!is_skipped_name(name), "{name}");
        }
    }

    #[test]
    fn the_folder_allow_list() {
        let mut seen = std::collections::HashSet::new();
        for (ext, _) in FOLDER_FILE_EXTENSIONS {
            assert!(seen.insert(*ext), "{ext} twice");
            assert_eq!(*ext, ext.to_lowercase());
        }
        assert_eq!(folder_file_type("Notes/Ch1.MD"), Some(FolderFileType::Text));
        assert_eq!(folder_file_type("scan.PDF"), Some(FolderFileType::Pdf));
        assert_eq!(folder_file_type("photo.heic"), Some(FolderFileType::Image));
        assert_eq!(folder_file_type("README"), Some(FolderFileType::Text));
        assert_eq!(folder_file_type("docs/LICENSE"), Some(FolderFileType::Text));
        assert_eq!(
            folder_file_type("build/notes.md"),
            Some(FolderFileType::Text)
        );
        for invisible in [
            "main.rs",
            "data.json",
            "table.csv",
            "config.yaml",
            "archive.zip",
            "talk.m4a",
            "lecture.mov",
            "Makefile",
            ".env",
            ".hidden.md",
            "credentials.txt",
            "build",
            "",
        ] {
            assert_eq!(folder_file_type(invisible), None, "{invisible}");
        }
        assert_eq!(FolderFileType::Pdf.source_kind(), SourceKind::Pdf);
        assert_eq!(FolderFileType::Text.source_kind(), SourceKind::Text);
        assert_eq!(FolderFileType::Image.source_kind(), SourceKind::Image);
        // Every allow-listed extension is a source kind the service reads.
        for (ext, t) in FOLDER_FILE_EXTENSIONS {
            assert_eq!(
                SourceKind::for_name(&format!("x.{ext}")),
                Some(t.source_kind()),
                "{ext}"
            );
        }
    }
}
