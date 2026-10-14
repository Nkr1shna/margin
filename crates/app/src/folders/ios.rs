//! Opening a held folder on iOS and iPadOS (4b): the security-scoped bookmark, read
//! through an `IosDir` over `StdDir` with coordinated reads
//! (`docs/agent-phase4-brief.md` D1, D11). Not built yet: every open says so.

use super::{Dir, DirError, FolderHandle};

pub fn open(_handle: &FolderHandle) -> Result<Box<dyn Dir>, DirError> {
    Err(super::not_built())
}

pub fn forget(_handle: &FolderHandle) {}
