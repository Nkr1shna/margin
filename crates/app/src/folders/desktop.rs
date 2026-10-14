//! Opening a held folder on macOS and Linux (4a): on macOS the bookmark, falling back
//! to the path; on Linux the path; both read through `StdDir`
//! (`docs/agent-phase4-brief.md` D1, D11). Not built yet: every open says so.

use super::{Dir, DirError, FolderHandle};

pub fn open(_handle: &FolderHandle) -> Result<Box<dyn Dir>, DirError> {
    Err(super::not_built())
}

pub fn forget(_handle: &FolderHandle) {}
