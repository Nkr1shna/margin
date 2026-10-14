//! Opening a held folder on iOS and iPadOS (`docs/agent-phase4-brief.md` D1, D11). The
//! handle is the bookmark `picker::ios::pick_folder` made inside the folder's security
//! scope; `open` resolves it, starts the scope again and hands out an `IosDir`: `StdDir`
//! for listing (so the symlink rules are the same as on the Mac), with every file read
//! through `NSFileCoordinator` into memory, so a file that's only in iCloud is downloaded
//! first and a file being written is read whole. Dropping the `IosDir` stops the scope.
//! A bookmark the system calls stale still resolves; it is made again inside the scope
//! and queued for the store with `folders::resaved` (the sync engine saves it).

use std::cell::RefCell;
use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::ptr::NonNull;

use base64::Engine as _;
use block2::StackBlock;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyObject, Bool};
use objc2::{ClassType, msg_send};
use objc2_foundation::{
    NSArray, NSData, NSError, NSFileCoordinator, NSFileCoordinatorReadingOptions, NSString, NSURL,
    NSURLBookmarkCreationOptions, NSURLBookmarkResolutionOptions,
};

use super::{Dir, DirEntry, DirError, DirKind, FolderHandle, StdDir};

pub fn open(handle: &FolderHandle) -> Result<Box<dyn Dir>, DirError> {
    let FolderHandle::IosBookmark { bookmark_b64 } = handle else {
        return Err(DirError::Revoked(format!(
            "This folder wasn't attached on this {}; attach it again here.",
            device_name()
        )));
    };
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(bookmark_b64) else {
        return Err(revoked(None));
    };
    autoreleasepool(|_| {
        let data = NSData::with_bytes(&bytes);
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
        .map_err(|_| revoked(bookmark_name(&data).as_deref()))?;
        let name = url.lastPathComponent().map(|n| n.to_string());
        // SAFETY: balanced by `Scope`'s `Drop`.
        if !unsafe { url.startAccessingSecurityScopedResource() } {
            return Err(revoked(name.as_deref()));
        }
        let scope = Scope(url);
        if stale.as_bool()
            && let Ok(fresh) = scope
                .0
                .bookmarkDataWithOptions_includingResourceValuesForKeys_relativeToURL_error(
                    NSURLBookmarkCreationOptions(0),
                    None,
                    None,
                )
        {
            let fresh = FolderHandle::IosBookmark {
                bookmark_b64: base64::engine::general_purpose::STANDARD.encode(fresh.to_vec()),
            };
            super::resaved(handle.clone(), fresh);
        }
        let root = PathBuf::from(
            scope
                .0
                .path()
                .ok_or_else(|| revoked(name.as_deref()))?
                .to_string(),
        );
        let inner = StdDir::new(&root).map_err(|e| match e {
            DirError::NotFound | DirError::NotADir => revoked(name.as_deref()),
            other => other,
        })?;
        Ok(Box::new(IosDir {
            inner,
            root,
            _scope: scope,
        }) as Box<dyn Dir>)
    })
}

/// Nothing to release: a bookmark holds no grant beyond itself.
pub fn forget(_handle: &FolderHandle) {}

/// "Margin no longer has access to the folder “notes”; attach it again on this iPhone."
fn revoked(name: Option<&str>) -> DirError {
    let folder = match name {
        Some(name) if !name.is_empty() => format!("the folder “{name}”"),
        _ => "this folder".into(),
    };
    DirError::Revoked(format!(
        "Margin no longer has access to {folder}; attach it again on this {}.",
        device_name()
    ))
}

/// The folder's name as the bookmark remembers it, for when it can't be resolved.
fn bookmark_name(data: &NSData) -> Option<String> {
    let key = NSString::from_str("NSURLNameKey");
    let keys = NSArray::from_slice(&[&*key]);
    // SAFETY: `+[NSURL resourceValuesForKeys:fromBookmarkData:]` takes an array of
    // resource keys and bookmark data, and returns a dictionary or nil.
    let values: Option<Retained<AnyObject>> =
        unsafe { msg_send![NSURL::class(), resourceValuesForKeys: &*keys, fromBookmarkData: data] };
    // SAFETY: `-[NSDictionary objectForKey:]` with a string key.
    let value: Option<Retained<AnyObject>> = unsafe { msg_send![&*values?, objectForKey: &*key] };
    Some(value?.downcast::<NSString>().ok()?.to_string())
}

/// A started security scope, stopped when dropped.
struct Scope(Retained<NSURL>);

impl Drop for Scope {
    fn drop(&mut self) {
        // SAFETY: started in `open` before the `Scope` was made.
        unsafe { self.0.stopAccessingSecurityScopedResource() };
    }
}

/// A held folder inside its security scope.
struct IosDir {
    inner: StdDir,
    root: PathBuf,
    /// Dropped last, after nothing reads the folder any more.
    _scope: Scope,
}

/// The name iCloud gives a file it hasn't downloaded yet (older systems): `.name.icloud`.
fn placeholder_of(name: &str) -> String {
    format!(".{name}.icloud")
}

/// The file a placeholder stands for, if `name` is one.
fn placeholder_for(name: &str) -> Option<&str> {
    name.strip_prefix('.')?
        .strip_suffix(".icloud")
        .filter(|n| !n.is_empty())
}

/// `rel` split into its parent and its own name.
fn split_rel(rel: &str) -> (&str, &str) {
    let rel = rel.trim_end_matches('/');
    rel.rsplit_once('/').unwrap_or(("", rel))
}

