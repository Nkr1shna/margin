//! The sync engine: walk, manifest, uploads, triggers, pause and resume (4-core,
//! `docs/agent-phase4-brief.md` D6, with the device-side hashing of
//! `docs/agent-rag-architecture.md` §2.7).
//!
//! One run per folder at a time; a second request while one runs is coalesced into "run
//! again when done". A run:
//! 1. **Scans** on a blocking thread: opens the folder (`folders::open`), walks it
//!    depth-first in name order through `Dir::list`, passing over hidden and
//!    secret-looking names (`is_skipped_name`) and every file
//!    `margin_shared::folder_file_type` doesn't know, and hashes each file it takes
//!    (SHA-256, streamed) for its `FileStamp`.
//! 2. Sends the **manifest** (`Api::sync_workspace`) and gets the plan back.
//! 3. **Uploads** the plan's files in order, one at a time, each read through `Dir::open`
//!    on its own thread (the folder is opened again for each file, so iOS holds the
//!    security scope only while reading): small files in one request, larger ones in
//!    parts that a later run resumes. A file that vanished or changed since the scan is
//!    passed over (the next run sends it), and so is one whose bytes no longer hash to
//!    the stamp.
//! 4. Ends **idle**, or **failed** with the first error (the other files still tried).
//!
//! Folder uploads don't go through the composer's upload queue (`AppCtx::uploads`): a
//! folder of hundreds of files would hold up the files of a chat behind it.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Utc};
use dioxus::prelude::{ReadableExt, WritableExt};
use futures_util::future::{AbortHandle, Abortable};
use margin_shared::{
    Event, FileStamp, Id, ManifestFile, NewWorkspace, Platform, SyncManifest, SyncPlan,
    UnreadableFile, UnreadableReason, Workspace, folder_file_type, is_skipped_name, routes,
};
use tokio::sync::{oneshot, watch};

use super::store::{self, LocalFolder, Shared};
use super::{Dir, DirEntry, DirError, DirKind, FolderHandle};
use crate::api::{Api, ApiFailure, PART_RETRIES, retry_part};
use crate::state::AppCtx;
use crate::tools::LocalTask;

/// What a folder's sync is doing, for the Folders pane and the chat's strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncState {
    /// Walking the folder and hashing its files.
    Scanning,
    /// Sending the files the service asked for.
    Uploading {
        done: u32,
        total: u32,
        bytes_done: u64,
        bytes_total: u64,
    },
    /// The last run finished then.
    Idle {
        last: DateTime<Utc>,
    },
    /// iOS, iPadOS, Android: Margin went to the background; the sync goes on when it's
    /// back.
    Paused,
    Failed(String),
}

/// What the answer to `SyncFolder` says about a run: the folder's name and the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub name: String,
    pub plan: SyncPlan,
}

impl Outcome {
    /// "Synced “notes”: 3 files to send, 1 removed; sending now."
    pub fn message(&self) -> String {
        let send = self.plan.upload.len();
        let removed = match self.plan.removed {
            0 => String::new(),
            n => format!(", {n} removed"),
        };
        match send {
            0 => format!(
                "Synced “{}”: nothing to send{removed}; the service has every file.",
                self.name
            ),
            n => format!(
                "Synced “{}”: {}{removed}; sending now.",
                self.name,
                files(n as u64, "to send")
            ),
        }
    }
}

fn files(n: u64, what: &str) -> String {
    if n == 1 {
        format!("1 file {what}")
    } else {
        format!("{n} files {what}")
    }
}

/// What `SyncFolder` and friends answer for a folder this device doesn't hold.
pub const NOT_HELD: &str = "This device doesn't hold the folder with that id.";
/// What they answer while Margin is in the background (mobile).
pub const PAUSED: &str =
    "Margin is in the background on this device; the folder syncs once it's open again.";

/// Opens a held folder (`folders::open` in the app, a test folder in tests).
pub type Opener = Arc<dyn Fn(&FolderHandle) -> Result<Box<dyn Dir>, DirError> + Send + Sync>;

/// The sync engine for the folders of one store. Single-threaded (the app's UI thread);
/// cheap to clone.
#[derive(Clone)]
pub struct Engine(Rc<Inner>);

struct Inner {
    store: Shared,
    opener: Opener,
    spawn: Box<dyn Fn(LocalTask)>,
    api: RefCell<Option<Api>>,
    runs: RefCell<BTreeMap<Id, Run>>,
    paused: Cell<bool>,
    /// Uploads in parts started and not finished, by folder and path, so a later run
    /// goes on from what the service has.
    parts: RefCell<HashMap<(Id, String), Pending>>,
    states: watch::Sender<BTreeMap<Id, SyncState>>,
}

/// A run of one folder's sync.
struct Run {
    /// Stops the current pass (pause, removal, a restart).
    cancel: Arc<AtomicBool>,
    /// Run once more when this pass ends.
    again: bool,
    /// The run should end (paused, or the folder removed) rather than go again.
    stopped: bool,
    /// Whether the current pass has sent its manifest.
    manifest_sent: bool,
    /// The upload request in flight.
    request: Option<AbortHandle>,
    /// `SyncFolder` requests waiting for the next manifest's plan.
    waiters: Vec<oneshot::Sender<Result<Outcome, String>>>,
}

#[derive(Debug, Clone)]
struct Pending {
    upload_id: Id,
    stamp: FileStamp,
}

/// How a pass ended.
enum PassEnd {
    Done(Result<DateTime<Utc>, String>),
    /// Cancelled (pause, removal or a restart).
    Stopped,
    /// The service no longer has the folder.
    Gone,
}

/// How one file's upload went.
enum Delivery {
    Sent,
    /// Vanished or changed since the scan: the next run takes it.
    Skipped,
    Cancelled,
    Revoked(String),
    Failed(String),
}

impl Engine {
    pub fn new(store: Shared, opener: Opener, spawn: impl Fn(LocalTask) + 'static) -> Self {
        // A stale bookmark made again while opening is saved in place of the old one.
        let opener: Opener = {
            let store = store.clone();
            Arc::new(move |handle: &FolderHandle| {
                let opened = opener(handle);
                for (old, new) in super::take_resaved() {
                    if let Err(e) = store::lock(&store).update_handle(&old, new) {
                        eprintln!("couldn't save a folder's renewed bookmark: {e}");
                    }
                }
                opened
            })
        };
        Self(Rc::new(Inner {
            store,
            opener,
            spawn: Box::new(spawn),
            api: RefCell::new(None),
            runs: RefCell::new(BTreeMap::new()),
            paused: Cell::new(false),
            parts: RefCell::new(HashMap::new()),
            states: watch::Sender::new(BTreeMap::new()),
        }))
    }

    /// The client to sync with; `None` (signed out) stops every run.
    pub fn set_api(&self, api: Option<Api>) {
        let signed_out = api.is_none();
        *self.0.api.borrow_mut() = api;
        if signed_out {
            let ids: Vec<Id> = self.0.runs.borrow().keys().cloned().collect();
            for id in ids {
                self.stop(&id);
            }
            self.0.parts.borrow_mut().clear();
            self.0.states.send_replace(BTreeMap::new());
        }
    }

