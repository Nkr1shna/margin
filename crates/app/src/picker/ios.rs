//! iOS and iPadOS: `UIDocumentPickerViewController`, presented over the app's view
//! controller. Files are opened in place, so each picked URL is a security scope: inside
//! it the chosen files are copied (with file coordination, which downloads cloud files
//! first) into the staging folder. A picked folder is kept, not copied: inside its scope
//! a bookmark is made (it carries the scope, so the folder can be opened again on later
//! launches; `folders::ios` resolves it), with the folder's name and where it is.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

use base64::Engine as _;
use block2::StackBlock;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_foundation::{
    NSArray, NSError, NSFileCoordinator, NSFileCoordinatorReadingOptions, NSObject,
    NSObjectProtocol, NSString, NSURL, NSURLBookmarkCreationOptions,
};
use objc2_ui_kit::{UIDocumentPickerDelegate, UIDocumentPickerViewController, UIViewController};
use objc2_uniform_type_identifiers::{UTType, UTTypeData, UTTypeFolder};
use tokio::sync::oneshot;

use crate::folders::{FolderHandle, FolderPick};
use crate::sources::{Selection, select_files};

/// What the picker hands back: the picked URLs, empty when it was cancelled.
type Picked = Vec<Retained<NSURL>>;
type Reply = oneshot::Sender<Picked>;

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and `Delegate` doesn't implement
    // `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MarginDocumentPickerDelegate"]
    #[ivars = RefCell<Option<Reply>>]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl UIDocumentPickerDelegate for Delegate {
        #[unsafe(method(documentPicker:didPickDocumentsAtURLs:))]
        fn did_pick(&self, _picker: &UIDocumentPickerViewController, urls: &NSArray<NSURL>) {
            self.reply(urls.to_vec());
        }

        #[unsafe(method(documentPickerWasCancelled:))]
        fn was_cancelled(&self, _picker: &UIDocumentPickerViewController) {
            self.reply(Vec::new());
        }
    }
);

impl Delegate {
    fn new(mtm: MainThreadMarker, reply: Reply) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(RefCell::new(Some(reply)));
        // SAFETY: NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }

    fn reply(&self, urls: Picked) {
        if let Some(reply) = self.ivars().borrow_mut().take() {
            let _ = reply.send(urls);
        }
    }
}

/// The view controller to present over: the app window's, or whatever it presents.
fn top_view_controller() -> Option<Retained<UIViewController>> {
    use dioxus_host::tao::platform::ios::WindowExtIOS;
    let pointer = dioxus_host::window().window.ui_view_controller();
    // SAFETY: tao hands out its window's live UIViewController; retaining it keeps it
    // alive while it's used here.
    let mut top = unsafe { Retained::retain(pointer.cast::<UIViewController>()) }?;
    while let Some(presented) = top.presentedViewController() {
        top = presented;
    }
    Some(top)
}

#[cfg(feature = "desktop")]
use dioxus::desktop as dioxus_host;
#[cfg(not(feature = "desktop"))]
use dioxus::mobile as dioxus_host;

/// Show the picker for `kind`, opened in place; the delegate must live until it replies.
fn present(
    mtm: MainThreadMarker,
    kind: &UTType,
    multiple: bool,
) -> Option<(Retained<Delegate>, oneshot::Receiver<Picked>)> {
    let host = top_view_controller()?;
    let types = NSArray::from_slice(&[kind]);
    let picker = UIDocumentPickerViewController::initForOpeningContentTypes(
        UIDocumentPickerViewController::alloc(mtm),
        &types,
    );
    picker.setAllowsMultipleSelection(multiple);
    let (tx, rx) = oneshot::channel();
    let delegate = Delegate::new(mtm, tx);
    picker.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    host.presentViewController_animated_completion(&picker, true, None);
    Some((delegate, rx))
}

/// Ask for one folder and keep it: inside its security scope, a bookmark (which carries
/// the scope), its name and where it is ("iCloud Drive/notes", "On My iPhone/notes");
/// the scope is stopped before this returns. `None` when the picker was cancelled or
/// the folder can't be kept (the reason is logged).
pub async fn pick_folder() -> Option<FolderPick> {
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::warn!("the folder picker can only be shown from the main thread");
        return None;
    };
    // SAFETY: a UTType constant from UniformTypeIdentifiers, always initialized.
    let kind = unsafe { UTTypeFolder };
    let Some((delegate, reply)) = present(mtm, kind, false) else {
        tracing::warn!("couldn't find a view controller to show the folder picker over");
        return None;
    };
    let urls = reply.await.unwrap_or_default();
    drop(delegate);
    let url = urls.into_iter().next()?;
    match keep_folder(&url) {
        Ok(pick) => Some(pick),
        Err(error) => {
            tracing::warn!("couldn't keep the picked folder: {error}");
            None
        }
    }
}