impl IosDir {
    /// Whether `rel` is a file iCloud hasn't downloaded yet (only its placeholder is here).
    fn is_placeholder(&self, rel: &str) -> bool {
        let (parent, name) = split_rel(rel);
        if name.is_empty() {
            return false;
        }
        let stand_in = if parent.is_empty() {
            placeholder_of(name)
        } else {
            format!("{parent}/{}", placeholder_of(name))
        };
        matches!(self.inner.stat(&stand_in), Ok(e) if e.kind == DirKind::File)
    }

    /// Read `rel` whole under a coordinated read (which downloads it first when it's
    /// only in iCloud); the read itself goes through `StdDir`, so its rules apply.
    fn coordinated_read(&self, rel: &str) -> Result<Vec<u8>, DirError> {
        let mut path = self.root.clone();
        for part in rel
            .split(['/', '\\'])
            .filter(|p| !p.is_empty() && *p != ".")
        {
            path.push(part);
        }
        autoreleasepool(|_| {
            let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
            let coordinator = NSFileCoordinator::new();
            let read = RefCell::new(Err(DirError::Io("The file wasn't read.".into())));
            let reader = StackBlock::new(|_url: NonNull<NSURL>| {
                let result = self.inner.open(rel).and_then(|mut file| {
                    let mut bytes = Vec::new();
                    file.read_to_end(&mut bytes)
                        .map(|_| bytes)
                        .map_err(|e| DirError::Io(e.to_string()))
                });
                *read.borrow_mut() = result;
            });
            let mut error: Option<Retained<NSError>> = None;
            coordinator.coordinateReadingItemAtURL_options_error_byAccessor(
                &url,
                NSFileCoordinatorReadingOptions(0),
                Some(&mut error),
                &reader,
            );
            if let Some(error) = error {
                return Err(DirError::Io(error.localizedDescription().to_string()));
            }
            read.into_inner()
        })
    }
}

impl Dir for IosDir {
    /// `StdDir`'s listing, with iCloud placeholders listed as the files they stand for
    /// (size 0 and no time until downloaded, so the next sync after the download sends
    /// the file again with its real stamp).
    fn list(&self, rel: &str) -> Result<Vec<DirEntry>, DirError> {
        let entries = self.inner.list(rel)?;
        let mut out: Vec<DirEntry> = Vec::with_capacity(entries.len());
        let mut stand_ins = Vec::new();
        for entry in entries {
            match placeholder_for(&entry.name) {
                Some(name) if entry.kind == DirKind::File => stand_ins.push(name.to_string()),
                _ => out.push(entry),
            }
        }
        for name in stand_ins {
            if !out.iter().any(|e| e.name == name) {
                out.push(DirEntry {
                    name,
                    kind: DirKind::File,
                    bytes: 0,
                    modified: None,
                });
            }
        }
        Ok(out)
    }

    fn stat(&self, rel: &str) -> Result<DirEntry, DirError> {
        match self.inner.stat(rel) {
            Err(DirError::NotFound) if self.is_placeholder(rel) => Ok(DirEntry {
                name: split_rel(rel).1.to_string(),
                kind: DirKind::File,
                bytes: 0,
                modified: None,
            }),
            other => other,
        }
    }

    fn open(&self, rel: &str) -> Result<Box<dyn Read + '_>, DirError> {
        match self.inner.stat(rel) {
            Ok(entry) if entry.kind == DirKind::Dir => return Err(DirError::IsADir),
            Ok(_) => {}
            Err(DirError::NotFound) if self.is_placeholder(rel) => {}
            Err(error) => return Err(error),
        }
        Ok(Box::new(Cursor::new(self.coordinated_read(rel)?)))
    }
}

/// "iPhone" or "iPad", for the messages that say where to attach the folder again.
fn device_name() -> &'static str {
    if machine().is_some_and(|m| m.starts_with("iPad")) {
        "iPad"
    } else {
        "iPhone"
    }
}

/// The device model ("iPhone16,1", "iPad14,3"); on the simulator, the simulated one.
/// Safe from any thread (no UIKit), as `open` runs on a blocking thread.
fn machine() -> Option<String> {
    if let Ok(model) = std::env::var("SIMULATOR_MODEL_IDENTIFIER") {
        return Some(model);
    }
    unsafe extern "C" {
        fn sysctlbyname(
            name: *const std::ffi::c_char,
            oldp: *mut std::ffi::c_void,
            oldlenp: *mut usize,
            newp: *mut std::ffi::c_void,
            newlen: usize,
        ) -> std::ffi::c_int;
    }
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    // SAFETY: a NUL-terminated name, and a buffer with its length.
    let status = unsafe {
        sysctlbyname(
            c"hw.machine".as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (status == 0).then(|| {
        let text = &buf[..len.min(buf.len())];
        let end = text.iter().position(|b| *b == 0).unwrap_or(text.len());
        String::from_utf8_lossy(&text[..end]).into_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_name_the_files_they_stand_for() {
        assert_eq!(placeholder_for(".paper.pdf.icloud"), Some("paper.pdf"));
        assert_eq!(placeholder_for(".icloud"), None);
        assert_eq!(placeholder_for("paper.pdf"), None);
        assert_eq!(placeholder_of("paper.pdf"), ".paper.pdf.icloud");
        assert_eq!(split_rel("notes/ch1.md"), ("notes", "ch1.md"));
        assert_eq!(split_rel("ch1.md"), ("", "ch1.md"));
    }

    #[test]
    fn revoked_messages_name_the_folder() {
        let DirError::Revoked(message) = revoked(Some("notes")) else {
            panic!()
        };
        assert!(message.starts_with("Margin no longer has access to the folder “notes”;"));
        assert!(message.ends_with(&format!("on this {}.", device_name())));
    }
}
