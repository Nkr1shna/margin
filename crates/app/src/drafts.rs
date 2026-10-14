//! Note edits the service doesn't have yet, kept outside the note editor so leaving a
//! note, quitting the app, or the app being killed in the middle of an edit doesn't
//! lose it.
//!
//! The editor records its unsaved changes here on every keystroke and clears them once
//! the service has them. Every change is also written to `drafts.json` in the app's
//! data folder (`config::data_dir`), so an edit survives the process: on the next launch
//! (each time the library loads, [`resume`]) the drafts for this service are sent again,
//! and a note reopened meanwhile shows its draft. Nothing waits for a quit hook, so this
//! works on iOS and Android, which have none. The file holds no token: drafts are sent
//! with the device's token of the time, and only to the service they were made on.
//!
//! When the editor goes away with changes still pending, it hands them to a task that
//! outlives it ([`save_in_background`]); when the desktop window closes or the app
//! quits, [`flush_blocking`] sends whatever is left before the process exits.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use dioxus::prelude::*;
use margin_shared::{Id, Note, NotePatch};
use serde::{Deserialize, Serialize};

use crate::api::{Api, ApiResult};

/// The title the service stores for a note whose title was cleared.
pub const UNTITLED: &str = "Untitled note";
/// The drafts file, in the app's data folder.
pub const FILE_NAME: &str = "drafts.json";

/// An unsaved edit, with the service and token to send it to.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub base: String,
    pub token: String,
    pub patch: NotePatch,
}

impl Draft {
    /// Send the edit (with a fresh client, so this works from any runtime).
    pub async fn send(&self, id: &str) -> ApiResult<Note> {
        Api::with_token(&self.base, &self.token)
            .patch_note(id, &self.patch)
            .await
    }
}

/// A draft as the drafts file keeps it: no token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Saved {
    /// The service it was made on.
    pub base: String,
    pub patch: NotePatch,
    pub updated_at: DateTime<Utc>,
}

/// `drafts.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DraftFile {
    #[serde(default)]
    pub drafts: BTreeMap<Id, Saved>,
}

impl DraftFile {
    /// The file at `path`; empty when it's missing or unreadable.
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Writes it to `path` (through a temporary file, so a crash never leaves half of
    /// it); with no drafts left, removes the file.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if self.drafts.is_empty() {
            return match std::fs::remove_file(path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    Err(format!("Couldn't remove {}: {e}", path.display()))
                }
                _ => Ok(()),
            };
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| format!("Couldn't write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("Couldn't save {}: {e}", path.display()))
    }
}

/// Every unsaved edit this process knows of.
#[derive(Default)]
struct Registry {
    /// Edits waiting to be sent.
    pending: BTreeMap<Id, Draft>,
    /// Edits being sent right now: kept on disk until the service confirms them.
    sending: BTreeMap<Id, Draft>,
    /// When each edit was last changed, for the file.
    updated: BTreeMap<Id, DateTime<Utc>>,
    /// Drafts from the file for another service (or not yet resumed): kept as they are.
    other: BTreeMap<Id, Saved>,
    /// The file has been read into `other`.
    loaded: bool,
    /// Notes open in an editor, which sends their edits itself.
    editing: HashSet<Id>,
}

impl Registry {
    fn load(&mut self) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        if let Some(path) = path() {
            for (id, saved) in DraftFile::load(&path).drafts {
                if !self.pending.contains_key(&id) && !self.sending.contains_key(&id) {
                    self.other.insert(id, saved);
                }
            }
        }
    }

    /// What the file should hold: the newest edit of every note.
    fn file(&self) -> DraftFile {
        let saved = |id: &Id, d: &Draft| Saved {
            base: d.base.clone(),
            patch: d.patch.clone(),
            updated_at: self.updated.get(id).copied().unwrap_or_else(Utc::now),
        };
        let mut drafts = self.other.clone();
        for (id, d) in &self.sending {
            drafts.insert(id.clone(), saved(id, d));
        }
        for (id, d) in &self.pending {
            drafts.insert(id.clone(), saved(id, d));
        }
        DraftFile { drafts }
    }

    fn persist(&mut self) {
        self.load();
        let Some(path) = path() else { return };
        if let Err(e) = self.file().save(&path) {
            eprintln!("couldn't keep unsaved note edits: {e}");
        }
    }

    fn forget_updated(&mut self, id: &str) {
        if !self.pending.contains_key(id) && !self.sending.contains_key(id) {
            self.updated.remove(id);
        }
    }
}

