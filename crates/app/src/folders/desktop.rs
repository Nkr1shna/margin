//! Opening a held folder on macOS and Linux (4a, `docs/agent-phase4-brief.md` D1, D11).
//!
//! macOS: the handle's bookmark is resolved first (without UI), so a folder that was
//! moved or renamed is still found; a bookmark the system calls stale is made again and
//! the handle updated. When the bookmark can't be resolved the path is used. Linux: the
//! path. Either way the folder is read through `StdDir`.
//!
//! A folder that is gone, or that the system won't let Margin read, is
//! `DirError::Revoked` with the message the Folders pane shows.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use super::{Dir, DirError, FolderHandle, StdDir};

pub fn open(handle: &FolderHandle) -> Result<Box<dyn Dir>, DirError> {
    let FolderHandle::Path { path, .. } = handle else {
        return Err(DirError::Revoked(format!(
            "This folder was attached on another kind of device; attach it again on {}.",
            this_device()
        )));
    };
    let name = folder_name(Path::new(path));
    let root = locate(handle);
    let dir = open_root(&root, &name)?;
    Ok(Box::new(dir))
}

/// Nothing to release: a path and a plain bookmark grant nothing.
pub fn forget(_handle: &FolderHandle) {}

/// The folder's own name, for messages and the pick: its last component, or the whole
/// path for a root.
pub(super) fn folder_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// What this device is called in "attach it again on …".
fn this_device() -> &'static str {
    if cfg!(target_os = "macos") {
        "this Mac"
    } else {
        "this computer"
    }
}

fn missing(name: &str) -> DirError {
    DirError::Revoked(format!(
        "The folder “{name}” can't be found; attach it again on {}.",
        this_device()
    ))
}

fn denied(name: &str) -> DirError {
    DirError::Revoked(if cfg!(target_os = "macos") {
        format!(
            "macOS didn't let Margin read “{name}”. Allow it under System Settings → Privacy & \
             Security → Files and Folders, then attach it again."
        )
    } else {
        format!(
            "Margin isn't allowed to read “{name}”. Let your user read it, then attach it again."
        )
    })
}

/// Opens the folder at `root`, reading its listing once so a folder that is gone or
/// can't be read is `Revoked` here rather than partway through a sync.
fn open_root(root: &Path, name: &str) -> Result<StdDir, DirError> {
    match std::fs::read_dir(root) {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::PermissionDenied => return Err(denied(name)),
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            return Err(missing(name));
        }
        Err(e) => return Err(DirError::Io(format!("Couldn't read “{name}”: {e}"))),
    }
    StdDir::new(root).map_err(|e| match e {
        DirError::NotFound | DirError::NotADir => missing(name),
        other => other,
    })
}

/// Handles whose bookmark was made again this run, by the handle they replace, so every
/// later open uses the fresh one. Each is also handed to `store_refreshed`.
fn refreshed() -> &'static Mutex<Vec<(FolderHandle, FolderHandle)>> {
    static REFRESHED: OnceLock<Mutex<Vec<(FolderHandle, FolderHandle)>>> = OnceLock::new();
    REFRESHED.get_or_init(Default::default)
}

/// The fresh handle for `handle`, if its bookmark was made again this run.
fn current(handle: &FolderHandle) -> FolderHandle {
    refreshed()
        .lock()
        .ok()
        .and_then(|map| {
            map.iter()
                .find(|(old, _)| old == handle)
                .map(|(_, new)| new.clone())
        })
        .unwrap_or_else(|| handle.clone())
}

/// Keeps a re-made handle: for the rest of this run here, and on disk through the handle
/// store (`store::update_handle`, which replaces `old` wherever it is held).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn store_refreshed(old: &FolderHandle, new: FolderHandle) {
    if let Ok(mut map) = refreshed().lock() {
        match map.iter_mut().find(|(held, _)| held == old) {
            Some((_, slot)) => *slot = new.clone(),
            None => map.push((old.clone(), new.clone())),
        }
    }
    super::resaved(old.clone(), new);
}

