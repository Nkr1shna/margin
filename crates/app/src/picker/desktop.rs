//! Desktop: rfd's file and folder dialogs (an AppKit sheet on macOS, the XDG portal on
//! Linux). The files are read where they are.

use crate::sources::{Selection, select_files, select_folder};

pub async fn pick(folder: bool) -> Option<Selection> {
    let dialog = rfd::AsyncFileDialog::new();
    if folder {
        let handle = dialog.set_title("Add a folder").pick_folder().await?;
        let root = handle.path().to_path_buf();
        tokio::task::spawn_blocking(move || select_folder(&root))
            .await
            .ok()
    } else {
        let handles = dialog.set_title("Add files").pick_files().await?;
        let paths = handles.iter().map(|h| h.path().to_path_buf()).collect();
        tokio::task::spawn_blocking(move || select_files(paths))
            .await
            .ok()
    }
}