static REGISTRY: Mutex<Option<Registry>> = Mutex::new(None);

fn with<T>(f: impl FnOnce(&mut Registry) -> T) -> T {
    let mut guard = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(Registry::default))
}

/// Where the drafts file lives: the app's data folder (a scratch file in tests).
fn path() -> Option<PathBuf> {
    #[cfg(test)]
    {
        Some(std::env::temp_dir().join(format!("margin-drafts-test-{}.json", std::process::id())))
    }
    #[cfg(not(test))]
    {
        crate::config::data_dir().map(|d| d.join(FILE_NAME))
    }
}

/// The PATCH that brings the service's copy (`saved`) up to the editor's
/// (`current`), or `None` when they match. A blank title is stored as "Untitled note".
pub fn patch_for(saved: (&str, &str), current: (&str, &str)) -> Option<NotePatch> {
    let title = (current.0 != saved.0).then(|| {
        if current.0.trim().is_empty() {
            UNTITLED.to_string()
        } else {
            current.0.to_string()
        }
    });
    let body_md = (current.1 != saved.1).then(|| current.1.to_string());
    (title.is_some() || body_md.is_some()).then_some(NotePatch { title, body_md })
}

/// Record the note's unsaved edit (`None`: nothing unsaved, the service has it all).
pub fn record(id: &str, api: Option<&Api>, patch: Option<NotePatch>) {
    let draft = match (api.and_then(|a| a.token().map(|t| (a.base(), t))), patch) {
        (Some((base, token)), Some(patch)) => Some(Draft {
            base: base.to_string(),
            token: token.to_string(),
            patch,
        }),
        _ => None,
    };
    with(|r| {
        r.load();
        let changed = match draft {
            Some(draft) => {
                let changed = r.pending.get(id) != Some(&draft);
                r.pending.insert(id.to_string(), draft);
                r.updated.insert(id.to_string(), Utc::now());
                r.other.remove(id);
                changed
            }
            None => {
                // Saved by the editor: no earlier edit of it is worth keeping either.
                let had = r.pending.remove(id).is_some() || r.other.remove(id).is_some();
                r.forget_updated(id);
                had
            }
        };
        if changed {
            r.persist();
        }
    });
}

/// The note's unsaved edit, if any (left in place): one waiting, one being sent, or one
/// from the drafts file made on this service.
pub fn get(id: &str) -> Option<NotePatch> {
    with(|r| {
        r.load();
        r.pending
            .get(id)
            .or_else(|| r.sending.get(id))
            .map(|d| d.patch.clone())
    })
}

/// Remove and return the note's unsaved edit to send it; it stays on disk until
/// [`sent`] (or [`restore`] puts it back).
pub fn take(id: &str) -> Option<Draft> {
    with(|r| {
        let draft = r.pending.remove(id)?;
        r.sending.insert(id.to_string(), draft.clone());
        Some(draft)
    })
}

/// The service has `draft`: drop it from the drafts file (a newer edit stays).
pub fn sent(id: &str, draft: &Draft) {
    with(|r| {
        if r.sending.get(id) == Some(draft) {
            r.sending.remove(id);
        }
        r.forget_updated(id);
        r.persist();
    });
}

/// Forget the note's edits altogether (the note was deleted).
pub fn discard(id: &str) {
    with(|r| {
        r.load();
        let had = r.pending.remove(id).is_some()
            | r.sending.remove(id).is_some()
            | r.other.remove(id).is_some();
        r.updated.remove(id);
        if had {
            r.persist();
        }
    });
}