    /// Follows every folder's state.
    pub fn states(&self) -> watch::Receiver<BTreeMap<Id, SyncState>> {
        self.0.states.subscribe()
    }

    pub fn state(&self, id: &str) -> Option<SyncState> {
        self.0.states.borrow().get(id).cloned()
    }

    fn set_state(&self, id: &str, state: SyncState) {
        self.0.states.send_if_modified(|states| {
            if states.get(id) == Some(&state) {
                return false;
            }
            states.insert(id.to_string(), state);
            true
        });
    }

    fn api(&self) -> Option<Api> {
        self.0.api.borrow().clone()
    }

    fn folder(&self, id: &str) -> Option<LocalFolder> {
        store::lock(&self.0.store).get(id)
    }

    pub fn running(&self, id: &str) -> bool {
        self.0.runs.borrow().contains_key(id)
    }

    /// Sync a held folder now, or once more after the run going on.
    pub fn start(&self, id: &str) {
        self.begin(id, None);
    }

    /// Sync every held folder.
    pub fn sync_all(&self) {
        let ids = store::lock(&self.0.store).ids();
        for id in ids {
            self.start(&id);
        }
    }

    /// Sync now for `SyncFolder`: the answer is the next manifest's plan. A run already
    /// sending files stops (its parts resume) and starts over, so the plan is fresh.
    pub fn request(&self, id: &str) -> oneshot::Receiver<Result<Outcome, String>> {
        let (tx, rx) = oneshot::channel();
        if self.0.paused.get() {
            let _ = tx.send(Err(PAUSED.into()));
            return rx;
        }
        if self.folder(id).is_none() {
            let _ = tx.send(Err(NOT_HELD.into()));
            return rx;
        }
        self.begin(id, Some(tx));
        rx
    }

    fn begin(&self, id: &str, waiter: Option<oneshot::Sender<Result<Outcome, String>>>) {
        if self.folder(id).is_none() || self.api().is_none() {
            if let Some(tx) = waiter {
                let _ = tx.send(Err(NOT_HELD.into()));
            }
            return;
        }
        if self.0.paused.get() {
            self.set_state(id, SyncState::Paused);
            if let Some(tx) = waiter {
                let _ = tx.send(Err(PAUSED.into()));
            }
            return;
        }
        {
            let mut runs = self.0.runs.borrow_mut();
            if let Some(run) = runs.get_mut(id) {
                run.again = true;
                run.stopped = false;
                if let Some(tx) = waiter {
                    run.waiters.push(tx);
                    if run.manifest_sent {
                        // Its plan is old: stop sending and take a new manifest.
                        run.cancel.store(true, Ordering::Relaxed);
                        if let Some(request) = run.request.take() {
                            request.abort();
                        }
                    }
                }
                return;
            }
            runs.insert(
                id.to_string(),
                Run {
                    cancel: Arc::new(AtomicBool::new(false)),
                    again: false,
                    stopped: false,
                    manifest_sent: false,
                    request: None,
                    waiters: waiter.into_iter().collect(),
                },
            );
        }
        let engine = self.clone();
        let id = id.to_string();
        (self.0.spawn)(Box::pin(async move { engine.drive(id).await }));
    }

    /// Stop a run where it is: its pass ends at the next check, its request is dropped.
    fn stop(&self, id: &str) {
        let mut runs = self.0.runs.borrow_mut();
        if let Some(run) = runs.get_mut(id) {
            run.stopped = true;
            run.again = false;
            run.cancel.store(true, Ordering::Relaxed);
            if let Some(request) = run.request.take() {
                request.abort();
            }
        }
    }

    /// Margin went to the background (iOS, iPadOS, Android): every run stops, its
    /// parts left to resume.
    pub fn pause(&self) {
        self.0.paused.set(true);
        let ids: Vec<Id> = self.0.runs.borrow().keys().cloned().collect();
        for id in ids {
            self.stop(&id);
            self.set_state(&id, SyncState::Paused);
        }
    }

    /// Back in front: new runs may start (the caller starts them).
    pub fn resume(&self) {
        self.0.paused.set(false);
    }

    /// The folder was removed: its run stops, its state goes and so do its unfinished
    /// parts (the service drops those after a day; they are cancelled here too).
    pub fn forget(&self, id: &str) {
        self.stop(id);
        self.0
            .states
            .send_if_modified(|states| states.remove(id).is_some());
        let pending: Vec<Pending> = {
            let mut parts = self.0.parts.borrow_mut();
            let keys: Vec<(Id, String)> = parts.keys().filter(|(w, _)| w == id).cloned().collect();
            keys.into_iter().filter_map(|k| parts.remove(&k)).collect()
        };
        if let (Some(api), false) = (self.api(), pending.is_empty()) {
            (self.0.spawn)(Box::pin(async move {
                for p in pending {
                    let _ = api.cancel_source_upload(&p.upload_id).await;
                }
            }));
        }
    }

    async fn drive(self, id: Id) {
        loop {
            let cancel = Arc::new(AtomicBool::new(false));
            if let Some(run) = self.0.runs.borrow_mut().get_mut(&id) {
                run.cancel = cancel.clone();
                run.again = false;
                run.manifest_sent = false;
            }
            let end = self.pass(&id, &cancel).await;
            let (again, stopped, waiters) = {
                let mut runs = self.0.runs.borrow_mut();
                let Some(run) = runs.get_mut(&id) else {
                    return;
                };
                let again = run.again && !run.stopped && !self.0.paused.get();
                let waiters = if again {
                    Vec::new()
                } else {
                    std::mem::take(&mut run.waiters)
                };
                (again, run.stopped, waiters)
            };
            let leftover = match end {
                PassEnd::Done(Ok(last)) => {
                    self.set_state(&id, SyncState::Idle { last });
                    "The sync ended before the service answered.".to_string()
                }
                PassEnd::Done(Err(message)) => {
                    self.set_state(&id, SyncState::Failed(message.clone()));
                    message
                }
                PassEnd::Stopped if self.0.paused.get() => {
                    self.set_state(&id, SyncState::Paused);
                    PAUSED.to_string()
                }
                PassEnd::Stopped => "The sync stopped.".to_string(),
                PassEnd::Gone => {
                    self.0.runs.borrow_mut().remove(&id);
                    for tx in waiters {
                        let _ = tx.send(Err(NOT_HELD.into()));
                    }
                    self.forget(&id);
                    let removed = store::lock(&self.0.store).remove(&id);
                    if let Some(folder) = removed {
                        let handle = folder.handle;
                        let _ = tokio::task::spawn_blocking(move || super::forget(&handle)).await;
                    }
                    return;
                }
            };
            if again {
                continue;
            }
            for tx in waiters {
                let _ = tx.send(Err(leftover.clone()));
            }
            self.0.runs.borrow_mut().remove(&id);
            let _ = stopped;
            return;
        }
    }

