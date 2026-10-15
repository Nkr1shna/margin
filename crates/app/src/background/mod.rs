//! Work that keeps going after Margin leaves the foreground (`docs/phase5-brief.md`
//! D1): on iOS and iPadOS 26 a `BGContinuedProcessingTask`, with the system's progress
//! UI, while a folder this device holds is syncing or a chat that reads one is running.
//! While the task runs the app is *active* though not in front (`state::active`): the
//! events socket stays open, device tools are answered and the sync engine doesn't
//! pause. When it ends or the system expires it, phase 4's behaviour applies again.
//!
//! This file is the contract between the app (the [`Coordinator`], platform-free and
//! unit-tested) and the platform: a [`Work`] per OS from [`platform`] (a no-op on
//! macOS, Linux and Android; 5-ios replaces the iOS arm with `ios.rs`).

use std::collections::BTreeMap;
use std::sync::Arc;

use margin_shared::{Chat, Id, Workspace};

use crate::folders::store::{FolderStatus, LocalFolders};
use crate::folders::sync::SyncState;

/// The one task identifier (`BGTaskSchedulerPermittedIdentifiers` in Dioxus.toml).
pub const TASK_ID: &str = "com.krishnanelloore.margin.app.folders";

/// What a task is, for the system's UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkSpec {
    /// [`TASK_ID`].
    pub id: String,
    /// "Sending “notes”", "Scanning “notes”", "Margin is reading “notes”".
    pub title: String,
    /// The chat's title, when a chat keeps the task.
    pub subtitle: Option<String>,
}

/// How far the task is. `total: None` is indeterminate. `title`/`subtitle` are set
/// only when they changed since the last start or update.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkProgress {
    pub done: u64,
    pub total: Option<u64>,
    pub title: Option<String>,
    pub subtitle: Option<String>,
}

/// The platform's background work. Called from the app's async runtime, never from a
/// blocking thread. The platform reports the task running and ending through
/// `state::set_continued` (true once the system runs it; false when it expires, the
/// owner ends it from the system UI, or `end` is called).
pub trait Work: Send + Sync {
    /// Asks the system to keep running now. Only while the app is in front (the system
    /// refuses otherwise); never queued: `Err` when the system won't run it now or the
    /// OS can't (before iOS 26, other platforms), and the app behaves as in phase 4.
    fn start(&self, spec: WorkSpec) -> Result<(), String>;
    /// New progress (and title or subtitle) for the running task.
    fn update(&self, progress: WorkProgress);
    /// The work is done: the task completes successfully.
    fn end(&self);
}

/// The platforms with no continued work: computers are always in front, and Android's
/// sync stays in-front-only (brief Q1).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoWork;

impl Work for NoWork {
    fn start(&self, _spec: WorkSpec) -> Result<(), String> {
        Err("This platform doesn't keep work going in the background.".into())
    }
    fn update(&self, _progress: WorkProgress) {}
    fn end(&self) {}
}

#[cfg(target_os = "ios")]
pub mod ios;

/// This platform's [`Work`].
pub fn platform() -> Arc<dyn Work> {
    #[cfg(target_os = "ios")]
    {
        Arc::new(ios::ContinuedProcessing)
    }
    #[cfg(not(target_os = "ios"))]
    {
        Arc::new(NoWork)
    }
}

/// What should keep running now, by the D1 table, or `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Want {
    pub title: String,
    pub subtitle: Option<String>,
    pub done: u64,
    pub total: Option<u64>,
}