/// Put back an edit that couldn't be sent, unless a newer one was recorded meanwhile.
pub fn restore(id: &str, draft: Draft) {
    with(|r| {
        if r.sending.get(id) == Some(&draft) {
            r.sending.remove(id);
        }
        r.pending.entry(id.to_string()).or_insert(draft);
        r.persist();
    });
}

/// The note is open in an editor (`true`) or no longer (`false`). [`resume`] leaves
/// the edits of open notes to their editor.
pub fn editing(id: &str, open: bool) {
    with(|r| {
        if open {
            r.editing.insert(id.to_string());
        } else {
            r.editing.remove(id);
        }
    });
}

/// Take the drafts file's edits made on `api`'s service into this process (with its
/// current token), and return the notes with edits to send that no editor has open.
pub fn adopt(api: &Api) -> Vec<Id> {
    let Some(token) = api.token() else {
        return Vec::new();
    };
    with(|r| {
        r.load();
        let mine: Vec<Id> = r
            .other
            .iter()
            .filter(|(_, s)| s.base == api.base())
            .map(|(id, _)| id.clone())
            .collect();
        for id in mine {
            if let Some(saved) = r.other.remove(&id) {
                r.updated.insert(id.clone(), saved.updated_at);
                r.pending.entry(id).or_insert(Draft {
                    base: saved.base,
                    token: token.to_string(),
                    patch: saved.patch,
                });
            }
        }
        // A newer token (signed in again) for edits still waiting on this service.
        for draft in r.pending.values_mut() {
            if draft.base == api.base() {
                draft.token = token.to_string();
            }
        }
        r.pending
            .iter()
            .filter(|(id, d)| d.base == api.base() && !r.editing.contains(*id))
            .map(|(id, _)| id.clone())
            .collect()
    })
}

/// Send the edits the service doesn't have yet: those left by an earlier run of the app
/// (from the drafts file) and those whose save failed. Called whenever the library has
/// loaded (launch, sign-in, reconnect).
pub fn resume(app: crate::state::AppCtx, api: &Api) {
    for id in adopt(api) {
        // Notes deleted meanwhile on another device: the service says 404 and the edit
        // is dropped.
        save_in_background(app, id);
    }
}

/// Send the note's unsaved edit from a task that outlives the editor, retrying a few
/// times. An edit that still can't be sent is kept (in the drafts file too), so
/// reopening the note shows it and the next launch or reconnect tries again; one the
/// service rejects as gone (deleted) is dropped.
pub fn save_in_background(app: crate::state::AppCtx, id: Id) {
    // While the whole app is being torn down there is no runtime to spawn on; the
    // edit then stays recorded for `flush_blocking`.
    if dioxus::core::Runtime::try_current().is_none() {
        return;
    }
    let Some(draft) = take(&id) else { return };
    dioxus::core::spawn_forever(async move {
        let mut wait = Duration::from_secs(1);
        for attempt in 0..3 {
            if attempt > 0 {
                tokio::time::sleep(wait).await;
                wait *= 3;
            }
            match draft.send(&id).await {
                Ok(note) => {
                    sent(&id, &draft);
                    let mut lib = app.lib;
                    lib.write()
                        .apply(margin_shared::Event::NoteChanged { note });
                    return;
                }
                Err(e) if e.status == Some(404) => {
                    sent(&id, &draft);
                    return;
                }
                Err(e) if e.is_unauthorized() => {
                    // Kept for when this device signs in again.
                    restore(&id, draft);
                    app.failure(&e);
                    return;
                }
                Err(_) => {}
            }
        }
        eprintln!("couldn't save note {id}; keeping the edit for the next try");
        restore(&id, draft);
    });
}