    async fn pass(&self, id: &str, cancel: &Arc<AtomicBool>) -> PassEnd {
        let Some(folder) = self.folder(id) else {
            return PassEnd::Gone;
        };
        let Some(api) = self.api() else {
            return PassEnd::Stopped;
        };
        self.set_state(id, SyncState::Scanning);
        let found = match scan_blocking(self.0.opener.clone(), &folder, cancel.clone()).await {
            Ok(found) => found,
            Err(ScanError::Cancelled) => return PassEnd::Stopped,
            Err(ScanError::Dir(DirError::Revoked(message))) => {
                let _ = store::lock(&self.0.store).mark_revoked(id, &message);
                return PassEnd::Done(Err(message));
            }
            Err(ScanError::Dir(e)) => {
                return PassEnd::Done(Err(format!(
                    "Couldn't read the folder “{}”: {e}",
                    folder.name
                )));
            }
        };
        let _ = store::lock(&self.0.store).clear_revoked(id);
        if cancel.load(Ordering::Relaxed) {
            return PassEnd::Stopped;
        }
        let manifest = SyncManifest {
            files: found.files,
            unreadable: found.unreadable,
        };
        let plan = match api.sync_workspace(id, &manifest).await {
            Ok(plan) => plan,
            Err(e) if e.status == Some(404) => return PassEnd::Gone,
            Err(e) => return PassEnd::Done(Err(e.message)),
        };
        let _ = store::lock(&self.0.store).touch_synced(id);
        let waiters = match self.0.runs.borrow_mut().get_mut(id) {
            Some(run) => {
                run.manifest_sent = true;
                std::mem::take(&mut run.waiters)
            }
            None => Vec::new(),
        };
        let outcome = Outcome {
            name: folder.name.clone(),
            plan: plan.clone(),
        };
        for tx in waiters {
            let _ = tx.send(Ok(outcome.clone()));
        }

        let stamps: HashMap<&str, &ManifestFile> = manifest
            .files
            .iter()
            .map(|f| (f.path.as_str(), f))
            .collect();
        let todo: Vec<&ManifestFile> = plan
            .upload
            .iter()
            .filter_map(|path| stamps.get(path.as_str()).copied())
            .collect();
        let total = todo.len() as u32;
        let bytes_total: u64 = todo.iter().map(|f| f.stamp.bytes).sum();
        let mut bytes_done = 0u64;
        let mut first_error = None;
        for (done, file) in (0u32..).zip(todo) {
            if cancel.load(Ordering::Relaxed) {
                return PassEnd::Stopped;
            }
            self.set_state(
                id,
                SyncState::Uploading {
                    done,
                    total,
                    bytes_done,
                    bytes_total,
                },
            );
            let progress = {
                let engine = self.clone();
                let id = id.to_string();
                move |sent: u64| {
                    engine.set_state(
                        &id,
                        SyncState::Uploading {
                            done,
                            total,
                            bytes_done: bytes_done + sent,
                            bytes_total,
                        },
                    );
                }
            };
            match self
                .send_file(&api, id, &folder, &file.path, &file.stamp, cancel, progress)
                .await
            {
                Delivery::Sent | Delivery::Skipped => {}
                Delivery::Cancelled => return PassEnd::Stopped,
                Delivery::Revoked(message) => {
                    let _ = store::lock(&self.0.store).mark_revoked(id, &message);
                    return PassEnd::Done(Err(message));
                }
                Delivery::Failed(message) => {
                    first_error
                        .get_or_insert_with(|| format!("Couldn't send “{}”: {message}", file.path));
                }
            }
            bytes_done += file.stamp.bytes;
        }
        PassEnd::Done(first_error.map_or_else(|| Ok(Utc::now()), Err))
    }

    /// Walk a held folder and answer its manifest (`DeviceToolCall::Manifest`).
    pub async fn manifest(&self, id: &str) -> Result<Vec<ManifestFile>, String> {
        let folder = self.folder(id).ok_or_else(|| NOT_HELD.to_string())?;
        match scan_blocking(
            self.0.opener.clone(),
            &folder,
            Arc::new(AtomicBool::new(false)),
        )
        .await
        {
            Ok(found) => {
                let _ = store::lock(&self.0.store).clear_revoked(id);
                Ok(found.files)
            }
            Err(ScanError::Dir(DirError::Revoked(message))) => {
                let _ = store::lock(&self.0.store).mark_revoked(id, &message);
                Err(message)
            }
            Err(ScanError::Dir(e)) => {
                Err(format!("Couldn't read the folder “{}”: {e}", folder.name))
            }
            Err(ScanError::Cancelled) => Err("The walk was stopped.".into()),
        }
    }

    /// Upload one file the service asked for (`DeviceToolCall::FetchFile`), if it still
    /// has the stamp the service expects.
    pub async fn fetch(&self, id: &str, path: &str, stamp: &FileStamp) -> Result<String, String> {
        let folder = self.folder(id).ok_or_else(|| NOT_HELD.to_string())?;
        if folder_file_type(path).is_none() || margin_shared::relative_components(path).is_err() {
            return Err(format!("“{path}” isn't a file Margin takes from folders."));
        }
        let api = self
            .api()
            .ok_or_else(|| "This device isn't signed in.".to_string())?;
        let cancel = Arc::new(AtomicBool::new(false));
        match self
            .send_file(&api, id, &folder, path, stamp, &cancel, |_| {})
            .await
        {
            Delivery::Sent => Ok(format!("Sent “{}/{path}”.", folder.name)),
            Delivery::Skipped => Err(format!(
                "“{path}” changed or is gone since the service asked for it; the next sync sends what is there now."
            )),
            Delivery::Cancelled => Err("The upload was stopped.".into()),
            Delivery::Revoked(message) => {
                let _ = store::lock(&self.0.store).mark_revoked(id, &message);
                Err(message)
            }
            Delivery::Failed(message) => Err(format!("Couldn't send “{path}”: {message}")),
        }
    }

    /// Keep the abort handle of the request in flight, so a pause can drop it.
    fn track(&self, id: &str, request: Option<AbortHandle>) {
        if let Some(run) = self.0.runs.borrow_mut().get_mut(id) {
            run.request = request;
        }
    }