/// Where the folder is now.
fn locate(handle: &FolderHandle) -> PathBuf {
    let now = current(handle);
    let FolderHandle::Path { path, bookmark_b64 } = &now else {
        unreachable!("open checks the kind");
    };
    #[cfg(target_os = "macos")]
    if let Some(b64) = bookmark_b64
        && let Some(found) = mac::follow(handle, path, b64)
    {
        return found;
    }
    #[cfg(not(target_os = "macos"))]
    let _ = bookmark_b64;
    PathBuf::from(path)
}

#[cfg(target_os = "macos")]
mod mac {
    use std::path::{Path, PathBuf};

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use objc2::rc::autoreleasepool;
    use objc2::runtime::Bool;
    use objc2_foundation::{
        NSData, NSString, NSURL, NSURLBookmarkCreationOptions, NSURLBookmarkResolutionOptions,
    };

    use super::FolderHandle;

    /// A plain bookmark (options 0: not security-scoped; Margin isn't sandboxed) to the
    /// folder at `path`.
    pub fn bookmark(path: &Path) -> Result<Vec<u8>, String> {
        autoreleasepool(|_| {
            let url = NSURL::fileURLWithPath_isDirectory(
                &NSString::from_str(&path.to_string_lossy()),
                true,
            );
            url.bookmarkDataWithOptions_includingResourceValuesForKeys_relativeToURL_error(
                NSURLBookmarkCreationOptions(0),
                None,
                None,
            )
            .map(|data| data.to_vec())
            .map_err(|e| e.localizedDescription().to_string())
        })
    }

    /// Where a bookmark points now, and whether the system wants it made again.
    pub fn resolve(bytes: &[u8]) -> Result<(PathBuf, bool), String> {
        autoreleasepool(|_| {
            let data = NSData::with_bytes(bytes);
            let mut stale = Bool::NO;
            // SAFETY: `stale` is a valid pointer for the call.
            let url = unsafe {
                NSURL::URLByResolvingBookmarkData_options_relativeToURL_bookmarkDataIsStale_error(
                    &data,
                    NSURLBookmarkResolutionOptions::WithoutUI,
                    None,
                    &mut stale,
                )
            }
            .map_err(|e| e.localizedDescription().to_string())?;
            let path = url
                .path()
                .ok_or_else(|| "The bookmark doesn't lead to a path.".to_string())?;
            Ok((PathBuf::from(path.to_string()), stale.as_bool()))
        })
    }

    /// The folder's place by its bookmark, when the bookmark resolves to a folder that
    /// isn't in the Trash; a stale or moved bookmark is made again and kept as the new
    /// handle for `held` (the handle as the store has it). `None` falls back to the path.
    pub fn follow(held: &FolderHandle, path: &str, b64: &str) -> Option<PathBuf> {
        let bytes = STANDARD.decode(b64).ok()?;
        let (found, stale) = resolve(&bytes).ok()?;
        if in_trash(&found) {
            return None;
        }
        if (stale || found != Path::new(path))
            && let Ok(fresh) = bookmark(&found)
        {
            super::store_refreshed(
                held,
                FolderHandle::Path {
                    path: found.to_string_lossy().into_owned(),
                    bookmark_b64: Some(STANDARD.encode(fresh)),
                },
            );
        }
        Some(found)
    }

    /// A bookmark follows a folder into the Trash; a trashed folder counts as gone.
    fn in_trash(path: &Path) -> bool {
        std::env::var_os("HOME")
            .map(|home| path.starts_with(Path::new(&home).join(".Trash")))
            .unwrap_or(false)
            || path.components().any(|c| c.as_os_str() == ".Trashes")
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    /// A scratch folder removed when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "margin-desktop-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
            }
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn path_handle(path: &Path) -> FolderHandle {
        FolderHandle::Path {
            path: path.to_string_lossy().into_owned(),
            bookmark_b64: None,
        }
    }

