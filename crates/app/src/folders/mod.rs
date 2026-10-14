//! Folders this device holds and attaches to chats (phase 4). The folder's handle (a
//! path and bookmark on macOS, a security-scoped bookmark on iOS and iPadOS, a tree URI
//! on Android) stays on this device; the sync engine walks the folder through `Dir`,
//! sends the service a manifest of its allow-listed files
//! (`margin_shared::folder_file_type`) and uploads what the service asks for.
//!
//! This file is the contract between 4-core (`store`, `sync`, the UI) and the platform
//! areas 4a/4b/4c (`open`, `forget`, `picker::pick_folder`).

use std::io::Read;

use serde::{Deserialize, Serialize};

#[cfg(target_os = "android")]
mod android;
#[cfg(not(any(target_os = "ios", target_os = "android")))]
mod desktop;
mod dir;
#[cfg(target_os = "ios")]
mod ios;
pub mod store;
pub mod sync;

#[cfg(target_os = "android")]
use android as platform;
#[cfg(not(any(target_os = "ios", target_os = "android")))]
use desktop as platform;
#[cfg(target_os = "ios")]
use ios as platform;

#[allow(unused_imports)] // 4a and 4b open folders with it.
pub use dir::StdDir;

/// How this device reaches a folder again. Never leaves the device: not in a request
/// body, a log line or an event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum FolderHandle {
    /// macOS and Linux: the path, and on macOS a plain bookmark (base64) that follows the
    /// folder if it moves.
    Path {
        path: String,
        #[serde(default)]
        bookmark_b64: Option<String>,
    },
    /// iOS and iPadOS: a security-scoped bookmark (base64).
    IosBookmark { bookmark_b64: String },
    /// Android: a document tree URI with a persisted read grant.
    AndroidTree { uri: String },
}

/// What the folder picker hands back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderPick {
    /// The folder's own name, e.g. "notes".
    pub name: String,
    /// Where it is, as the owner would recognise it ("~/Documents/notes", "iCloud
    /// Drive/notes"). Display only.
    pub location: String,
    pub handle: FolderHandle,
}

/// What an entry of a folder is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirKind {
    File,
    Dir,
}

/// One entry of a folder, as `Dir::list` and `Dir::stat` give it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// The entry's own name (not a path).
    pub name: String,
    pub kind: DirKind,
    /// File size; 0 for folders.
    pub bytes: u64,
    /// When the platform gives it.
    pub modified: Option<chrono::DateTime<chrono::Utc>>,
}

/// Why a folder can't be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirError {
    NotFound,
    NotADir,
    IsADir,
    /// The path would leave the folder (a symbolic link, or a canonical path outside
    /// the root).
    Outside,
    /// Access to the folder was lost; the message is what the Folders pane shows.
    Revoked(String),
    Io(String),
}

impl std::fmt::Display for DirError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DirError::NotFound => f.write_str("There's no such file or folder."),
            DirError::NotADir => f.write_str("That's a file, not a folder."),
            DirError::IsADir => f.write_str("That's a folder, not a file."),
            DirError::Outside => f.write_str("That path leads outside the folder."),
            DirError::Revoked(message) | DirError::Io(message) => f.write_str(message),
        }
    }
}

