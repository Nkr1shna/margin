//! The folders this device holds and their handles, persisted on this device (4-core,
//! `docs/agent-phase4-brief.md` D2).
//!
//! One process-wide store (`global`) behind a mutex, kept as JSON at
//! `config::data_dir()/folders.json` (`{ "version": 1, "folders": { "<workspace id>":
//! { "handle", "name", "location", "revoked", "last_manifest_at" } } }`), written to a
//! temporary file and renamed into place. Every change bumps a counter the root
//! component follows (`changes`), copying what it needs into `AppCtx::folders`; blocking
//! threads only ever touch the store, never a signal. Handles never leave the device.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use chrono::{DateTime, Utc};
use margin_shared::Id;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use super::FolderHandle;

/// The file format's version.
const VERSION: u32 = 1;

/// A folder this device holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalFolder {
    pub handle: FolderHandle,
    /// The folder's own name, as the workspace has it.
    pub name: String,
    /// Where it is, as the owner would recognise it. Display only.
    pub location: String,
    /// Access to the folder was lost: what the Folders pane says about it.
    #[serde(default)]
    pub revoked: Option<String>,
    /// When the service last took this folder's manifest.
    #[serde(default)]
    pub last_manifest_at: Option<DateTime<Utc>>,
}

/// Whether this device can still read a folder it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderStatus {
    Ok,
    Revoked(String),
}

/// What the screens know about the folders this device holds (`AppCtx::folders`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalFolders {
    pub held: BTreeMap<Id, FolderStatus>,
}

impl LocalFolders {
    pub fn holds(&self, id: &str) -> bool {
        self.held.contains_key(id)
    }

    pub fn revoked(&self, id: &str) -> Option<&str> {
        match self.held.get(id) {
            Some(FolderStatus::Revoked(message)) => Some(message),
            _ => None,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    version: u32,
    #[serde(default)]
    folders: BTreeMap<Id, LocalFolder>,
}

/// The held folders, and where they are kept (`None`: in memory only, for tests).
#[derive(Debug)]
pub struct Store {
    path: Option<PathBuf>,
    folders: BTreeMap<Id, LocalFolder>,
    changes: watch::Sender<u64>,
}

/// A store shared between the screens, the sync engine and its blocking threads.
pub type Shared = Arc<Mutex<Store>>;

impl Store {
    /// The store kept at `path`, as far as it can be read (a missing or unreadable file
    /// is an empty store; the next change writes a good one).
    pub fn open(path: Option<PathBuf>) -> Self {
        let folders = path
            .as_deref()
            .and_then(read_file)
            .map(|f| f.folders)
            .unwrap_or_default();
        Self {
            path,
            folders,
            changes: watch::Sender::new(0),
        }
    }

    /// An empty store that is never written.
    #[cfg(test)]
    pub fn memory() -> Self {
        Self::open(None)
    }

    pub fn shared(self) -> Shared {
        Arc::new(Mutex::new(self))
    }

    pub fn get(&self, id: &str) -> Option<LocalFolder> {
        self.folders.get(id).cloned()
    }

    pub fn ids(&self) -> Vec<Id> {
        self.folders.keys().cloned().collect()
    }

    pub fn snapshot(&self) -> LocalFolders {
        LocalFolders {
            held: self
                .folders
                .iter()
                .map(|(id, f)| {
                    let status = match &f.revoked {
                        Some(message) => FolderStatus::Revoked(message.clone()),
                        None => FolderStatus::Ok,
                    };
                    (id.clone(), status)
                })
                .collect(),
        }
    }

    /// Follows every change (the counter's value means nothing).
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// Hold a new folder. On a failure to save, the store is as it was.
    pub fn insert(&mut self, id: Id, folder: LocalFolder) -> Result<(), String> {
        let before = self.folders.insert(id.clone(), folder);
        if let Err(e) = self.save() {
            match before {
                Some(old) => self.folders.insert(id, old),
                None => self.folders.remove(&id),
            };
            return Err(e);
        }
        self.bump();
        Ok(())
    }

    /// A stale bookmark was made again: `old` is replaced by `new` wherever it is held.
    pub fn update_handle(&mut self, old: &FolderHandle, new: FolderHandle) -> Result<(), String> {
        let ids: Vec<Id> = self
            .folders
            .iter()
            .filter(|(_, f)| f.handle == *old)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let new = new.clone();
            self.change(&id, |f| f.handle = new)?;
        }
        Ok(())
    }

    /// Access to the folder was lost; the sync stops until it is attached again.
    pub fn mark_revoked(&mut self, id: &str, message: &str) -> Result<(), String> {
        if self
            .folders
            .get(id)
            .is_some_and(|f| f.revoked.as_deref() == Some(message))
        {
            return Ok(());
        }
        self.change(id, |f| f.revoked = Some(message.to_string()))
    }

    /// The folder could be read again.
    pub fn clear_revoked(&mut self, id: &str) -> Result<(), String> {
        if self.folders.get(id).is_some_and(|f| f.revoked.is_none()) {
            return Ok(());
        }
        self.change(id, |f| f.revoked = None)
    }

    /// The service took a manifest of the folder now.
    pub fn touch_synced(&mut self, id: &str) -> Result<(), String> {
        self.change(id, |f| f.last_manifest_at = Some(Utc::now()))
    }

    /// Stop holding a folder; its handle comes back (for `folders::forget`).
    pub fn remove(&mut self, id: &str) -> Option<LocalFolder> {
        let removed = self.folders.remove(id)?;
        if let Err(e) = self.save() {
            eprintln!("couldn't save the folders this device holds: {e}");
        }
        self.bump();
        Some(removed)
    }

    /// Keep only the folders in `keep` (the service's list, just fetched); the others
    /// were removed while this device was away. Their handles come back for
    /// `folders::forget`.
    pub fn prune(&mut self, keep: &BTreeSet<Id>) -> Vec<(Id, LocalFolder)> {
        let gone: Vec<Id> = self
            .folders
            .keys()
            .filter(|id| !keep.contains(*id))
            .cloned()
            .collect();
        if gone.is_empty() {
            return Vec::new();
        }
        let removed: Vec<(Id, LocalFolder)> = gone
            .into_iter()
            .filter_map(|id| self.folders.remove(&id).map(|f| (id, f)))
            .collect();
        if let Err(e) = self.save() {
            eprintln!("couldn't save the folders this device holds: {e}");
        }
        self.bump();
        removed
    }

    fn change(&mut self, id: &str, update: impl FnOnce(&mut LocalFolder)) -> Result<(), String> {
        let Some(folder) = self.folders.get_mut(id) else {
            return Ok(());
        };
        let before = folder.clone();
        update(folder);
        if *folder == before {
            return Ok(());
        }
        let result = self.save();
        self.bump();
        result
    }

    fn bump(&self) {
        self.changes.send_modify(|n| *n = n.wrapping_add(1));
    }

    fn save(&self) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        write_file(
            path,
            &File {
                version: VERSION,
                folders: self.folders.clone(),
            },
        )
    }
}

