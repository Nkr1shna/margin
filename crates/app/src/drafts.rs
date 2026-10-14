//! Note edits the service doesn't have yet, kept outside the note editor so leaving a
//! note (or quitting the app) in the middle of an edit doesn't lose it.
//!
//! The editor records its unsaved changes here on every keystroke and clears them once
//! the service has them. When the editor goes away with changes still pending, it hands
//! them to a task that outlives it ([`save_in_background`]); when the window closes or
//! the app quits, [`flush_blocking`] sends whatever is left before the process exits.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use dioxus::prelude::*;
use margin_shared::{Id, Note, NotePatch};

use crate::api::{Api, ApiResult};

/// The title the service stores for a note whose title was cleared.
pub const UNTITLED: &str = "Untitled note";

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

static PENDING: Mutex<BTreeMap<Id, Draft>> = Mutex::new(BTreeMap::new());

fn pending() -> std::sync::MutexGuard<'static, BTreeMap<Id, Draft>> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
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

/// Record the note's unsaved edit (`None`: nothing unsaved).
pub fn record(id: &str, api: Option<&Api>, patch: Option<NotePatch>) {
    let draft = match (api.and_then(|a| a.token().map(|t| (a.base(), t))), patch) {
        (Some((base, token)), Some(patch)) => Draft {
            base: base.to_string(),
            token: token.to_string(),
            patch,
        },
        _ => {
            pending().remove(id);
            return;
        }
    };
    pending().insert(id.to_string(), draft);
}

/// The note's unsaved edit, if any (left in place).
pub fn get(id: &str) -> Option<NotePatch> {
    pending().get(id).map(|d| d.patch.clone())
}

/// Remove and return the note's unsaved edit.
pub fn take(id: &str) -> Option<Draft> {
    pending().remove(id)
}

/// Put back an edit that couldn't be sent, unless a newer one was recorded meanwhile.
pub fn restore(id: &str, draft: Draft) {
    pending().entry(id.to_string()).or_insert(draft);
}

/// Send the note's unsaved edit from a task that outlives the editor, retrying a few
/// times. An edit that still can't be sent is kept, so reopening the note shows it and
/// quitting tries once more; one the service rejects as gone (deleted) is dropped.
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
                    let mut lib = app.lib;
                    lib.write()
                        .apply(margin_shared::Event::NoteChanged { note });
                    return;
                }
                Err(e) if e.status == Some(404) => return,
                Err(e) if e.is_unauthorized() => {
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
/// edits the service accepted. (Desktop only: the mobile shells have no quit hook.)
#[cfg_attr(not(feature = "desktop"), allow(dead_code))]
pub fn flush_blocking(timeout: Duration) -> usize {
    let drafts: Vec<(Id, Draft)> = std::mem::take(&mut *pending()).into_iter().collect();
    send_blocking(drafts, timeout)
}

/// Send these edits on a thread of their own, waiting at most `timeout`.
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
                let sends = drafts.iter().map(|(id, d)| d.send(id));
                futures_util::future::join_all(sends)
                    .await
                    .into_iter()
                    .filter(Result::is_ok)
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
        assert_eq!(get("draft-test-1"), None);
        // Restoring doesn't overwrite a newer edit.
        let newer = patch_for(("A", ""), ("C", ""));
        record("draft-test-1", Some(&api), newer.clone());
        restore("draft-test-1", draft);
        assert_eq!(get("draft-test-1"), newer);
        record("draft-test-1", Some(&api), None);
        assert_eq!(get("draft-test-1"), None);
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