/// A folder this device holds, opened for one sync and dropped after it. Paths are
/// `/`-separated and relative to the root; `""` is the root. The sync walks every folder
/// (hidden names excepted) and takes only files `margin_shared::folder_file_type` knows;
/// a `Dir` doesn't filter. It does refuse anything that would leave the root: a symbolic
/// link (to a file or a folder, anywhere in the path) is never followed — `list` leaves
/// it out, `stat`/`open` answer `DirError::Outside` — and, where the platform can
/// canonicalise, a resolved path outside the canonical root is `Outside` too. Android's
/// document trees have no links, so there it never happens. `list` returns entries in
/// any order (the sync sorts by name) and only files and folders (special files are left
/// out), each with its size and modification time when the platform gives one. `open`
/// streams where the platform allows it. Every method may block; the sync runs them on a
/// blocking thread. A folder whose access was lost (the bookmark can't be resolved, the
/// URI grant is gone, the path is gone, permission is denied) is
/// `DirError::Revoked(message)` with the message the Folders pane shows, e.g. "Margin no
/// longer has access to the folder “notes”; attach it again on the iPhone."
pub trait Dir {
    fn list(&self, rel: &str) -> Result<Vec<DirEntry>, DirError>;
    /// `NotFound`, `Outside`.
    fn stat(&self, rel: &str) -> Result<DirEntry, DirError>;
    /// `IsADir`, `NotFound`, `Outside`.
    fn open(&self, rel: &str) -> Result<Box<dyn Read + '_>, DirError>;
}

/// The message for a platform whose folder access isn't built yet.
fn not_built() -> DirError {
    DirError::Revoked("Folders aren't built on this platform yet.".into())
}

/// Opens a folder from its handle for one sync (4a macOS and Linux, 4b iOS and iPadOS,
/// 4c Android). Blocking.
pub fn open(handle: &FolderHandle) -> Result<Box<dyn Dir>, DirError> {
    platform::open(handle)
}

/// Releases what holding the folder takes (an Android URI grant); after the folder is
/// removed. Blocking.
pub fn forget(handle: &FolderHandle) {
    platform::forget(handle)
}

/// Handles `open` made again (a stale macOS or iOS bookmark), each with the one it
/// replaces, waiting for the sync engine to save them (`take_resaved`).
fn resaved_queue() -> &'static std::sync::Mutex<Vec<(FolderHandle, FolderHandle)>> {
    static QUEUE: std::sync::OnceLock<std::sync::Mutex<Vec<(FolderHandle, FolderHandle)>>> =
        std::sync::OnceLock::new();
    QUEUE.get_or_init(Default::default)
}

/// The platform's `open` (4a, 4b) made a new handle for a folder it was given `old`
/// for: the store replaces it after the open (`store::update_handle`). Any thread.
#[cfg_attr(not(any(target_os = "macos", target_os = "ios")), allow(dead_code))]
pub fn resaved(old: FolderHandle, new: FolderHandle) {
    resaved_queue()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((old, new));
}

/// Every `(old, new)` pair `resaved` queued since the last call, oldest first. On every
/// platform; Android never queues any.
pub fn take_resaved() -> Vec<(FolderHandle, FolderHandle)> {
    std::mem::take(&mut *resaved_queue().lock().unwrap_or_else(|e| e.into_inner()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_round_trip_tagged_by_kind() {
        for handle in [
            FolderHandle::Path {
                path: "/Users/k/notes".into(),
                bookmark_b64: Some("Ym9vaw==".into()),
            },
            FolderHandle::IosBookmark {
                bookmark_b64: "Ym9vaw==".into(),
            },
            FolderHandle::AndroidTree {
                uri: "content://com.android.externalstorage.documents/tree/primary%3Anotes".into(),
            },
        ] {
            let json = serde_json::to_value(&handle).unwrap();
            assert!(json["kind"].is_string());
            assert_eq!(
                serde_json::from_value::<FolderHandle>(json).unwrap(),
                handle
            );
        }
        let linux: FolderHandle =
            serde_json::from_str(r#"{"kind":"Path","path":"/home/k/notes"}"#).unwrap();
        assert_eq!(
            linux,
            FolderHandle::Path {
                path: "/home/k/notes".into(),
                bookmark_b64: None
            }
        );
    }

    #[test]
    fn another_platforms_folder_is_revoked_here() {
        let handle = FolderHandle::AndroidTree { uri: "x".into() };
        // A folder attached on an Android device can't be opened on a desktop: the
        // Folders pane says to attach it again here.
        let err = open(&handle).err().unwrap();
        assert!(matches!(err, DirError::Revoked(_)), "{err:?}");
        assert!(!err.to_string().is_empty());
        forget(&handle);
    }
}