    /// Run `request` so a pause or a removal can drop it; `None` when it was dropped.
    async fn abortable<T>(
        &self,
        id: &str,
        cancel: &AtomicBool,
        request: impl std::future::Future<Output = T>,
    ) -> Option<T> {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let (handle, registration) = AbortHandle::new_pair();
        self.track(id, Some(handle));
        let result = Abortable::new(request, registration).await.ok();
        self.track(id, None);
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_file(
        &self,
        api: &Api,
        id: &str,
        folder: &LocalFolder,
        path: &str,
        stamp: &FileStamp,
        cancel: &Arc<AtomicBool>,
        progress: impl Fn(u64),
    ) -> Delivery {
        let reader = FileReader::spawn(self.0.opener.clone(), folder.handle.clone(), path);
        match reader.stat().await {
            Ok(entry) if same_stamp(&entry, stamp) => {}
            Ok(_) | Err(DirError::NotFound | DirError::NotADir | DirError::IsADir) => {
                return Delivery::Skipped;
            }
            Err(DirError::Revoked(message)) => return Delivery::Revoked(message),
            Err(e) => return Delivery::Failed(e.to_string()),
        }
        if stamp.bytes <= routes::UPLOAD_PART_BYTES {
            let data = match reader.read(0, stamp.bytes).await {
                Ok(data) => data,
                Err(e) => return read_failure(e),
            };
            if hex(&Sha256::digest(&data)) != stamp.sha256 {
                return Delivery::Skipped;
            }
            return match self
                .abortable(
                    id,
                    cancel,
                    api.upload_folder_file(id, path, stamp.modified, data),
                )
                .await
            {
                None => Delivery::Cancelled,
                Some(Ok(_)) => Delivery::Sent,
                Some(Err(e)) => Delivery::Failed(e.message),
            };
        }
        self.send_parts(api, id, path, stamp, cancel, &reader, progress)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_parts(
        &self,
        api: &Api,
        id: &str,
        path: &str,
        stamp: &FileStamp,
        cancel: &Arc<AtomicBool>,
        reader: &FileReader,
        progress: impl Fn(u64),
    ) -> Delivery {
        let key = (id.to_string(), path.to_string());
        let earlier = self.0.parts.borrow().get(&key).cloned();
        // Go on from an earlier run's upload of the same bytes, if the service has it.
        let mut resumed = None;
        if let Some(p) = earlier {
            if p.stamp == *stamp {
                match self
                    .abortable(id, cancel, api.source_upload_state(&p.upload_id))
                    .await
                {
                    None => return Delivery::Cancelled,
                    Some(Ok(state)) => resumed = Some(state),
                    Some(Err(_)) => {}
                }
            } else {
                let _ = api.cancel_source_upload(&p.upload_id).await;
            }
            if resumed.is_none() {
                self.0.parts.borrow_mut().remove(&key);
            }
        }
        let upload = match resumed {
            Some(upload) => upload,
            None => {
                match self
                    .abortable(
                        id,
                        cancel,
                        api.start_folder_upload(id, path, stamp.modified, stamp.bytes),
                    )
                    .await
                {
                    None => return Delivery::Cancelled,
                    Some(Ok(upload)) => upload,
                    Some(Err(e)) => return Delivery::Failed(e.message),
                }
            }
        };
        self.0.parts.borrow_mut().insert(
            key.clone(),
            Pending {
                upload_id: upload.id.clone(),
                stamp: stamp.clone(),
            },
        );
        let give_up = |sent: Delivery| {
            let api = api.clone();
            let upload_id = upload.id.clone();
            let key = key.clone();
            async move {
                self.0.parts.borrow_mut().remove(&key);
                let _ = api.cancel_source_upload(&upload_id).await;
                sent
            }
        };

        // Every byte goes through the hash once, in order, whatever was sent before.
        let mut hasher = Sha256::new();
        let mut hashed: u64 = 0;
        let mut offset = upload.received.min(stamp.bytes);
        while hashed < offset {
            if cancel.load(Ordering::Relaxed) {
                return Delivery::Cancelled;
            }
            let len = routes::UPLOAD_PART_BYTES.min(offset - hashed);
            match reader.read(hashed, len).await {
                Ok(data) => {
                    hasher.update(&data);
                    hashed += len;
                }
                Err(e) => return give_up(read_failure(e)).await,
            }
        }
        progress(offset);
        let mut failures = 0;
        while offset < stamp.bytes {
            if cancel.load(Ordering::Relaxed) {
                return Delivery::Cancelled;
            }
            let len = routes::UPLOAD_PART_BYTES.min(stamp.bytes - offset);
            let data = match reader.read(offset, len).await {
                Ok(data) => data,
                Err(e) => return give_up(read_failure(e)).await,
            };
            let end = offset + len;
            if end > hashed {
                hasher.update(&data[(hashed - offset) as usize..]);
                hashed = end;
            }
            match self
                .abortable(id, cancel, api.put_upload_part(&upload.id, offset, data))
                .await
            {
                None => return Delivery::Cancelled,
                Some(Ok(state)) => {
                    offset = state.received;
                    failures = 0;
                    progress(offset);
                }
                Some(Err(e)) if retry_part(&e) && failures < PART_RETRIES => {
                    failures += 1;
                    tokio::time::sleep(std::time::Duration::from_secs(1 << failures)).await;
                    if let Ok(state) = api.source_upload_state(&upload.id).await {
                        offset = state.received;
                    }
                }
                Some(Err(e)) => return give_up(Delivery::Failed(e.message)).await,
            }
        }
        if hex(&hasher.finish()) != stamp.sha256 {
            return give_up(Delivery::Skipped).await;
        }
        let finished = self
            .abortable(id, cancel, api.finish_source_upload(&upload.id))
            .await;
        match finished {
            None => Delivery::Cancelled,
            Some(Ok(_)) => {
                self.0.parts.borrow_mut().remove(&key);
                Delivery::Sent
            }
            Some(Err(e)) => give_up(Delivery::Failed(failure_text(&e))).await,
        }
    }
}

fn failure_text(e: &ApiFailure) -> String {
    e.message.clone()
}

fn read_failure(e: DirError) -> Delivery {
    match e {
        DirError::NotFound | DirError::IsADir | DirError::NotADir => Delivery::Skipped,
        DirError::Revoked(message) => Delivery::Revoked(message),
        e => Delivery::Failed(e.to_string()),
    }
}

/// Whether a file still looks as it did in the scan (its hash is checked as it is read).
fn same_stamp(entry: &DirEntry, stamp: &FileStamp) -> bool {
    entry.kind == DirKind::File
        && entry.bytes == stamp.bytes
        && (stamp.modified.is_none() || entry.modified == stamp.modified)
}

// ---------------------------------------------------------------------------
// The app's engine
// ---------------------------------------------------------------------------

thread_local! {
    static ENGINE: Engine = Engine::new(
        store::global().clone(),
        Arc::new(super::open),
        |task| {
            dioxus::core::spawn_forever(task);
        },
    );
}

/// The app's sync engine (UI thread).
pub fn engine() -> Engine {
    ENGINE.with(Engine::clone)
}

/// Sync a held folder now (or once more after the run going on).
pub fn start(id: &str) {
    engine().start(id)
}

/// This device's platform, as the service names it.
pub fn this_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::Macos
    } else if cfg!(target_os = "ios") {
        Platform::Ios
    } else if cfg!(target_os = "android") {
        Platform::Android
    } else {
        Platform::Linux
    }
}

/// Attach a folder of this device (`docs/agent-phase4-brief.md` D3): pick it, register
/// it with the service, keep its handle here and start its first sync. `Ok(None)` when
/// the pick was cancelled.
pub async fn attach(app: AppCtx) -> Result<Option<Workspace>, String> {
    let Some(pick) = crate::picker::pick_folder().await else {
        return Ok(None);
    };
    let api = app.api().ok_or_else(|| "Not signed in.".to_string())?;
    let ws = api
        .create_workspace(&NewWorkspace {
            name: pick.name.clone(),
            platform: this_platform(),
            location: pick.location.clone(),
        })
        .await
        .map_err(|e| app.failure(&e))?;
    let kept = store::insert(
        ws.id.clone(),
        LocalFolder {
            handle: pick.handle.clone(),
            name: pick.name,
            location: pick.location,
            revoked: None,
            last_manifest_at: None,
        },
    );
    if let Err(e) = kept {
        let _ = api.delete_workspace(&ws.id).await;
        let handle = pick.handle;
        let _ = tokio::task::spawn_blocking(move || super::forget(&handle)).await;
        return Err(format!("Couldn't keep the folder on this device: {e}"));
    }
    let mut lib = app.lib;
    lib.write().apply(Event::WorkspaceChanged {
        workspace: ws.clone(),
    });
    engine().start(&ws.id);
    Ok(Some(ws))
}