/// The bookmark, name and location of a picked folder, made inside its scope.
fn keep_folder(url: &NSURL) -> Result<FolderPick, String> {
    // SAFETY: balanced below, whatever happens in between.
    let started = unsafe { url.startAccessingSecurityScopedResource() };
    let kept = autoreleasepool(|_| {
        let path = url
            .path()
            .map(|p| p.to_string())
            .ok_or("The folder has no path.")?;
        // A plain bookmark: on iOS one made inside the scope carries it (there is no
        // `WithSecurityScope` option here).
        let data = url
            .bookmarkDataWithOptions_includingResourceValuesForKeys_relativeToURL_error(
                NSURLBookmarkCreationOptions(0),
                None,
                None,
            )
            .map_err(|e| e.localizedDescription().to_string())?;
        let location = location_of(&path, device_name());
        let name = location
            .rsplit('/')
            .next()
            .filter(|n| !n.is_empty() && *n != "…")
            .map(str::to_string)
            .or_else(|| url.lastPathComponent().map(|n| n.to_string()))
            .unwrap_or_else(|| "Folder".into());
        Ok(FolderPick {
            name,
            location,
            handle: FolderHandle::IosBookmark {
                bookmark_b64: base64::engine::general_purpose::STANDARD.encode(data.to_vec()),
            },
        })
    });
    if started {
        // SAFETY: started above.
        unsafe { url.stopAccessingSecurityScopedResource() };
    }
    kept
}

/// "iPhone" or "iPad", as the Files app names this device's own storage ("On My iPad").
fn device_name() -> &'static str {
    if machine().is_some_and(|m| m.starts_with("iPad")) {
        "iPad"
    } else {
        "iPhone"
    }
}

/// The device model ("iPhone16,1", "iPad14,3"); on the simulator, the simulated one.
/// Safe from any thread (no UIKit).
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

/// Where a picked folder is, as the Files app shows it, from its path: "iCloud
/// Drive/notes", "iCloud Drive/Pages/drafts" (an app's iCloud folder), "On My
/// iPhone/notes", "On My iPhone/Margin/notes" (this app's own Documents), a cloud
/// provider's name and its folders, or the path's last two names when nothing matches.
fn location_of(path: &str, device: &str) -> String {
    let path = path.trim_end_matches('/');
    let join = |head: String, rest: &str| {
        let rest = rest.trim_matches('/');
        if rest.is_empty() {
            head
        } else {
            format!("{head}/{rest}")
        }
    };
    if let Some((_, rest)) = path.split_once("/Mobile Documents/") {
        let (container, rest) = rest.split_once('/').unwrap_or((rest, ""));
        if container == "com~apple~CloudDocs" {
            return join("iCloud Drive".into(), rest);
        }
        // An app's own iCloud folder: "iCloud~com~apple~Pages/Documents/…".
        let app = container.rsplit('~').next().unwrap_or(container);
        let rest = rest.strip_prefix("Documents").unwrap_or(rest);
        return join(format!("iCloud Drive/{app}"), rest);
    }
    if let Some((_, rest)) = path.split_once("/File Provider Storage") {
        return join(format!("On My {device}"), rest);
    }
    if let Some((_, rest)) = path.split_once("/Library/CloudStorage/") {
        // "GoogleDrive-k@example.com/My Drive/notes": the provider, then the folders.
        let (account, rest) = rest.split_once('/').unwrap_or((rest, ""));
        let provider = account.split('-').next().unwrap_or(account);
        return join(provider.to_string(), rest);
    }
    if let Some((_, rest)) = path.split_once("/Containers/Data/Application/") {
        // An app's Documents, shown in Files under the app's name.
        let (app_dir, rest) = rest.split_once('/').unwrap_or((rest, ""));
        let rest = rest.strip_prefix("Documents").unwrap_or(rest);
        let ours = std::env::var("HOME").is_ok_and(|home| home.ends_with(app_dir));
        let app = if ours { "Margin" } else { "…" };
        return join(format!("On My {device}/{app}"), rest);
    }
    let mut names = path.rsplit('/').filter(|n| !n.is_empty());
    let last = names.next().unwrap_or_default();
    match names.next() {
        Some(parent) => format!("{parent}/{last}"),
        None => last.to_string(),
    }
}