fn read_file(path: &Path) -> Option<File> {
    let bytes = std::fs::read(path).ok()?;
    match serde_json::from_slice::<File>(&bytes) {
        Ok(file) if file.version <= VERSION => Some(file),
        Ok(file) => {
            eprintln!(
                "{} is from a newer Margin (version {}); not reading it",
                path.display(),
                file.version
            );
            None
        }
        Err(e) => {
            eprintln!("couldn't read {}: {e}", path.display());
            None
        }
    }
}

/// Written to a temporary file next to it, then renamed over it.
fn write_file(path: &Path, file: &File) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
    }
    let json = serde_json::to_vec_pretty(file).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("Couldn't write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("Couldn't save {}: {e}", path.display()))
}

/// Where the app keeps the held folders.
fn default_path() -> Option<PathBuf> {
    // Tests never touch the real data folder.
    if cfg!(test) {
        return None;
    }
    crate::config::data_dir().map(|d| d.join("folders.json"))
}

/// The app's store, read from disk the first time it is asked for.
pub fn global() -> &'static Shared {
    static STORE: OnceLock<Shared> = OnceLock::new();
    STORE.get_or_init(|| Store::open(default_path()).shared())
}

/// Lock a shared store (a panic elsewhere while it was held doesn't lose it).
pub fn lock(store: &Shared) -> MutexGuard<'_, Store> {
    store.lock().unwrap_or_else(|e| e.into_inner())
}

/// Read the store from disk now (on launch, before the live loop).
pub fn load() {
    let _ = global();
}

pub fn insert(id: Id, folder: LocalFolder) -> Result<(), String> {
    lock(global()).insert(id, folder)
}

/// Called by the platform's `open` (4a, 4b) when it made a stale bookmark again: the
/// handle it was given (`old`) is replaced by `new` wherever the store holds it.
#[allow(dead_code)]
pub fn update_handle(old: &FolderHandle, new: FolderHandle) -> Result<(), String> {
    lock(global()).update_handle(old, new)
}

