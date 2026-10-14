//! The device-tool runner: requests the service addresses to this device over the events
//! socket (`DirectedFrame`), each run as its own task keyed by `request_id` and answered
//! with `DeviceFrame::ToolResult` before the request's deadline. `ToolCancel` abandons
//! one; a dropped connection abandons them all (the service has given up on them).
//!
//! App-control calls (`navigate`, `open_citation`, `show_passage`, `toggle_sidebar`) go
//! to the shell, which runs them on the main thread (`app_control`). Folder calls
//! (`sync_folder`, `manifest`, `fetch_file`) go to the folder sync engine
//! (`crate::folders::sync`). The old on-request reads of a folder (`list_files`,
//! `find_files`, `search_text`, `read_file`) answer that the service reads folders now.

pub mod app_control;

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use dioxus::prelude::{ReadableExt, WritableExt};
use futures_util::future::{AbortHandle, Abortable};
use margin_shared::{
    DeviceFrame, DeviceToolCall, DeviceToolResult, DirectedFrame, Id, ToolRequest,
};
use tokio::sync::oneshot;

use crate::state::{AgentCommand, AppCtx};

/// What the old on-request folder reads answer: a folder's files are copied to the
/// service and read there.
pub const READ_ON_SERVICE: &str = "This device doesn't read folders on request any more: an attached folder's files are copied to the Margin service, which searches them.";

/// The shortest time a request is given, whatever its deadline says: the deadline is the
/// service's clock, and this device's may be ahead of it. The service cancels the
/// request at its own deadline anyway, and ignores an answer after that.
pub const MIN_WAIT: Duration = Duration::from_secs(5);

/// A task's future, for whatever spawns it.
pub type LocalTask = Pin<Box<dyn Future<Output = ()>>>;

/// Where the runner's work goes: the app (`AppHost`) or a test.
pub trait Host: Clone + 'static {
    /// Run an app-control call and give its answer.
    fn app_control(
        &self,
        chat_id: Id,
        call: DeviceToolCall,
    ) -> Pin<Box<dyn Future<Output = DeviceToolResult>>>;
    /// Run a folder call (`SyncFolder`, `Manifest`, `FetchFile`) and give its answer.
    fn folder(&self, call: DeviceToolCall) -> Pin<Box<dyn Future<Output = DeviceToolResult>>>;
    /// Send a frame to the service (dropped when there's no connection).
    fn send(&self, frame: DeviceFrame);
}

/// The requests running on this device, by `request_id`.
#[derive(Default)]
pub struct Runner {
    running: Rc<RefCell<HashMap<Id, AbortHandle>>>,
}

impl Runner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a request (its task goes to `spawn`) or cancel one.
    pub fn handle<H: Host>(&self, frame: DirectedFrame, host: &H, spawn: impl FnOnce(LocalTask)) {
        match frame {
            DirectedFrame::ToolRequest { request } => {
                let request_id = request.request_id.clone();
                let (abort, registration) = AbortHandle::new_pair();
                // The same id again (the service never does this) replaces the first.
                if let Some(old) = self.running.borrow_mut().insert(request_id.clone(), abort) {
                    old.abort();
                }
                let running = self.running.clone();
                let host = host.clone();
                let work = Abortable::new(
                    async move {
                        if let Some(frame) = answer(&host, request).await {
                            host.send(frame);
                        }
                    },
                    registration,
                );
                spawn(Box::pin(async move {
                    let finished = work.await.is_ok();
                    // A cancelled task was already taken out (and maybe replaced).
                    if finished {
                        running.borrow_mut().remove(&request_id);
                    }
                }));
            }
            DirectedFrame::ToolCancel { request_id } => {
                if let Some(abort) = self.running.borrow_mut().remove(&request_id) {
                    abort.abort();
                }
            }
        }
    }

    /// Abandon every running request (the connection they came on is gone).
    pub fn cancel_all(&self) {
        for (_, abort) in self.running.borrow_mut().drain() {
            abort.abort();
        }
    }

    /// How many requests are running.
    #[cfg(test)]
    pub fn running(&self) -> usize {
        self.running.borrow().len()
    }
}

/// Time left until `deadline`, at least `MIN_WAIT`.
pub fn time_left(deadline: DateTime<Utc>, now: DateTime<Utc>) -> Duration {
    (deadline - now).to_std().unwrap_or_default().max(MIN_WAIT)
}

