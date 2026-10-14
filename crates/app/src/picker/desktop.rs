//! Desktop: rfd's file dialog (an AppKit sheet on macOS, the XDG portal on Linux). The
//! files are read where they are.

use crate::folders::FolderPick;
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

/// The folder picker (4a: rfd's folder dialog, a bookmark on macOS, the TCC probe). Not
/// built yet.
pub async fn pick_folder() -> Option<FolderPick> {
    None
}
