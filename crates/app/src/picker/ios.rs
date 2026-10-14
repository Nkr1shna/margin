//! iOS and iPadOS: `UIDocumentPickerViewController`, presented over the app's view
//! controller. Files and folders are opened in place, so each picked URL is a security
//! scope: inside it the folder is walked like a desktop folder and the chosen files are
//! copied (with file coordination, which downloads cloud files first) into the staging
//! folder.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

use block2::StackBlock;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_foundation::{
    NSArray, NSError, NSFileCoordinator, NSFileCoordinatorReadingOptions, NSObject,
    NSObjectProtocol, NSString, NSURL,
};
use objc2_ui_kit::{UIDocumentPickerDelegate, UIDocumentPickerViewController, UIViewController};
use objc2_uniform_type_identifiers::{UTTypeData, UTTypeFolder};
use tokio::sync::oneshot;

use crate::sources::{Selection, select_files, select_folder};

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

/// Show the picker; the delegate must live until it replies.
fn present(
    mtm: MainThreadMarker,
    folder: bool,
) -> Option<(Retained<Delegate>, oneshot::Receiver<Picked>)> {
    let host = top_view_controller()?;
    // SAFETY: UTType constants from UniformTypeIdentifiers, always initialized.
    let kind = unsafe { if folder { UTTypeFolder } else { UTTypeData } };
    let types = NSArray::from_slice(&[kind]);
    let picker = UIDocumentPickerViewController::initForOpeningContentTypes(
        UIDocumentPickerViewController::alloc(mtm),
        &types,
    );
    picker.setAllowsMultipleSelection(!folder);
    let (tx, rx) = oneshot::channel();
    let delegate = Delegate::new(mtm, tx);
    picker.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    host.presentViewController_animated_completion(&picker, true, None);
    Some((delegate, rx))
}

pub async fn pick(folder: bool) -> Option<Selection> {
    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("the document picker can only be shown from the main thread");
        return None;
    };
    let Some((delegate, reply)) = present(mtm, folder) else {
        eprintln!("couldn't find a view controller to show the document picker over");
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
            let selection = match (folder, paths.first()) {
                (true, Some(root)) => select_folder(root),
                _ => select_files(paths),
            };
            selection.stage(&dir, |from, to| coordinated_copy(from, to))
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