/// Run one request; the answer frame, or `None` when the deadline passed first (the
/// service no longer waits for it).
pub async fn answer<H: Host>(host: &H, request: ToolRequest) -> Option<DeviceFrame> {
    let wait = time_left(request.deadline, Utc::now());
    answer_within(host, request, wait).await
}

async fn answer_within<H: Host>(
    host: &H,
    request: ToolRequest,
    wait: Duration,
) -> Option<DeviceFrame> {
    let result = match request.call {
        DeviceToolCall::ListFiles { .. }
        | DeviceToolCall::FindFiles { .. }
        | DeviceToolCall::SearchText { .. }
        | DeviceToolCall::ReadFile { .. } => DeviceToolResult::Err {
            message: READ_ON_SERVICE.into(),
        },
        call @ (DeviceToolCall::SyncFolder { .. }
        | DeviceToolCall::Manifest { .. }
        | DeviceToolCall::FetchFile { .. }) => {
            tokio::time::timeout(wait, host.folder(call)).await.ok()?
        }
        call @ (DeviceToolCall::Navigate { .. }
        | DeviceToolCall::OpenCitation { .. }
        | DeviceToolCall::ShowPassage { .. }
        | DeviceToolCall::ToggleSidebar { .. }) => {
            tokio::time::timeout(wait, host.app_control(request.chat_id, call))
                .await
                .ok()?
        }
    };
    Some(DeviceFrame::ToolResult {
        request_id: request.request_id,
        result,
    })
}

/// The runner's host in the app: app-control calls queue for the shell, frames go to the
/// live loop's connection.
#[derive(Clone, Copy)]
pub struct AppHost(pub AppCtx);

impl Host for AppHost {
    fn app_control(
        &self,
        chat_id: Id,
        call: DeviceToolCall,
    ) -> Pin<Box<dyn Future<Output = DeviceToolResult>>> {
        let (reply, answer) = oneshot::channel();
        let mut queue = self.0.agent_commands;
        queue.write().push_back(AgentCommand {
            call,
            chat_id,
            reply,
        });
        Box::pin(async move {
            answer.await.unwrap_or_else(|_| DeviceToolResult::Err {
                message: "The app was signed out before it could do that.".into(),
            })
        })
    }

    fn folder(&self, call: DeviceToolCall) -> Pin<Box<dyn Future<Output = DeviceToolResult>>> {
        run_folder_call(crate::folders::sync::engine(), call)
    }

    fn send(&self, frame: DeviceFrame) {
        if let Some(sender) = self.0.device_frames.peek().as_ref() {
            let _ = sender.send(frame);
        }
    }
}