/// Send every recorded edit now, blocking for at most `timeout`. For the window closing
/// and the app quitting, when the app's own runtime is going away. Returns how many
/// edits the service accepted; the rest stay in the drafts file for the next launch.
/// (Desktop only: the mobile shells have no quit hook, and rely on the file.)
#[cfg_attr(not(feature = "desktop"), allow(dead_code))]
pub fn flush_blocking(timeout: Duration) -> usize {
    let ids: Vec<Id> = with(|r| r.pending.keys().cloned().collect());
    let drafts: Vec<(Id, Draft)> = ids
        .into_iter()
        .filter_map(|id| take(&id).map(|d| (id, d)))
        .collect();
    send_blocking(drafts, timeout)
}

/// Send these edits on a thread of their own, waiting at most `timeout`; each the
/// service accepts leaves the drafts file.
#[cfg_attr(not(feature = "desktop"), allow(dead_code))]
fn send_blocking(drafts: Vec<(Id, Draft)>, timeout: Duration) -> usize {
    if drafts.is_empty() {
        return 0;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("margin-flush-notes".into())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            let saved = rt.block_on(async {
                let sends = drafts.iter().map(|(id, d)| async move {
                    let ok = d.send(id).await.is_ok();
                    if ok {
                        sent(id, d);
                    }
                    ok
                });
                futures_util::future::join_all(sends)
                    .await
                    .into_iter()
                    .filter(|ok| *ok)
                    .count()
            });
            let _ = tx.send(saved);
        });
    if spawned.is_err() {
        return 0;
    }
    rx.recv_timeout(timeout).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeService, TOKEN};

    /// Tests share the registry and its file: each uses its own note ids, and these
    /// helpers read the file for one id.
    fn on_disk(id: &str) -> Option<Saved> {
        DraftFile::load(&path().unwrap()).drafts.remove(id)
    }

    #[test]
    fn patch_for_only_sends_what_changed() {
        assert_eq!(patch_for(("A", "b"), ("A", "b")), None);
        assert_eq!(
            patch_for(("A", "b"), ("A", "b2")),
            Some(NotePatch {
                title: None,
                body_md: Some("b2".into())
            })
        );
        assert_eq!(
            patch_for(("A", "b"), ("A2", "b")),
            Some(NotePatch {
                title: Some("A2".into()),
                body_md: None
            })
        );
        // A cleared title is stored as "Untitled note".
        assert_eq!(
            patch_for(("A", "b"), ("  ", "b")).and_then(|p| p.title),
            Some(UNTITLED.into())
        );
    }

    #[test]
    fn record_take_and_restore() {
        let api = Api::with_token("http://localhost:1", "t");
        let patch = patch_for(("A", ""), ("B", ""));
        record("draft-test-1", Some(&api), patch.clone());
        assert_eq!(get("draft-test-1"), patch);
        // No token (signed out): nothing to send it with, so nothing is kept.
        record(
            "draft-test-2",
            Some(&Api::anonymous("http://x")),
            patch.clone(),
        );
        assert_eq!(get("draft-test-2"), None);
        let draft = take("draft-test-1").unwrap();
        // Being sent: still shown, still on disk.
        assert_eq!(get("draft-test-1"), patch);
        assert_eq!(on_disk("draft-test-1").unwrap().patch, draft.patch);
        // Restoring doesn't overwrite a newer edit.
        let newer = patch_for(("A", ""), ("C", ""));
        record("draft-test-1", Some(&api), newer.clone());
        restore("draft-test-1", draft);
        assert_eq!(get("draft-test-1"), newer);
        record("draft-test-1", Some(&api), None);
        assert_eq!(get("draft-test-1"), None);
        assert_eq!(on_disk("draft-test-1"), None);
    }

    #[test]
    fn edits_are_kept_on_disk_without_the_token_until_sent() {
        let api = Api::with_token("http://localhost:1", "secret-token");
        let patch = patch_for(("A", "b"), ("A", "typed"));
        record("draft-disk-1", Some(&api), patch.clone());
        let saved = on_disk("draft-disk-1").unwrap();
        assert_eq!(saved.base, "http://localhost:1");
        assert_eq!(Some(saved.patch), patch);
        let text = std::fs::read_to_string(path().unwrap()).unwrap();
        assert!(!text.contains("secret-token"));

        let draft = take("draft-disk-1").unwrap();
        assert!(on_disk("draft-disk-1").is_some());
        sent("draft-disk-1", &draft);
        assert_eq!(on_disk("draft-disk-1"), None);
        assert_eq!(get("draft-disk-1"), None);

        record("draft-disk-2", Some(&api), patch.clone());
        discard("draft-disk-2");
        assert_eq!(on_disk("draft-disk-2"), None);
    }

    #[test]
    fn the_drafts_file_round_trips_and_clears() {
        let dir = std::env::temp_dir().join(format!("margin-drafts-file-{}", std::process::id()));
        let file_path = dir.join(FILE_NAME);
        let mut file = DraftFile::default();
        file.drafts.insert(
            "n1".into(),
            Saved {
                base: "https://m.example".into(),
                patch: NotePatch {
                    title: None,
                    body_md: Some("unsent".into()),
                },
                updated_at: Utc::now(),
            },
        );
        file.save(&file_path).unwrap();
        assert_eq!(DraftFile::load(&file_path), file);
        // Missing or damaged: empty.
        std::fs::write(&file_path, "{not json").unwrap();
        assert_eq!(DraftFile::load(&file_path), DraftFile::default());
        // Nothing left: the file goes.
        DraftFile::default().save(&file_path).unwrap();
        assert!(!file_path.exists());
        DraftFile::default().save(&file_path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_launch_adopts_the_files_drafts_for_its_service() {
        // As if an earlier run left these in the file.
        let earlier = |base: &str, body: &str| Saved {
            base: base.into(),
            patch: NotePatch {
                title: None,
                body_md: Some(body.into()),
            },
            updated_at: Utc::now(),
        };
        with(|r| {
            r.load();
            r.other
                .insert("draft-adopt-1".into(), earlier("http://adopt", "mine"));
            r.other
                .insert("draft-adopt-2".into(), earlier("http://elsewhere", "theirs"));
            r.other
                .insert("draft-adopt-3".into(), earlier("http://adopt", "open"));
            r.persist();
        });
        editing("draft-adopt-3", true);
        let api = Api::with_token("http://adopt", "new-token");
        let to_send = adopt(&api);
        // This service's drafts, now with the current token; the open note's is left
        // to its editor; another service's stays in the file.
        assert!(to_send.contains(&"draft-adopt-1".to_string()));
        assert!(!to_send.contains(&"draft-adopt-3".to_string()));
        assert!(!to_send.contains(&"draft-adopt-2".to_string()));
        assert_eq!(get("draft-adopt-3").unwrap().body_md.as_deref(), Some("open"));
        let draft = take("draft-adopt-1").unwrap();
        assert_eq!(draft.token, "new-token");
        assert_eq!(draft.patch.body_md.as_deref(), Some("mine"));
        assert_eq!(get("draft-adopt-2"), None);
        assert_eq!(on_disk("draft-adopt-2").unwrap().base, "http://elsewhere");
        sent("draft-adopt-1", &draft);
        editing("draft-adopt-3", false);
        discard("draft-adopt-2");
        discard("draft-adopt-3");
        assert_eq!(on_disk("draft-adopt-3"), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn flush_blocking_sends_pending_edits() {
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, TOKEN);
        let id = fake.state.lock().unwrap().library.notes[0].id.clone();
        record(&id, Some(&api), patch_for(("", ""), ("Flushed", "on quit")));
        // (Only this test's edit: other tests share the registry.)
        let drafts = vec![(id.clone(), take(&id).unwrap())];
        let saved = tokio::task::spawn_blocking(|| send_blocking(drafts, Duration::from_secs(5)))
            .await
            .unwrap();
        assert_eq!(saved, 1);
        assert_eq!(get(&id), None);
        let s = fake.state.lock().unwrap();
        let note = s.library.notes.iter().find(|n| n.id == id).unwrap();
        assert_eq!(
            (note.title.as_str(), note.body_md.as_str()),
            ("Flushed", "on quit")
        );
    }
}
