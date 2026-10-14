//! The system pickers that add sources: files or a folder, chosen with the platform's own
//! picker (AppKit or the XDG portal on desktop, `UIDocumentPickerViewController` on iOS
//! and iPadOS, the Storage Access Framework on Android). Each returns the same
//! `Selection` of files to upload, read from local paths.
//!
//! Dioxus 0.7.9's `<input type=file>` can't be used for this on mobile: it asks the host,
//! which has no picker there and answers with no files.
//!
//! A mobile picker only grants access to what it returns for a while (an iOS security
//! scope, an Android URI grant), so the files it chose are copied into a staging folder in
//! the app's caches and read from there; each copy is removed once its upload is done or
//! dropped, and whatever is left is cleared when the app starts.

use std::path::{Path, PathBuf};

use crate::sources::Selection;

#[cfg(target_os = "android")]
mod android;
#[cfg(not(any(target_os = "ios", target_os = "android")))]
mod desktop;
#[cfg(target_os = "ios")]
mod ios;

#[cfg(target_os = "android")]
use android as platform;
#[cfg(not(any(target_os = "ios", target_os = "android")))]
use desktop as platform;
#[cfg(target_os = "ios")]
use ios as platform;

/// Ask for files (several at once) or a folder and choose what to upload from them;
/// `None` when the picker was cancelled or couldn't be shown.
pub async fn pick(folder: bool) -> Option<Selection> {
    platform::pick(folder).await
}

/// Where picked files wait for their upload; `None` on desktop, which reads the files
/// where they are.
fn staging_root() -> Option<PathBuf> {
    #[cfg(target_os = "ios")]
    {
        Some(std::env::temp_dir().join("Margin-picked"))
    }
    #[cfg(target_os = "android")]
    {
        crate::config::cache_dir().map(|dir| dir.join("Margin-picked"))
    }
    #[cfg(not(any(target_os = "ios", target_os = "android")))]
    {
        None
    }
}

/// A new, empty folder for one add's copies.
#[cfg_attr(not(any(target_os = "ios", target_os = "android")), allow(dead_code))]
fn new_staging_dir() -> Option<PathBuf> {
    let root = staging_root()?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let dir = root.join(format!("{stamp:x}"));
    match std::fs::create_dir_all(&dir) {
        Ok(()) => Some(dir),
        Err(error) => {
            eprintln!("couldn't create {}: {error}", dir.display());
            None
        }
    }
}

/// Remove `path`'s staged copy, if it is one, now that it's uploaded or dropped; the
/// add's folder goes with its last file.
pub fn release(path: &Path) {
    if let Some(root) = staging_root() {
        release_in(&root, path);
    }
}

fn release_in(root: &Path, path: &Path) {
    if !path.starts_with(root) || path == root {
        return;
    }
    let _ = std::fs::remove_file(path);
    if let Some(dir) = path.parent().filter(|dir| *dir != root) {
        // Fails while other files of the add are still waiting.
        let _ = std::fs::remove_dir(dir);
    }
}

/// Clear copies left from an earlier run (uploads don't outlive the app).
pub fn clear_staging() {
    if let Some(root) = staging_root() {
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_removes_only_staged_copies() {
        let root = std::env::temp_dir().join(format!("margin-picker-{}", std::process::id()));
        let add = root.join("1a2b");
        std::fs::create_dir_all(&add).unwrap();
        let first = add.join("0000-a.md");
        let second = add.join("0001-b.md");
        std::fs::write(&first, "a").unwrap();
        std::fs::write(&second, "b").unwrap();
        let outside = std::env::temp_dir().join(format!(
            "margin-picker-outside-{}.md",
            std::process::id()
        ));
        std::fs::write(&outside, "keep").unwrap();

        release_in(&root, &outside);
        assert!(outside.exists(), "a file outside the staging folder stays");
        release_in(&root, &root);
        assert!(root.exists());

        release_in(&root, &first);
        assert!(!first.exists());
        assert!(add.exists(), "the add's folder stays while a file waits");
        release_in(&root, &second);
        assert!(!add.exists(), "the add's folder goes with its last file");
        assert!(root.exists());

        std::fs::remove_file(&outside).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }
}