/// What keeps the task, from the held folders' syncs and the chats:
/// - a sync of a folder this device holds that is `Scanning` or `Uploading` (progress in
///   bytes while uploading, indeterminate while scanning);
/// - a chat `Running` or `WaitingForUser` whose `workspace_ids` holds a folder this
///   device holds (indeterminate; the chat's title as subtitle).
///
/// A turn on a chat with none of this device's folders, and lesson or video jobs (they
/// run on the service), keep nothing.
pub fn wanted<'a>(
    syncs: &BTreeMap<Id, SyncState>,
    chats: impl IntoIterator<Item = &'a Chat>,
    held: &LocalFolders,
    workspaces: &BTreeMap<Id, Workspace>,
) -> Option<Want> {
    let readable = |id: &str| matches!(held.held.get(id), Some(FolderStatus::Ok));
    let name = |id: &str| {
        workspaces
            .get(id)
            .map(|w| w.name.clone())
            .unwrap_or_else(|| "a folder".into())
    };
    let named = |verb: &str, ids: &[&Id]| match ids {
        [one] => format!("{verb} “{}”", name(one)),
        many => format!("{verb} {} folders", many.len()),
    };

    let mut scanning = Vec::new();
    let mut uploading = Vec::new();
    let (mut bytes_done, mut bytes_total) = (0u64, 0u64);
    for (id, state) in syncs {
        if !readable(id) {
            continue;
        }
        match state {
            SyncState::Scanning => scanning.push(id),
            SyncState::Uploading {
                bytes_done: d,
                bytes_total: t,
                ..
            } => {
                uploading.push(id);
                bytes_done += d;
                bytes_total += t;
            }
            SyncState::Idle { .. } | SyncState::Paused | SyncState::Failed(_) => {}
        }
    }
    let chat = chats.into_iter().find_map(|c| {
        let running = c.state.is_busy();
        let folders: Vec<&Id> = c.workspace_ids.iter().filter(|w| readable(w)).collect();
        (running && !folders.is_empty()).then_some((c, folders))
    });
    let subtitle = chat.as_ref().map(|(c, _)| c.title.clone());

    if !uploading.is_empty() {
        return Some(Want {
            title: named("Sending", &uploading),
            subtitle,
            done: bytes_done.min(bytes_total),
            total: Some(bytes_total),
        });
    }
    if !scanning.is_empty() {
        return Some(Want {
            title: named("Scanning", &scanning),
            subtitle,
            done: 0,
            total: None,
        });
    }
    chat.map(|(c, folders)| Want {
        title: named("Margin is reading", &folders),
        subtitle: Some(c.title.clone()),
        done: 0,
        total: None,
    })
}

/// What the coordinator looks at each time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inputs {
    /// `state` foreground: a task can only be started then.
    pub foreground: bool,
    /// `state` continued: the system is running the task.
    pub continued: bool,
    pub want: Option<Want>,
}

/// Decides when to start, update and end the platform's task by the D1 table.
pub struct Coordinator {
    work: Arc<dyn Work>,
    /// A task was started and hasn't ended.
    running: bool,
    /// The system refused this stretch of work; not asked again until it ends or the
    /// app comes back to the front (a refusal as the owner left: `foreground` lags a
    /// tick).
    refused: bool,
    /// The last `Inputs::foreground` seen.
    in_front: bool,
    /// What the running task last showed.
    shown: Option<Want>,
}

impl Coordinator {
    pub fn new(work: Arc<dyn Work>) -> Self {
        Self {
            work,
            running: false,
            refused: false,
            in_front: false,
            shown: None,
        }
    }