/// Remove a folder this device holds: its files are deleted from the service, chats
/// lose it, and this device lets go of it. Nothing in the folder changes.
pub async fn remove(app: AppCtx, id: &str) -> Result<(), String> {
    let api = app.api().ok_or_else(|| "Not signed in.".to_string())?;
    match api.delete_workspace(id).await {
        Ok(()) => {}
        // Already gone on the service.
        Err(e) if e.status == Some(404) => {}
        Err(e) => return Err(app.failure(&e)),
    }
    engine().forget(id);
    if let Some(folder) = store::remove(id) {
        let handle = folder.handle;
        let _ = tokio::task::spawn_blocking(move || super::forget(&handle)).await;
    }
    let mut lib = app.lib;
    lib.write()
        .apply(Event::WorkspaceDeleted { id: id.to_string() });
    let mut ask = app.ask_workspaces;
    if ask.peek().iter().any(|w| w == id) {
        ask.write().retain(|w| w != id);
    }
    Ok(())
}

/// After the service listed the folders: let go of held folders it no longer has
/// (removed while this device was away).
pub async fn prune_held(ids: std::collections::BTreeSet<Id>) {
    let gone = store::prune(&ids);
    let engine = engine();
    for (id, folder) in gone {
        engine.forget(&id);
        let handle = folder.handle;
        let _ = tokio::task::spawn_blocking(move || super::forget(&handle)).await;
    }
}

// ---------------------------------------------------------------------------
// The scan
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum ScanError {
    Cancelled,
    Dir(DirError),
}

impl From<DirError> for ScanError {
    fn from(e: DirError) -> Self {
        ScanError::Dir(e)
    }
}

/// What a walk found: the files it read, and those it saw but couldn't read (evicted
/// to iCloud, locked, no permission), which the service must keep rather than remove.
#[derive(Debug, Default)]
struct Scan {
    files: Vec<ManifestFile>,
    unreadable: Vec<UnreadableFile>,
}

async fn scan_blocking(
    opener: Opener,
    folder: &LocalFolder,
    cancel: Arc<AtomicBool>,
) -> Result<Scan, ScanError> {
    let handle = folder.handle.clone();
    tokio::task::spawn_blocking(move || {
        let dir = opener(&handle)?;
        scan(dir.as_ref(), &cancel)
    })
    .await
    .unwrap_or_else(|e| {
        Err(ScanError::Dir(DirError::Io(format!(
            "The walk stopped ({e})"
        ))))
    })
}

/// Every visible, allow-listed file of `dir` with its stamp, in walk order (depth-first,
/// name order). Blocking.
fn scan(dir: &dyn Dir, cancel: &AtomicBool) -> Result<Scan, ScanError> {
    let mut found = Scan::default();
    walk(dir, "", cancel, &mut found)?;
    Ok(found)
}

fn walk(dir: &dyn Dir, rel: &str, cancel: &AtomicBool, found: &mut Scan) -> Result<(), ScanError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(ScanError::Cancelled);
    }
    let mut entries = match dir.list(rel) {
        Ok(entries) => entries,
        // Gone since its parent was listed, or a link: nothing in it to take.
        Err(DirError::NotFound | DirError::Outside | DirError::NotADir) if !rel.is_empty() => {
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    for entry in entries {
        if is_skipped_name(&entry.name) {
            continue;
        }
        let path = if rel.is_empty() {
            entry.name.clone()
        } else {
            format!("{rel}/{}", entry.name)
        };
        match entry.kind {
            DirKind::Dir => walk(dir, &path, cancel, found)?,
            DirKind::File => {
                let Some(kind) = folder_file_type(&entry.name) else {
                    continue;
                };
                match hash_file(dir, &path, cancel) {
                    Ok((bytes, sha256)) => found.files.push(ManifestFile {
                        path,
                        kind,
                        stamp: FileStamp {
                            bytes,
                            modified: entry.modified,
                            sha256,
                        },
                    }),
                    Err(ScanError::Cancelled) => return Err(ScanError::Cancelled),
                    Err(ScanError::Dir(DirError::Revoked(m))) => {
                        return Err(DirError::Revoked(m).into());
                    }
                    // Gone since it was listed: the service may remove it.
                    Err(ScanError::Dir(DirError::NotFound)) => {}
                    // Unreadable (evicted to iCloud, locked, no permission, a link): it
                    // can't be sent, but it's still there, so the service keeps its copy.
                    Err(ScanError::Dir(e)) => {
                        eprintln!("passing over {path}: {e}");
                        let reason = if dir.evicted(&path) {
                            UnreadableReason::NotDownloaded
                        } else {
                            UnreadableReason::CantRead
                        };
                        found.unreadable.push(UnreadableFile { path, reason });
                    }
                }
            }
        }
    }
    Ok(())
}

/// A file's size and SHA-256, read through once.
fn hash_file(dir: &dyn Dir, path: &str, cancel: &AtomicBool) -> Result<(u64, String), ScanError> {
    let mut reader = dir.open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(ScanError::Cancelled);
        }
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(DirError::Io(e.to_string()).into()),
        };
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((total, hex(&hasher.finish())))
}

// ---------------------------------------------------------------------------
// Reading a file to send it
// ---------------------------------------------------------------------------

enum ReadRequest {
    Stat(oneshot::Sender<Result<DirEntry, DirError>>),
    Read {
        offset: u64,
        len: u64,
        reply: oneshot::Sender<Result<Vec<u8>, DirError>>,
    },
}

/// One file of a held folder, read on its own thread (a `Dir` stays on the thread that
/// opened it). Reads in order go on from where the last one ended; an earlier offset
/// opens the file again. Dropping it closes the folder.
struct FileReader {
    requests: std::sync::mpsc::Sender<ReadRequest>,
}

impl FileReader {
    fn spawn(opener: Opener, handle: FolderHandle, path: &str) -> Self {
        let (requests, incoming) = std::sync::mpsc::channel::<ReadRequest>();
        let path = path.to_string();
        let spawned = std::thread::Builder::new()
            .name("margin-folder-read".into())
            .spawn(move || serve_reads(opener, handle, path, incoming));
        if let Err(e) = spawned {
            eprintln!("couldn't start reading a folder file: {e}");
        }
        Self { requests }
    }

    async fn stat(&self) -> Result<DirEntry, DirError> {
        let (reply, answer) = oneshot::channel();
        self.ask(ReadRequest::Stat(reply), answer).await
    }

    /// Exactly `len` bytes from `offset`.
    async fn read(&self, offset: u64, len: u64) -> Result<Vec<u8>, DirError> {
        let (reply, answer) = oneshot::channel();
        self.ask(ReadRequest::Read { offset, len, reply }, answer)
            .await
    }

    async fn ask<T>(
        &self,
        request: ReadRequest,
        answer: oneshot::Receiver<Result<T, DirError>>,
    ) -> Result<T, DirError> {
        if self.requests.send(request).is_err() {
            return Err(DirError::Io("The file reader stopped.".into()));
        }
        answer
            .await
            .unwrap_or_else(|_| Err(DirError::Io("The file reader stopped.".into())))
    }
}