pub async fn pick_files() -> Option<Selection> {
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::warn!("the document picker can only be shown from the main thread");
        return None;
    };
    // SAFETY: a UTType constant from UniformTypeIdentifiers, always initialized.
    let kind = unsafe { UTTypeData };
    let Some((delegate, reply)) = present(mtm, kind, true) else {
        tracing::warn!("couldn't find a view controller to show the document picker over");
        return None;
    };
    let urls = reply.await.unwrap_or_default();
    drop(delegate);
    if urls.is_empty() {
        return None;
    }

    // SAFETY: balanced below, after the files are copied.
    let scoped: Vec<(Retained<NSURL>, bool)> = urls
        .into_iter()
        .map(|url| {
            let started = unsafe { url.startAccessingSecurityScopedResource() };
            (url, started)
        })
        .collect();
    let paths: Vec<PathBuf> = scoped
        .iter()
        .filter_map(|(url, _)| url.path())
        .map(|path| PathBuf::from(path.to_string()))
        .collect();
    let staged = match super::new_staging_dir() {
        Some(dir) => tokio::task::spawn_blocking(move || {
            select_files(paths).stage(&dir, |from, to| coordinated_copy(from, to))
        })
        .await
        .ok(),
        None => None,
    };
    for (url, started) in scoped {
        if started {
            // SAFETY: started above.
            unsafe { url.stopAccessingSecurityScopedResource() };
        }
    }
    staged
}

/// Copy `from` to `to` under a coordinated read, so a file that's only in iCloud is
/// downloaded first and a file being written is read whole.
fn coordinated_copy(from: &Path, to: &Path) -> Result<(), String> {
    autoreleasepool(|_| {
        let url = NSURL::fileURLWithPath(&NSString::from_str(&from.to_string_lossy()));
        let coordinator = NSFileCoordinator::new();
        let copied = RefCell::new(Err("The file wasn't read.".to_string()));
        let reader = StackBlock::new(|url: NonNull<NSURL>| {
            // SAFETY: the coordinator passes a valid URL for the duration of the block.
            let url = unsafe { url.as_ref() };
            let result = match url.path() {
                Some(path) => std::fs::copy(path.to_string(), to)
                    .map(|_| ())
                    .map_err(|e| e.to_string()),
                None => Err("The file has no path.".to_string()),
            };
            *copied.borrow_mut() = result;
        });
        let mut error: Option<Retained<NSError>> = None;
        coordinator.coordinateReadingItemAtURL_options_error_byAccessor(
            &url,
            NSFileCoordinatorReadingOptions(0),
            Some(&mut error),
            &reader,
        );
        if let Some(error) = error {
            return Err(error.localizedDescription().to_string());
        }
        copied.into_inner()
    })
}

#[cfg(test)]
mod tests {
    use super::location_of;

    #[test]
    fn locations_read_as_the_files_app_shows_them() {
        let cloud = "/private/var/mobile/Library/Mobile Documents/com~apple~CloudDocs";
        assert_eq!(
            location_of(&format!("{cloud}/notes"), "iPhone"),
            "iCloud Drive/notes"
        );
        assert_eq!(location_of(&format!("{cloud}/"), "iPhone"), "iCloud Drive");
        assert_eq!(
            location_of(
                "/private/var/mobile/Library/Mobile Documents/iCloud~com~apple~Pages/Documents/drafts",
                "iPad"
            ),
            "iCloud Drive/Pages/drafts"
        );
        assert_eq!(
            location_of(
                "/private/var/mobile/Containers/Shared/AppGroup/0A1B/File Provider Storage/school/notes",
                "iPad"
            ),
            "On My iPad/school/notes"
        );
        assert_eq!(
            location_of(
                "/private/var/mobile/Library/CloudStorage/GoogleDrive-k@example.com/My Drive/notes",
                "iPhone"
            ),
            "GoogleDrive/My Drive/notes"
        );
        assert_eq!(location_of("/var/folders/x/notes", "iPhone"), "x/notes");
    }
}