    #[test]
    fn opens_a_folder_by_its_path() {
        let tmp = TempDir::new("open");
        std::fs::write(tmp.0.join("notes.md"), "hello").unwrap();
        let dir = open(&path_handle(&tmp.0)).unwrap();
        let names: Vec<String> = dir.list("").unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["notes.md"]);
        let mut text = String::new();
        dir.open("notes.md")
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "hello");
        forget(&path_handle(&tmp.0));
    }

    #[test]
    fn a_missing_folder_is_revoked() {
        let tmp = TempDir::new("missing");
        let gone = tmp.0.join("notes");
        let err = open(&path_handle(&gone)).err().unwrap();
        assert_eq!(
            err,
            DirError::Revoked(format!(
                "The folder “notes” can't be found; attach it again on {}.",
                this_device()
            ))
        );
        std::fs::write(&gone, "a file now").unwrap();
        assert!(matches!(
            open(&path_handle(&gone)),
            Err(DirError::Revoked(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_folder_is_revoked() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("denied");
        let locked = tmp.0.join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = open(&path_handle(&locked));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        if std::fs::read_dir("/root").is_ok() && cfg!(target_os = "linux") {
            return; // Running as root: nothing is unreadable.
        }
        let Err(DirError::Revoked(message)) = result else {
            panic!("expected Revoked");
        };
        assert!(message.contains("“locked”"), "{message}");
        if cfg!(target_os = "macos") {
            assert!(message.contains("System Settings"), "{message}");
        } else {
            assert!(!message.contains("System Settings"), "{message}");
        }
    }

    #[test]
    fn other_kinds_of_handle_are_revoked() {
        let err = open(&FolderHandle::AndroidTree {
            uri: "content://x".into(),
        })
        .err()
        .unwrap();
        assert!(matches!(err, DirError::Revoked(m) if m.contains("another kind of device")));
    }

    #[test]
    fn names_are_the_last_component() {
        assert_eq!(folder_name(Path::new("/Users/k/Documents/notes")), "notes");
        assert_eq!(folder_name(Path::new("/Users/k/notes/")), "notes");
        assert_eq!(folder_name(Path::new("/")), "/");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bookmarks_follow_a_moved_folder() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD;

        let tmp = TempDir::new("moved");
        let before = tmp.0.join("notes");
        std::fs::create_dir(&before).unwrap();
        std::fs::write(before.join("ch1.md"), "one").unwrap();
        let bookmark = mac::bookmark(&before).unwrap();
        let (found, _) = mac::resolve(&bookmark).unwrap();
        assert_eq!(
            std::fs::canonicalize(&found).unwrap(),
            std::fs::canonicalize(&before).unwrap()
        );

        let handle = FolderHandle::Path {
            path: before.to_string_lossy().into_owned(),
            bookmark_b64: Some(STANDARD.encode(&bookmark)),
        };
        let after = tmp.0.join("renamed");
        std::fs::rename(&before, &after).unwrap();
        let dir = open(&handle).unwrap();
        assert_eq!(dir.list("").unwrap()[0].name, "ch1.md");

        // The handle was made again for the new place, and later opens use it.
        let FolderHandle::Path { path, bookmark_b64 } = current(&handle) else {
            panic!("a path handle");
        };
        assert_eq!(
            std::fs::canonicalize(&path).unwrap(),
            std::fs::canonicalize(&after).unwrap()
        );
        assert!(bookmark_b64.is_some());
        assert!(open(&handle).is_ok());

        // Gone altogether: the bookmark can't resolve and the path is missing.
        std::fs::remove_dir_all(&after).unwrap();
        assert!(matches!(open(&handle), Err(DirError::Revoked(m)) if m.contains("can't be found")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_bad_bookmark_falls_back_to_the_path() {
        let tmp = TempDir::new("badmark");
        std::fs::write(tmp.0.join("a.md"), "a").unwrap();
        let handle = FolderHandle::Path {
            path: tmp.0.to_string_lossy().into_owned(),
            bookmark_b64: Some("bm90IGEgYm9va21hcms=".into()),
        };
        assert_eq!(open(&handle).unwrap().list("").unwrap().len(), 1);
    }
}