pub fn remove(id: &str) -> Option<LocalFolder> {
    lock(global()).remove(id)
}

pub fn prune(keep: &BTreeSet<Id>) -> Vec<(Id, LocalFolder)> {
    lock(global()).prune(keep)
}

pub fn snapshot() -> LocalFolders {
    lock(global()).snapshot()
}

pub fn changes() -> watch::Receiver<u64> {
    lock(global()).changes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        std::env::temp_dir()
            .join(format!(
                "margin-folders-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ))
            .join("folders.json")
    }

    fn folder(name: &str) -> LocalFolder {
        LocalFolder {
            handle: FolderHandle::Path {
                path: format!("/Users/k/{name}"),
                bookmark_b64: None,
            },
            name: name.into(),
            location: format!("~/{name}"),
            revoked: None,
            last_manifest_at: None,
        }
    }

    #[test]
    fn persists_atomically_and_reads_back() {
        let path = temp_path("persist");
        let mut store = Store::open(Some(path.clone()));
        let mut changes = store.changes();
        store.insert("w1".into(), folder("notes")).unwrap();
        store.insert("w2".into(), folder("papers")).unwrap();
        assert!(changes.has_changed().unwrap());
        changes.mark_unchanged();
        store
            .mark_revoked("w2", "Margin no longer has access to “papers”.")
            .unwrap();
        store.touch_synced("w1").unwrap();
        let old = store.get("w1").unwrap().handle;
        store
            .update_handle(
                &old,
                FolderHandle::Path {
                    path: "/Users/k/notes".into(),
                    bookmark_b64: Some("Ym9vaw==".into()),
                },
            )
            .unwrap();
        assert!(changes.has_changed().unwrap());
        // No temporary file is left behind.
        assert!(!path.with_extension("json.tmp").exists());
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["folders"]["w1"]["handle"]["kind"], "Path");
        assert_eq!(json["folders"]["w1"]["name"], "notes");

        let again = Store::open(Some(path.clone()));
        assert_eq!(again.folders, store.folders);
        assert!(again.get("w1").unwrap().last_manifest_at.is_some());
        let snapshot = again.snapshot();
        assert_eq!(snapshot.held["w1"], FolderStatus::Ok);
        assert_eq!(
            snapshot.revoked("w2"),
            Some("Margin no longer has access to “papers”.")
        );
        assert!(snapshot.holds("w1") && !snapshot.holds("w3"));

        // Access back: the badge goes.
        store.clear_revoked("w2").unwrap();
        assert_eq!(store.snapshot().held["w2"], FolderStatus::Ok);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn prune_keeps_what_the_service_lists() {
        let mut store = Store::memory();
        for id in ["w1", "w2", "w3"] {
            store.insert(id.into(), folder(id)).unwrap();
        }
        let keep: BTreeSet<Id> = ["w2".to_string(), "other".to_string()].into();
        let removed = store.prune(&keep);
        let ids: Vec<&str> = removed.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["w1", "w3"]);
        assert_eq!(store.ids(), vec!["w2".to_string()]);
        assert!(store.prune(&keep).is_empty());
        assert_eq!(store.remove("w2").unwrap().name, "w2");
        assert!(store.remove("w2").is_none());
    }

    #[test]
    fn a_damaged_or_newer_file_reads_as_empty() {
        let path = temp_path("damaged");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        assert!(Store::open(Some(path.clone())).ids().is_empty());
        std::fs::write(&path, r#"{"version": 9, "folders": {}}"#).unwrap();
        assert!(Store::open(Some(path.clone())).ids().is_empty());
        // The next change writes a good file.
        let mut store = Store::open(Some(path.clone()));
        store.insert("w1".into(), folder("notes")).unwrap();
        assert_eq!(
            Store::open(Some(path.clone())).ids(),
            vec!["w1".to_string()]
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_failed_save_leaves_the_store_as_it_was() {
        // A path under a file can't be created.
        let blocker = temp_path("blocked");
        std::fs::create_dir_all(blocker.parent().unwrap()).unwrap();
        std::fs::write(&blocker, "x").unwrap();
        let mut store = Store::open(Some(blocker.join("folders.json")));
        assert!(store.insert("w1".into(), folder("notes")).is_err());
        assert!(store.get("w1").is_none());
        std::fs::remove_dir_all(blocker.parent().unwrap()).unwrap();
    }
}