fn serve_reads(
    opener: Opener,
    handle: FolderHandle,
    path: String,
    incoming: std::sync::mpsc::Receiver<ReadRequest>,
) {
    let dir = match opener(&handle) {
        Ok(dir) => dir,
        Err(e) => {
            for request in incoming {
                match request {
                    ReadRequest::Stat(reply) => {
                        let _ = reply.send(Err(e.clone()));
                    }
                    ReadRequest::Read { reply, .. } => {
                        let _ = reply.send(Err(e.clone()));
                    }
                }
            }
            return;
        }
    };
    let mut open: Option<(Box<dyn Read + '_>, u64)> = None;
    for request in incoming {
        match request {
            ReadRequest::Stat(reply) => {
                let _ = reply.send(dir.stat(&path));
            }
            ReadRequest::Read { offset, len, reply } => {
                let _ = reply.send(read_at(dir.as_ref(), &path, &mut open, offset, len));
            }
        }
    }
}

fn read_at<'a>(
    dir: &'a dyn Dir,
    path: &str,
    open: &mut Option<(Box<dyn Read + 'a>, u64)>,
    offset: u64,
    len: u64,
) -> Result<Vec<u8>, DirError> {
    if open.as_ref().is_none_or(|(_, at)| *at > offset) {
        *open = Some((dir.open(path)?, 0));
    }
    let Some((reader, at)) = open.as_mut() else {
        unreachable!()
    };
    let io = |e: std::io::Error| DirError::Io(e.to_string());
    if *at < offset {
        let skipped =
            std::io::copy(&mut reader.take(offset - *at), &mut std::io::sink()).map_err(io)?;
        *at += skipped;
        if *at < offset {
            *open = None;
            return Err(DirError::Io(
                "The file got shorter while it was being sent.".into(),
            ));
        }
    }
    let mut data = Vec::with_capacity(len as usize);
    let read = reader.take(len).read_to_end(&mut data).map_err(io)?;
    *at += read as u64;
    if (read as u64) < len {
        *open = None;
        return Err(DirError::Io(
            "The file got shorter while it was being sent.".into(),
        ));
    }
    Ok(data)
}

// ---------------------------------------------------------------------------
// SHA-256, for the stamps
// ---------------------------------------------------------------------------

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Streaming SHA-256 (the `sha2` crate), with the stamps' API.
struct Sha256(sha2::Sha256);

impl Sha256 {
    fn new() -> Self {
        Self(<sha2::Sha256 as sha2::Digest>::new())
    }

    fn update(&mut self, data: &[u8]) {
        sha2::Digest::update(&mut self.0, data);
    }

    fn finish(self) -> [u8; 32] {
        sha2::Digest::finalize(self.0).into()
    }

    fn digest(data: &[u8]) -> [u8; 32] {
        let mut h = Self::new();
        h.update(data);
        h.finish()
    }
}

#[cfg(test)]
pub mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use margin_shared::{FolderFileType, NewWorkspace, Platform, Source};

    use super::*;
    use crate::fake::{FakeService, TOKEN};
    use crate::folders::StdDir;
    use crate::folders::store::Store;

    /// A scratch folder removed when dropped.
    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new(tag: &str) -> Self {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "margin-sync-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        pub fn file(&self, rel: &str, bytes: &[u8]) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }

        pub fn handle(&self) -> FolderHandle {
            FolderHandle::Path {
                path: self.0.to_string_lossy().into_owned(),
                bookmark_b64: None,
            }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The D5b tree: what the sync takes and what it leaves alone.
    pub fn d5b(tag: &str) -> TempDir {
        let dir = TempDir::new(tag);
        dir.file("build/notes.md", b"# Notes\n");
        dir.file("paper.pdf", b"%PDF-1.4 fake");
        dir.file("photo.HEIC", b"heic bytes");
        dir.file("src/main.rs", b"fn main() {}");
        dir.file("data.json", b"{}");
        dir.file(".hidden.md", b"hidden");
        dir.file(".git/HEAD.md", b"ref");
        dir.file("credentials.txt", b"secret");
        dir
    }

    pub fn std_opener() -> Opener {
        Arc::new(|handle: &FolderHandle| match handle {
            FolderHandle::Path { path, .. } => {
                StdDir::new(path).map(|d| Box::new(d) as Box<dyn Dir>)
            }
            _ => Err(DirError::Revoked("not a path".into())),
        })
    }

    /// A folder that takes its time listing (to stop a scan half way).
    struct SlowDir(StdDir);

    impl Dir for SlowDir {
        fn list(&self, rel: &str) -> Result<Vec<DirEntry>, DirError> {
            std::thread::sleep(Duration::from_millis(150));
            self.0.list(rel)
        }
        fn stat(&self, rel: &str) -> Result<DirEntry, DirError> {
            self.0.stat(rel)
        }
        fn open(&self, rel: &str) -> Result<Box<dyn Read + '_>, DirError> {
            self.0.open(rel)
        }
    }

    fn slow_opener() -> Opener {
        Arc::new(|handle: &FolderHandle| match handle {
            FolderHandle::Path { path, .. } => {
                StdDir::new(path).map(|d| Box::new(SlowDir(d)) as Box<dyn Dir>)
            }
            _ => Err(DirError::NotFound),
        })
    }

    pub struct Setup {
        pub fake: FakeService,
        pub api: Api,
        pub store: Shared,
        pub engine: Engine,
    }

    pub async fn setup(opener: Opener) -> Setup {
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, TOKEN);
        let store = Store::memory().shared();
        let engine = Engine::new(store.clone(), opener, |task| {
            tokio::task::spawn_local(task);
        });
        engine.set_api(Some(api.clone()));
        Setup {
            fake,
            api,
            store,
            engine,
        }
    }

    impl Setup {
        /// Register `dir` as a folder of this device and hold it.
        pub async fn attach(&self, dir: &TempDir, name: &str) -> Id {
            let ws = self
                .api
                .create_workspace(&NewWorkspace {
                    name: name.into(),
                    platform: Platform::Macos,
                    location: format!("~/{name}"),
                })
                .await
                .unwrap();
            store::lock(&self.store)
                .insert(
                    ws.id.clone(),
                    LocalFolder {
                        handle: dir.handle(),
                        name: name.into(),
                        location: format!("~/{name}"),
                        revoked: None,
                        last_manifest_at: None,
                    },
                )
                .unwrap();
            ws.id
        }

        async fn until(&self, id: &str, what: impl Fn(&SyncState) -> bool) -> SyncState {
            for _ in 0..600 {
                if let Some(state) = self.engine.state(id)
                    && what(&state)
                    && !self.engine.running(id)
                {
                    return state;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("still {:?}", self.engine.state(id));
        }

        fn folder_names(&self, id: &str) -> Vec<String> {
            let s = self.fake.state.lock().unwrap();
            let mut files: Vec<&Source> = s
                .folder_sources
                .iter()
                .filter(|f| f.workspace_id.as_deref() == Some(id))
                .collect();
            files.sort_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then(a.id.len().cmp(&b.id.len()))
                    .then(a.id.cmp(&b.id))
            });
            files.iter().map(|f| f.name.clone()).collect()
        }
    }

    fn idle(s: &SyncState) -> bool {
        matches!(s, SyncState::Idle { .. })
    }

    #[test]
    fn sha256_matches_the_standard_vectors() {
        assert_eq!(
            hex(&Sha256::digest(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&Sha256::digest(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // A million "a", fed in uneven pieces.
        let mut h = Sha256::new();
        let a = vec![b'a'; 1000];
        for i in 0..1000 {
            let cut = i % 7;
            h.update(&a[..cut]);
            h.update(&a[cut..]);
        }
        assert_eq!(
            hex(&h.finish()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn the_scan_takes_exactly_the_allow_listed_files_with_stamps() {
        let dir = d5b("scan");
        let std = StdDir::new(&dir.0).unwrap();
        let files = scan(&std, &AtomicBool::new(false)).unwrap().files;
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["build/notes.md", "paper.pdf", "photo.HEIC"]);
        assert_eq!(files[0].kind, FolderFileType::Text);
        assert_eq!(files[1].kind, FolderFileType::Pdf);
        assert_eq!(files[2].kind, FolderFileType::Image);
        assert_eq!(files[1].stamp.bytes, 13);
        assert_eq!(
            files[1].stamp.sha256,
            hex(&Sha256::digest(b"%PDF-1.4 fake"))
        );
        assert!(files.iter().all(|f| f.stamp.modified.is_some()));
        // A cancelled scan says so.
        assert!(matches!(
            scan(&std, &AtomicBool::new(true)),
            Err(ScanError::Cancelled)
        ));
    }

    /// A folder where some files can be listed but not read (evicted to iCloud, locked).
    /// Paths in the list can't be opened; those ending in `.HEIC` read as evicted.
    struct LockedDir(StdDir, Arc<std::sync::Mutex<Vec<String>>>);

    impl Dir for LockedDir {
        fn list(&self, rel: &str) -> Result<Vec<DirEntry>, DirError> {
            self.0.list(rel)
        }
        fn stat(&self, rel: &str) -> Result<DirEntry, DirError> {
            self.0.stat(rel)
        }
        fn open(&self, rel: &str) -> Result<Box<dyn Read + '_>, DirError> {
            if self.1.lock().unwrap().iter().any(|p| p == rel) {
                return Err(DirError::Io("Operation not permitted".into()));
            }
            self.0.open(rel)
        }
        fn evicted(&self, rel: &str) -> bool {
            rel.ends_with(".HEIC")
        }
    }

    #[tokio::test]
    async fn an_unreadable_file_is_kept_not_removed() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let dir = d5b("locked");
                let locked = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
                let opener: Opener = {
                    let locked = locked.clone();
                    Arc::new(move |handle: &FolderHandle| match handle {
                        FolderHandle::Path { path, .. } => StdDir::new(path)
                            .map(|d| Box::new(LockedDir(d, locked.clone())) as Box<dyn Dir>),
                        _ => Err(DirError::NotFound),
                    })
                };
                let t = setup(opener).await;
                let id = t.attach(&dir, "notes").await;
                t.engine.request(&id).await.unwrap().unwrap();
                t.until(&id, idle).await;
                assert_eq!(t.folder_names(&id).len(), 3);

                // Evicted: listed, not readable. Nothing is removed or sent.
                locked
                    .lock()
                    .unwrap()
                    .extend(["paper.pdf".to_string(), "photo.HEIC".into()]);
                let o = t.engine.request(&id).await.unwrap().unwrap();
                assert_eq!((o.plan.kept, o.plan.removed), (3, 0));
                assert!(o.plan.upload.is_empty());
                t.until(&id, idle).await;
                assert_eq!(
                    t.folder_names(&id),
                    vec!["build/notes.md", "paper.pdf", "photo.HEIC"]
                );
                {
                    let s = t.fake.state.lock().unwrap();
                    let manifest = &s.manifests_received.last().unwrap().1;
                    let unreadable = vec![
                        UnreadableFile {
                            path: "paper.pdf".into(),
                            reason: UnreadableReason::CantRead,
                        },
                        UnreadableFile {
                            path: "photo.HEIC".into(),
                            reason: UnreadableReason::NotDownloaded,
                        },
                    ];
                    assert_eq!(manifest.unreadable, unreadable);
                    assert_eq!(manifest.files.len(), 1);
                    let ws = s.workspaces.iter().find(|w| w.id == id).unwrap();
                    assert_eq!(ws.unreadable, unreadable);
                }

                // Readable again: listed as usual, and the list is clear.
                locked.lock().unwrap().clear();
                let o = t.engine.request(&id).await.unwrap().unwrap();
                assert_eq!((o.plan.kept, o.plan.removed), (3, 0));
                t.until(&id, idle).await;
                let s = t.fake.state.lock().unwrap();
                assert!(s.manifests_received.last().unwrap().1.unreadable.is_empty());
                assert!(s.workspaces.iter().all(|w| w.unreadable.is_empty()));
            })
            .await;
    }

    #[tokio::test]
    async fn a_sync_sends_the_manifest_then_the_plans_files_in_order() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let dir = d5b("sync");
                let t = setup(std_opener()).await;
                let id = t.attach(&dir, "notes").await;
                let outcome = t.engine.request(&id).await.unwrap().unwrap();
                assert_eq!(
                    outcome.message(),
                    "Synced “notes”: 3 files to send; sending now."
                );
                t.until(&id, idle).await;
                assert_eq!(
                    t.folder_names(&id),
                    vec!["build/notes.md", "paper.pdf", "photo.HEIC"]
                );
                {
                    let s = t.fake.state.lock().unwrap();
                    let manifest = &s.manifests_received[0].1;
                    assert_eq!(manifest.files.len(), 3);
                    for f in &s.folder_sources {
                        assert_eq!(f.workspace_id.as_deref(), Some(id.as_str()));
                        assert_eq!(f.folder.as_deref(), Some("notes"));
                        assert!(f.modified.is_some());
                    }
                    let ws = s.workspaces.iter().find(|w| w.id == id).unwrap();
                    assert_eq!((ws.sync.files, ws.sync.uploading), (3, 0));
                }
                assert!(
                    store::lock(&t.store)
                        .get(&id)
                        .unwrap()
                        .last_manifest_at
                        .is_some()
                );

                // Nothing changed: nothing is sent again.
                let o = t.engine.request(&id).await.unwrap().unwrap();
                assert_eq!(o.plan.upload, Vec::<String>::new());
                assert_eq!(o.plan.kept, 3);
                assert_eq!(
                    o.message(),
                    "Synced “notes”: nothing to send; the service has every file."
                );
                t.until(&id, idle).await;
                assert_eq!(t.folder_names(&id).len(), 3);

                // An edit and a deletion: one removed and sent again, one removed.
                dir.file("paper.pdf", b"%PDF-1.4 edited!");
                std::fs::remove_file(dir.0.join("photo.HEIC")).unwrap();
                let o = t.engine.request(&id).await.unwrap().unwrap();
                assert_eq!(o.plan.upload, vec!["paper.pdf".to_string()]);
                assert_eq!(o.plan.removed, 2);
                assert_eq!(
                    o.message(),
                    "Synced “notes”: 1 file to send, 2 removed; sending now."
                );
                t.until(&id, idle).await;
                assert_eq!(t.folder_names(&id), vec!["build/notes.md", "paper.pdf"]);
            })
            .await;
    }

    #[tokio::test]
    async fn a_large_file_goes_up_in_parts_and_resumes_after_a_pause() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let dir = TempDir::new("parts");
                let big: Vec<u8> = (0..(routes::UPLOAD_PART_BYTES + 1000))
                    .map(|i| (i % 251) as u8)
                    .collect();
                dir.file("scan.pdf", &big);
                let t = setup(std_opener()).await;
                t.fake.state.lock().unwrap().part_delay = Some(Duration::from_millis(400));
                let id = t.attach(&dir, "papers").await;
                t.engine.start(&id);
                // Stop once the first part is in.
                for _ in 0..1000 {
                    if t.fake.state.lock().unwrap().parts_received >= 1 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                t.engine.pause();
                assert_eq!(
                    t.until(&id, |s| *s == SyncState::Paused).await,
                    SyncState::Paused
                );
                assert!(t.folder_names(&id).is_empty());
                // A start while paused stays paused.
                t.engine.start(&id);
                assert_eq!(t.engine.state(&id), Some(SyncState::Paused));
                assert!(!t.engine.running(&id));

                t.fake.state.lock().unwrap().part_delay = None;
                t.engine.resume();
                t.engine.start(&id);
                t.until(&id, idle).await;
                let s = t.fake.state.lock().unwrap();
                // Two parts in all: the first wasn't sent twice.
                assert_eq!(s.parts_received, 2);
                assert!(s.part_uploads.is_empty());
                let source = &s.folder_sources[0];
                assert_eq!(source.name, "scan.pdf");
                assert_eq!(source.bytes, big.len() as u64);
                assert_eq!(s.source_files[&source.id], big);
            })
            .await;
    }

    #[tokio::test]
    async fn a_pause_mid_scan_sends_nothing() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let dir = d5b("midscan");
                let t = setup(slow_opener()).await;
                let id = t.attach(&dir, "notes").await;
                let answer = t.engine.request(&id);
                tokio::time::sleep(Duration::from_millis(50)).await;
                assert_eq!(t.engine.state(&id), Some(SyncState::Scanning));
                t.engine.pause();
                t.until(&id, |s| *s == SyncState::Paused).await;
                assert_eq!(answer.await.unwrap(), Err(PAUSED.to_string()));
                assert!(t.fake.state.lock().unwrap().manifests_received.is_empty());
                assert_eq!(
                    t.engine.request(&id).await.unwrap(),
                    Err(PAUSED.to_string())
                );
            })
            .await;
    }

    #[tokio::test]
    async fn requests_while_running_are_coalesced() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let dir = d5b("coalesce");
                let t = setup(slow_opener()).await;
                let id = t.attach(&dir, "notes").await;
                // Starts before the walk began are this run's.
                t.engine.start(&id);
                t.engine.start(&id);
                tokio::time::sleep(Duration::from_millis(50)).await;
                assert_eq!(t.engine.state(&id), Some(SyncState::Scanning));
                // Starts while it runs make one more.
                for _ in 0..3 {
                    t.engine.start(&id);
                }
                t.until(&id, idle).await;
                // One run, and one more for the starts while it ran.
                assert_eq!(t.fake.state.lock().unwrap().manifests_received.len(), 2);
                assert_eq!(t.folder_names(&id).len(), 3);
            })
            .await;
    }

    #[tokio::test]
    async fn lost_access_marks_the_folder_and_unknown_folders_are_refused() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let message = "Margin no longer has access to the folder “notes”; attach it again on the iPhone.";
                let opener: Opener =
                    Arc::new(move |_: &FolderHandle| Err(DirError::Revoked(message.into())));
                let t = setup(opener).await;
                let dir = TempDir::new("revoked");
                let id = t.attach(&dir, "notes").await;
                assert_eq!(
                    t.engine.request(&id).await.unwrap(),
                    Err(message.to_string())
                );
                t.until(&id, |s| matches!(s, SyncState::Failed(_))).await;
                assert_eq!(
                    t.engine.state(&id),
                    Some(SyncState::Failed(message.into()))
                );
                assert_eq!(
                    store::lock(&t.store).snapshot().revoked(&id),
                    Some(message)
                );
                assert_eq!(t.engine.manifest(&id).await, Err(message.to_string()));
                assert_eq!(
                    t.engine.request("nope").await.unwrap(),
                    Err(NOT_HELD.to_string())
                );
            })
            .await;
    }

    #[tokio::test]
    async fn a_folder_removed_on_the_service_is_let_go() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let dir = d5b("gone");
                let t = setup(std_opener()).await;
                let id = t.attach(&dir, "notes").await;
                t.api.delete_workspace(&id).await.unwrap();
                assert_eq!(
                    t.engine.request(&id).await.unwrap(),
                    Err(NOT_HELD.to_string())
                );
                for _ in 0..100 {
                    if store::lock(&t.store).get(&id).is_none() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert!(store::lock(&t.store).get(&id).is_none());
                assert_eq!(t.engine.state(&id), None);
            })
            .await;
    }

    #[tokio::test]
    async fn manifest_and_fetch_answer_the_service() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let dir = d5b("fetch");
                let t = setup(std_opener()).await;
                let id = t.attach(&dir, "notes").await;
                let files = t.engine.manifest(&id).await.unwrap();
                assert_eq!(files.len(), 3);
                let pdf = files.iter().find(|f| f.path == "paper.pdf").unwrap();
                assert_eq!(
                    t.engine.fetch(&id, "paper.pdf", &pdf.stamp).await,
                    Ok("Sent “notes/paper.pdf”.".to_string())
                );
                assert_eq!(t.folder_names(&id), vec!["paper.pdf"]);
                // A stale stamp isn't sent; neither is a file folders don't take.
                let mut stale = pdf.stamp.clone();
                stale.sha256 = "00".repeat(32);
                assert!(t.engine.fetch(&id, "paper.pdf", &stale).await.is_err());
                assert!(
                    t.engine
                        .fetch(&id, "src/main.rs", &pdf.stamp)
                        .await
                        .is_err()
                );
                assert_eq!(t.folder_names(&id), vec!["paper.pdf"]);
            })
            .await;
    }

    #[test]
    fn renewed_bookmarks_replace_the_old_handles() {
        let store = Store::memory().shared();
        let old = FolderHandle::Path {
            path: "/Users/k/renewed".into(),
            bookmark_b64: Some("b2xk".into()),
        };
        let new = FolderHandle::Path {
            path: "/Users/k/renewed".into(),
            bookmark_b64: Some("bmV3".into()),
        };
        store::lock(&store)
            .insert(
                "w1".into(),
                LocalFolder {
                    handle: old.clone(),
                    name: "notes".into(),
                    location: "~/notes".into(),
                    revoked: None,
                    last_manifest_at: None,
                },
            )
            .unwrap();
        let pair = (old.clone(), new.clone());
        let opener: Opener = Arc::new(move |_: &FolderHandle| {
            crate::folders::resaved(pair.0.clone(), pair.1.clone());
            Err(DirError::NotFound)
        });
        let engine = Engine::new(store.clone(), opener, |_| {});
        let _ = (engine.0.opener)(&old);
        assert_eq!(store::lock(&store).get("w1").unwrap().handle, new);
    }
}
