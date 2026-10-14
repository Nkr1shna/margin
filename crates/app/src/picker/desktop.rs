//! Desktop: rfd's dialogs (an AppKit sheet on macOS, the XDG portal on Linux). Picked
//! files are read where they are; a picked folder is held by its path (and on macOS a
//! bookmark that follows it if it moves) and synced from there.

use std::path::{Path, PathBuf};

use crate::folders::{FolderHandle, FolderPick};
use crate::sources::{Selection, select_files};

pub async fn pick_files() -> Option<Selection> {
    let handles = rfd::AsyncFileDialog::new()
        .set_title("Add files")
        .pick_files()
        .await?;
    let paths = handles.iter().map(|h| h.path().to_path_buf()).collect();
    tokio::task::spawn_blocking(move || select_files(paths))
        .await
        .ok()
}

/// The folder dialog; the pick is held by its path, with a plain bookmark on macOS.
pub async fn pick_folder() -> Option<FolderPick> {
    let picked = rfd::AsyncFileDialog::new()
        .set_title("Attach a folder")
        .pick_folder()
        .await?;
    let path = picked.path().to_path_buf();
    tokio::task::spawn_blocking(move || held(path)).await.ok()
}

/// What holding `path` takes. Blocking: on macOS it makes the bookmark and reads the
/// folder once, so that macOS asks now (for Desktop, Documents, Downloads and the like)
/// rather than from the first sync's background thread.
fn held(path: PathBuf) -> FolderPick {
    #[cfg(target_os = "macos")]
    let bookmark_b64 = {
        use base64::Engine;
        let _ = std::fs::read_dir(&path);
        match bookmark(&path) {
            Ok(bytes) => Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
            Err(error) => {
                // The path still works; only a later move of the folder isn't followed.
                eprintln!("couldn't bookmark the folder: {error}");
                None
            }
        }
    };
    #[cfg(not(target_os = "macos"))]
    let bookmark_b64 = None;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    FolderPick {
        name: name(&path),
        location: location(&path, home.as_deref()),
        handle: FolderHandle::Path {
            path: path.to_string_lossy().into_owned(),
            bookmark_b64,
        },
    }
}

/// A plain bookmark (options 0: Margin isn't sandboxed) to the folder at `path`.
#[cfg(target_os = "macos")]
fn bookmark(path: &Path) -> Result<Vec<u8>, String> {
    use objc2::rc::autoreleasepool;
    use objc2_foundation::{NSString, NSURL, NSURLBookmarkCreationOptions};

    autoreleasepool(|_| {
        let url =
            NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&path.to_string_lossy()), true);
        url.bookmarkDataWithOptions_includingResourceValuesForKeys_relativeToURL_error(
            NSURLBookmarkCreationOptions(0),
            None,
            None,
        )
        .map(|data| data.to_vec())
        .map_err(|e| e.localizedDescription().to_string())
    })
}

/// The folder's own name: its last component, or the whole path for a root.
fn name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Where the folder is, as the owner would write it: under the home folder as `~/…`.
fn location(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home.filter(|h| h.components().count() > 1)
        && let Ok(rest) = path.strip_prefix(home)
    {
        return if rest.as_os_str().is_empty() {
            "~".into()
        } else {
            format!("~/{}", rest.to_string_lossy())
        };
    }
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations_shorten_the_home_folder() {
        let home = Path::new("/Users/k");
        assert_eq!(
            location(Path::new("/Users/k/Documents/notes"), Some(home)),
            "~/Documents/notes"
        );
        assert_eq!(location(Path::new("/Users/k"), Some(home)), "~");
        assert_eq!(
            location(Path::new("/Users/kate/notes"), Some(home)),
            "/Users/kate/notes"
        );
        assert_eq!(
            location(Path::new("/Volumes/USB/notes"), Some(home)),
            "/Volumes/USB/notes"
        );
        assert_eq!(
            location(Path::new("/srv/notes"), Some(Path::new("/"))),
            "/srv/notes"
        );
        assert_eq!(location(Path::new("/srv/notes"), None), "/srv/notes");
    }

    #[test]
    fn names_are_the_last_component() {
        assert_eq!(name(Path::new("/Users/k/Documents/notes")), "notes");
        assert_eq!(name(Path::new("/")), "/");
    }

    #[test]
    fn a_held_folder_opens_by_its_handle() {
        let dir = std::env::temp_dir().join(format!("margin-pick-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.md"), "a").unwrap();
        let pick = held(dir.clone());
        assert_eq!(pick.name, name(&dir));
        let FolderHandle::Path { path, bookmark_b64 } = &pick.handle else {
            panic!("a path handle");
        };
        assert_eq!(Path::new(path), dir);
        assert_eq!(bookmark_b64.is_some(), cfg!(target_os = "macos"));
        let opened = crate::folders::open(&pick.handle).unwrap();
        assert_eq!(opened.list("").unwrap()[0].name, "a.md");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