/// A folder call answered by a sync engine: `SyncFolder` once the service has the
/// manifest (the uploads go on after the answer), `Manifest` with the walk, `FetchFile`
/// once the file is sent.
pub fn run_folder_call(
    engine: crate::folders::sync::Engine,
    call: DeviceToolCall,
) -> Pin<Box<dyn Future<Output = DeviceToolResult>>> {
    use margin_shared::DeviceToolOutput;
    Box::pin(async move {
        let result = match call {
            DeviceToolCall::SyncFolder { workspace_id } => {
                match engine.request(&workspace_id).await {
                    Ok(Ok(outcome)) => Ok(DeviceToolOutput::Done {
                        message: outcome.message(),
                    }),
                    Ok(Err(message)) => Err(message),
                    Err(_) => Err("The sync stopped before the service answered.".into()),
                }
            }
            DeviceToolCall::Manifest { workspace_id } => engine
                .manifest(&workspace_id)
                .await
                .map(|files| DeviceToolOutput::Manifest { files }),
            DeviceToolCall::FetchFile {
                workspace_id,
                path,
                stamp,
            } => engine
                .fetch(&workspace_id, &path, &stamp)
                .await
                .map(|message| DeviceToolOutput::Done { message }),
            _ => Err(READ_ON_SERVICE.into()),
        };
        match result {
            Ok(output) => DeviceToolResult::Ok { output },
            Err(message) => DeviceToolResult::Err { message },
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use margin_shared::{DeviceToolOutput, NavTarget};
    use tokio::sync::mpsc;

    /// Answers app control from a script: `Some(message)` at once, `None` never.
    #[derive(Clone)]
    struct FakeHost {
        reply: Option<String>,
        sent: mpsc::UnboundedSender<DeviceFrame>,
        asked: Rc<RefCell<Vec<(Id, DeviceToolCall)>>>,
        engine: Option<crate::folders::sync::Engine>,
    }

    impl Host for FakeHost {
        fn app_control(
            &self,
            chat_id: Id,
            call: DeviceToolCall,
        ) -> Pin<Box<dyn Future<Output = DeviceToolResult>>> {
            self.asked.borrow_mut().push((chat_id, call));
            let reply = self.reply.clone();
            Box::pin(async move {
                // Long enough for a test to cancel it first.
                tokio::time::sleep(Duration::from_millis(50)).await;
                match reply {
                    Some(message) => DeviceToolResult::Ok {
                        output: DeviceToolOutput::Done { message },
                    },
                    None => std::future::pending().await,
                }
            })
        }

        fn folder(&self, call: DeviceToolCall) -> Pin<Box<dyn Future<Output = DeviceToolResult>>> {
            match &self.engine {
                Some(engine) => run_folder_call(engine.clone(), call),
                None => Box::pin(async { unreachable!("no folder calls in this test") }),
            }
        }

        fn send(&self, frame: DeviceFrame) {
            let _ = self.sent.send(frame);
        }
    }

    fn host(reply: Option<&str>) -> (FakeHost, mpsc::UnboundedReceiver<DeviceFrame>) {
        let (sent, rx) = mpsc::unbounded_channel();
        (
            FakeHost {
                reply: reply.map(str::to_string),
                sent,
                asked: Rc::default(),
                engine: None,
            },
            rx,
        )
    }

    fn request(id: &str, call: DeviceToolCall, deadline_in: chrono::Duration) -> DirectedFrame {
        DirectedFrame::ToolRequest {
            request: ToolRequest {
                request_id: id.into(),
                chat_id: "c1".into(),
                call_id: format!("call_{id}"),
                deadline: Utc::now() + deadline_in,
                call,
            },
        }
    }

    fn spawn_local(task: LocalTask) {
        tokio::task::spawn_local(task);
    }

    #[test]
    fn time_left_has_a_floor() {
        let now = Utc::now();
        assert_eq!(
            time_left(now + chrono::Duration::seconds(20), now),
            Duration::from_secs(20)
        );
        assert_eq!(time_left(now - chrono::Duration::seconds(3), now), MIN_WAIT);
        assert_eq!(time_left(now + chrono::Duration::seconds(1), now), MIN_WAIT);
    }

    #[tokio::test]
    async fn app_control_is_answered_with_the_hosts_result() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (host, mut sent) = host(Some("Opened All notes."));
                let runner = Runner::new();
                let call = DeviceToolCall::Navigate {
                    to: NavTarget::AllNotes,
                };
                runner.handle(
                    request("r1", call.clone(), chrono::Duration::seconds(20)),
                    &host,
                    spawn_local,
                );
                assert_eq!(
                    sent.recv().await.unwrap(),
                    DeviceFrame::ToolResult {
                        request_id: "r1".into(),
                        result: DeviceToolResult::Ok {
                            output: DeviceToolOutput::Done {
                                message: "Opened All notes.".into()
                            }
                        }
                    }
                );
                assert_eq!(*host.asked.borrow(), vec![("c1".to_string(), call)]);
                tokio::task::yield_now().await;
                assert_eq!(runner.running(), 0);
            })
            .await;
    }

    #[tokio::test]
    async fn old_folder_reads_answer_that_the_service_reads_folders() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (host, mut sent) = host(None);
                let runner = Runner::new();
                let calls = [
                    DeviceToolCall::ListFiles {
                        workspace_id: "w1".into(),
                        path: String::new(),
                    },
                    DeviceToolCall::FindFiles {
                        workspace_id: "w1".into(),
                        path: String::new(),
                        pattern: "*.md".into(),
                    },
                    DeviceToolCall::SearchText {
                        workspace_id: "w1".into(),
                        path: String::new(),
                        query: "x".into(),
                        pattern: None,
                    },
                    DeviceToolCall::ReadFile {
                        workspace_id: "w1".into(),
                        path: "a.md".into(),
                        start_line: 1,
                        max_lines: 10,
                    },
                ];
                for (i, call) in calls.into_iter().enumerate() {
                    let id = format!("r{i}");
                    runner.handle(
                        request(&id, call, chrono::Duration::seconds(60)),
                        &host,
                        spawn_local,
                    );
                    assert_eq!(
                        sent.recv().await.unwrap(),
                        DeviceFrame::ToolResult {
                            request_id: id,
                            result: DeviceToolResult::Err {
                                message: READ_ON_SERVICE.into()
                            }
                        }
                    );
                }
                assert!(host.asked.borrow().is_empty());
            })
            .await;
    }

    #[tokio::test]
    async fn sync_folder_runs_the_sync_and_answers_with_the_plan() {
        use crate::folders::sync::tests::{d5b, setup, std_opener};
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let dir = d5b("tool");
                let t = setup(std_opener()).await;
                let id = t.attach(&dir, "notes").await;
                let (mut host, mut sent) = host(None);
                host.engine = Some(t.engine.clone());
                let runner = Runner::new();
                let sync = |w: &str| DeviceToolCall::SyncFolder {
                    workspace_id: w.into(),
                };
                runner.handle(
                    request("r1", sync(&id), chrono::Duration::seconds(60)),
                    &host,
                    spawn_local,
                );
                assert_eq!(
                    sent.recv().await.unwrap(),
                    DeviceFrame::ToolResult {
                        request_id: "r1".into(),
                        result: DeviceToolResult::Ok {
                            output: DeviceToolOutput::Done {
                                message: "Synced “notes”: 3 files to send; sending now.".into()
                            }
                        }
                    }
                );
                // A folder this device doesn't hold.
                runner.handle(
                    request("r2", sync("w-other"), chrono::Duration::seconds(60)),
                    &host,
                    spawn_local,
                );
                assert_eq!(
                    sent.recv().await.unwrap(),
                    DeviceFrame::ToolResult {
                        request_id: "r2".into(),
                        result: DeviceToolResult::Err {
                            message: crate::folders::sync::NOT_HELD.into()
                        }
                    }
                );
                // A cancelled request answers nothing.
                runner.handle(
                    request("r3", sync(&id), chrono::Duration::seconds(60)),
                    &host,
                    spawn_local,
                );
                runner.handle(
                    DirectedFrame::ToolCancel {
                        request_id: "r3".into(),
                    },
                    &host,
                    spawn_local,
                );
                assert_eq!(runner.running(), 0);
                tokio::time::sleep(Duration::from_millis(300)).await;
                assert!(sent.try_recv().is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn cancel_abandons_a_request_without_answering() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (host, mut sent) = host(Some("Showed the sidebar."));
                let runner = Runner::new();
                let call = DeviceToolCall::ToggleSidebar { visible: None };
                for id in ["r1", "r2", "r3"] {
                    runner.handle(
                        request(id, call.clone(), chrono::Duration::seconds(20)),
                        &host,
                        spawn_local,
                    );
                }
                tokio::task::yield_now().await;
                assert_eq!(runner.running(), 3);
                let cancel = |id: &str| DirectedFrame::ToolCancel {
                    request_id: id.into(),
                };
                runner.handle(cancel("r1"), &host, spawn_local);
                assert_eq!(runner.running(), 2);
                // A cancel for something unknown (already answered) does nothing.
                runner.handle(cancel("gone"), &host, spawn_local);
                assert_eq!(runner.running(), 2);
                runner.handle(cancel("r3"), &host, spawn_local);
                // Only the one left running answers.
                let DeviceFrame::ToolResult { request_id, .. } = sent.recv().await.unwrap();
                assert_eq!(request_id, "r2");
                tokio::time::sleep(Duration::from_millis(100)).await;
                assert!(sent.try_recv().is_err());
                assert_eq!(runner.running(), 0);
            })
            .await;
    }

    #[tokio::test]
    async fn a_lost_connection_abandons_everything() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (host, mut sent) = host(Some("Opened All notes."));
                let runner = Runner::new();
                for id in ["r1", "r2"] {
                    let call = DeviceToolCall::Navigate {
                        to: NavTarget::AllNotes,
                    };
                    runner.handle(
                        request(id, call, chrono::Duration::seconds(20)),
                        &host,
                        spawn_local,
                    );
                }
                runner.cancel_all();
                assert_eq!(runner.running(), 0);
                tokio::time::sleep(Duration::from_millis(100)).await;
                assert!(sent.try_recv().is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn no_answer_once_the_wait_is_over() {
        let (host, _sent) = host(None);
        let call = DeviceToolCall::ToggleSidebar {
            visible: Some(true),
        };
        let DirectedFrame::ToolRequest { request } =
            request("r1", call, chrono::Duration::seconds(20))
        else {
            unreachable!()
        };
        assert_eq!(
            answer_within(&host, request, Duration::from_millis(20)).await,
            None
        );
    }
}