    /// Whether a task is running (started and not ended or expired).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn running(&self) -> bool {
        self.running
    }

    pub fn observe(&mut self, inputs: Inputs) {
        // No task running on the system any more: it expired or the owner ended it (in
        // the system's UI, also while Margin is in front). Phase 4's behaviour applies
        // (the live loop pauses the engine); a new task starts when the app is in front
        // and work is still wanted. `start` sets `continued` before it returns, so a
        // task just started is never taken for one that ended.
        if self.running && !inputs.continued {
            self.running = false;
            self.shown = None;
        }
        // Back in front: a refusal from before (as the owner left, say) is asked again.
        if inputs.foreground && !self.in_front {
            self.refused = false;
        }
        self.in_front = inputs.foreground;
        match inputs.want {
            None => {
                self.refused = false;
                if self.running {
                    self.work.end();
                    self.running = false;
                    self.shown = None;
                }
            }
            Some(want) if !self.running => {
                if !inputs.foreground || self.refused {
                    return;
                }
                let spec = WorkSpec {
                    id: TASK_ID.to_string(),
                    title: want.title.clone(),
                    subtitle: want.subtitle.clone(),
                };
                match self.work.start(spec) {
                    Ok(()) => {
                        self.running = true;
                        self.work.update(WorkProgress {
                            done: want.done,
                            total: want.total,
                            title: None,
                            subtitle: None,
                        });
                        self.shown = Some(want);
                    }
                    Err(_) => self.refused = true,
                }
            }
            Some(want) => {
                let Some(shown) = &self.shown else {
                    return;
                };
                if *shown == want {
                    return;
                }
                self.work.update(WorkProgress {
                    done: want.done,
                    total: want.total,
                    title: (shown.title != want.title).then(|| want.title.clone()),
                    subtitle: if shown.subtitle != want.subtitle {
                        want.subtitle.clone()
                    } else {
                        None
                    },
                });
                self.shown = Some(want);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use chrono::Utc;
    use margin_shared::{ChatState, Platform, Provider, TitleSource, Usage, WorkspaceSync};

    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        Start(WorkSpec),
        Update(WorkProgress),
        End,
    }

    #[derive(Default)]
    struct FakeWork {
        calls: Mutex<Vec<Call>>,
        refuse: Mutex<bool>,
    }

    impl FakeWork {
        fn take(&self) -> Vec<Call> {
            std::mem::take(&mut self.calls.lock().unwrap())
        }
    }

    impl Work for FakeWork {
        fn start(&self, spec: WorkSpec) -> Result<(), String> {
            if *self.refuse.lock().unwrap() {
                return Err("no".into());
            }
            self.calls.lock().unwrap().push(Call::Start(spec));
            Ok(())
        }
        fn update(&self, progress: WorkProgress) {
            self.calls.lock().unwrap().push(Call::Update(progress));
        }
        fn end(&self) {
            self.calls.lock().unwrap().push(Call::End);
        }
    }

    fn held(ids: &[(&str, FolderStatus)]) -> LocalFolders {
        LocalFolders {
            held: ids
                .iter()
                .map(|(id, s)| (id.to_string(), s.clone()))
                .collect(),
        }
    }

    fn workspaces(names: &[(&str, &str)]) -> BTreeMap<Id, Workspace> {
        names
            .iter()
            .map(|(id, name)| {
                (
                    id.to_string(),
                    Workspace {
                        access: Vec::new(),
                        deny: Vec::new(),
                        owner: None,
                        id: id.to_string(),
                        name: name.to_string(),
                        device_id: "d".into(),
                        device_name: "iPhone".into(),
                        platform: Platform::Ios,
                        location: "iCloud Drive".into(),
                        created_at: Utc::now(),
                        sync: WorkspaceSync::default(),
                        unreadable: Vec::new(),
                    },
                )
            })
            .collect()
    }

    fn chat(id: &str, state: ChatState, folders: &[&str]) -> Chat {
        Chat {
            access: Vec::new(),
            deny: Vec::new(),
            owner: None,
            id: id.into(),
            title: format!("Chat {id}"),
            title_by: TitleSource::default(),
            state,
            workspace_ids: folders.iter().map(|f| f.to_string()).collect(),
            source_ids: Vec::new(),
            pending: None,
            usage: Usage::default(),
            waiting: None,
            provider: Provider::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn running() -> ChatState {
        ChatState::Running {
            turn_id: "t".into(),
            device_id: "d".into(),
            started_at: Utc::now(),
        }
    }

    fn uploading(bytes_done: u64, bytes_total: u64) -> SyncState {
        SyncState::Uploading {
            done: 1,
            total: 4,
            bytes_done,
            bytes_total,
        }
    }

    #[test]
    fn the_table_decides_what_keeps_the_task() {
        let ws = workspaces(&[("w1", "notes"), ("w2", "papers"), ("w9", "elsewhere")]);
        let mine = held(&[("w1", FolderStatus::Ok), ("w2", FolderStatus::Ok)]);
        let none: [Chat; 0] = [];
        // Nothing running.
        assert_eq!(wanted(&BTreeMap::new(), &none, &mine, &ws), None);
        // A scan: indeterminate.
        let syncs = BTreeMap::from([("w1".to_string(), SyncState::Scanning)]);
        assert_eq!(
            wanted(&syncs, &none, &mine, &ws),
            Some(Want {
                title: "Scanning “notes”".into(),
                subtitle: None,
                done: 0,
                total: None
            })
        );
        // Uploads count bytes, over every uploading folder.
        let syncs = BTreeMap::from([
            ("w1".to_string(), uploading(10, 100)),
            ("w2".to_string(), uploading(5, 50)),
        ]);
        let w = wanted(&syncs, &none, &mine, &ws).unwrap();
        assert_eq!(
            (w.title.as_str(), w.done, w.total),
            ("Sending 2 folders", 15, Some(150))
        );
        // Idle, paused or failed syncs keep nothing.
        let syncs = BTreeMap::from([
            ("w1".to_string(), SyncState::Paused),
            ("w2".to_string(), SyncState::Failed("x".into())),
        ]);
        assert_eq!(wanted(&syncs, &none, &mine, &ws), None);
        // A running chat with a held folder keeps it, titled by the chat.
        let chats = [chat("c1", running(), &["w2"])];
        assert_eq!(
            wanted(&BTreeMap::new(), &chats, &mine, &ws),
            Some(Want {
                title: "Margin is reading “papers”".into(),
                subtitle: Some("Chat c1".into()),
                done: 0,
                total: None
            })
        );
        // Waiting for the owner counts too.
        let waiting = ChatState::WaitingForUser {
            turn_id: "t".into(),
            device_id: "d".into(),
            item_id: "i".into(),
        };
        let chats = [chat("c1", waiting, &["w1"])];
        assert!(wanted(&BTreeMap::new(), &chats, &mine, &ws).is_some());
        // A chat with another device's folder, an idle chat, a revoked folder: no.
        let revoked = held(&[("w1", FolderStatus::Revoked("gone".into()))]);
        for (chats, held) in [
            ([chat("c1", running(), &["w9"])], &mine),
            ([chat("c1", running(), &[])], &mine),
            ([chat("c1", ChatState::Idle, &["w1"])], &mine),
            ([chat("c1", running(), &["w1"])], &revoked),
        ] {
            assert_eq!(wanted(&BTreeMap::new(), &chats, held, &ws), None);
        }
        // A sync and a chat together: the sync's progress, the chat's title below it.
        let syncs = BTreeMap::from([("w1".to_string(), uploading(1, 2))]);
        let chats = [chat("c1", running(), &["w1"])];
        let w = wanted(&syncs, &chats, &mine, &ws).unwrap();
        assert_eq!(w.title, "Sending “notes”");
        assert_eq!(w.subtitle.as_deref(), Some("Chat c1"));
    }

    fn want(title: &str, done: u64) -> Option<Want> {
        Some(Want {
            title: title.into(),
            subtitle: None,
            done,
            total: Some(100),
        })
    }

    fn inputs(foreground: bool, continued: bool, want: Option<Want>) -> Inputs {
        Inputs {
            foreground,
            continued,
            want,
        }
    }

    #[test]
    fn starts_in_front_updates_and_ends() {
        let work = Arc::new(FakeWork::default());
        let mut c = Coordinator::new(work.clone());
        // Nothing wanted: nothing happens.
        c.observe(inputs(true, false, None));
        assert!(work.take().is_empty());
        // A sync starts in front: start, then its progress.
        c.observe(inputs(true, false, want("Sending “notes”", 10)));
        assert_eq!(
            work.take(),
            vec![
                Call::Start(WorkSpec {
                    id: TASK_ID.into(),
                    title: "Sending “notes”".into(),
                    subtitle: None
                }),
                Call::Update(WorkProgress {
                    done: 10,
                    total: Some(100),
                    title: None,
                    subtitle: None
                })
            ]
        );
        assert!(c.running());
        // The system runs it; the app leaves the foreground: progress keeps coming.
        c.observe(inputs(false, true, want("Sending “notes”", 10)));
        assert!(
            work.take().is_empty(),
            "unchanged progress isn't sent again"
        );
        c.observe(inputs(false, true, want("Sending “notes”", 60)));
        c.observe(inputs(false, true, want("Margin is reading “notes”", 0)));
        assert_eq!(
            work.take(),
            vec![
                Call::Update(WorkProgress {
                    done: 60,
                    total: Some(100),
                    title: None,
                    subtitle: None
                }),
                Call::Update(WorkProgress {
                    done: 0,
                    total: Some(100),
                    title: Some("Margin is reading “notes”".into()),
                    subtitle: None
                })
            ]
        );
        // Both end: the task ends.
        c.observe(inputs(false, true, None));
        assert_eq!(work.take(), vec![Call::End]);
        assert!(!c.running());
    }

    #[test]
    fn never_starts_in_the_background() {
        let work = Arc::new(FakeWork::default());
        let mut c = Coordinator::new(work.clone());
        c.observe(inputs(false, false, want("Scanning “notes”", 0)));
        assert!(work.take().is_empty());
        assert!(!c.running());
        // Back in front with the work still wanted: it starts.
        c.observe(inputs(true, false, want("Scanning “notes”", 0)));
        assert!(matches!(work.take()[0], Call::Start(_)));
    }

    #[test]
    fn expiry_restores_phase_4_and_a_later_return_starts_again() {
        let work = Arc::new(FakeWork::default());
        let mut c = Coordinator::new(work.clone());
        c.observe(inputs(true, false, want("Sending “notes”", 1)));
        c.observe(inputs(false, true, want("Sending “notes”", 2)));
        work.take();
        // The system expired the task (`set_continued(false)`): no end, no update.
        c.observe(inputs(false, false, want("Sending “notes”", 3)));
        assert!(work.take().is_empty());
        assert!(!c.running());
        // The engine paused (the sync is Paused): still nothing.
        c.observe(inputs(false, false, None));
        assert!(work.take().is_empty());
        // Reopened, the sync resumes: a new task.
        c.observe(inputs(true, false, want("Sending “notes”", 3)));
        assert!(matches!(work.take()[0], Call::Start(_)));
    }

    #[test]
    fn a_refusal_isnt_retried_until_the_work_ends() {
        let work = Arc::new(FakeWork::default());
        *work.refuse.lock().unwrap() = true;
        let mut c = Coordinator::new(work.clone());
        c.observe(inputs(true, false, want("Sending “notes”", 1)));
        *work.refuse.lock().unwrap() = false;
        c.observe(inputs(true, false, want("Sending “notes”", 2)));
        assert!(work.take().is_empty());
        assert!(!c.running());
        c.observe(inputs(true, false, None));
        c.observe(inputs(true, false, want("Sending “notes”", 1)));
        assert!(matches!(work.take()[0], Call::Start(_)));
    }

    /// A refusal as the owner leaves (the foreground input lags a tick) is asked again
    /// when they come back with the work still running.
    #[test]
    fn a_refusal_is_retried_when_the_app_comes_back_in_front() {
        let work = Arc::new(FakeWork::default());
        *work.refuse.lock().unwrap() = true;
        let mut c = Coordinator::new(work.clone());
        c.observe(inputs(true, false, want("Sending “notes”", 1)));
        *work.refuse.lock().unwrap() = false;
        c.observe(inputs(false, false, want("Sending “notes”", 2)));
        assert!(work.take().is_empty());
        c.observe(inputs(true, false, want("Sending “notes”", 3)));
        assert!(matches!(work.take()[0], Call::Start(_)));
        assert!(c.running());
    }

    /// The owner ends the task from the system's UI while Margin is in front: the
    /// coordinator notices (no update goes to a task that is gone) and starts a new one
    /// while the work is still wanted in front.
    #[test]
    fn a_task_ended_in_front_is_noticed() {
        let work = Arc::new(FakeWork::default());
        let mut c = Coordinator::new(work.clone());
        c.observe(inputs(true, false, want("Sending “notes”", 1)));
        c.observe(inputs(true, true, want("Sending “notes”", 2)));
        work.take();
        // Ended on the system (`set_continued(false)`), still in front.
        c.observe(inputs(true, false, want("Sending “notes”", 3)));
        let calls = work.take();
        assert!(matches!(calls[0], Call::Start(_)), "{calls:?}");
        assert!(!calls.iter().any(|c| matches!(c, Call::End)), "{calls:?}");
        assert!(c.running());
    }

    #[test]
    fn the_no_op_platform_refuses() {
        assert!(
            NoWork
                .start(WorkSpec {
                    id: TASK_ID.into(),
                    title: "x".into(),
                    subtitle: None
                })
                .is_err()
        );
    }
}
