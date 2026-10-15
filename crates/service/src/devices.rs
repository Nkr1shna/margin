//! Device presence and directed frames: which devices have an events socket open, and
//! tool requests sent to one of them (`margin_agent::DeviceBridge`).
//!
//! `events::run` registers its socket here when it opens and unregisters it when it
//! closes; frames for the device arrive on the receiver `register` returns, and the
//! device's `DeviceFrame::ToolResult`s are handed to `deliver`. A device may have more
//! than one socket open (a reconnect overlapping the old one): requests go to the most
//! recently opened, and the device is offline once none is left.

use std::collections::HashMap;
use std::sync::Mutex;

use margin_agent::{BoxFuture, DeviceBridge, DeviceCall, DeviceError};
use margin_shared::sync::Locked;
use margin_shared::{DeviceToolOutput, DeviceToolResult, DirectedFrame, Id, ToolRequest};
use tokio::sync::{mpsc, oneshot};

/// Frames queued for one socket.
pub type FrameSender = mpsc::UnboundedSender<DirectedFrame>;

#[derive(Default)]
pub struct DeviceHub {
    /// device id → its open sockets, oldest first, by connection number.
    sockets: Mutex<HashMap<Id, Vec<(u64, FrameSender)>>>,
    next_conn: Mutex<u64>,
    /// request id → the call waiting for its answer.
    pending: Mutex<HashMap<Id, PendingCall>>,
}

/// A tool request sent to a device and not yet answered.
pub struct PendingCall {
    pub device_id: Id,
    /// The socket it was sent on; if that socket closes, the call fails as Offline.
    pub conn: u64,
    pub answer: oneshot::Sender<DeviceToolResult>,
}

/// A registered socket. Pass it back to `unregister` when the socket closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Conn(pub u64);

impl DeviceHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an open socket for `device_id`. Returns the connection, the frames to
    /// send on it, and whether the device just came online (emit `DeviceChanged`).
    pub fn register(
        &self,
        device_id: &str,
    ) -> (Conn, mpsc::UnboundedReceiver<DirectedFrame>, bool) {
        let conn = {
            let mut next = self.next_conn.locked();
            *next += 1;
            *next
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let mut sockets = self.sockets.locked();
        let list = sockets.entry(device_id.to_string()).or_default();
        let came_online = list.is_empty();
        list.push((conn, tx));
        (Conn(conn), rx, came_online)
    }

    /// Forgets a closed socket; calls waiting on it fail as Offline. Returns whether the
    /// device just went offline (emit `DeviceChanged`).
    pub fn unregister(&self, device_id: &str, conn: Conn) -> bool {
        let went_offline = {
            let mut sockets = self.sockets.locked();
            let Some(list) = sockets.get_mut(device_id) else {
                return false;
            };
            list.retain(|(c, _)| *c != conn.0);
            if list.is_empty() {
                sockets.remove(device_id);
                true
            } else {
                false
            }
        };
        // Dropping the answer senders wakes their callers with Offline.
        self.pending
            .locked()
            .retain(|_, p| !(p.device_id == device_id && p.conn == conn.0));
        went_offline
    }

    pub fn is_online(&self, device_id: &str) -> bool {
        self.sockets
            .locked()
            .get(device_id)
            .is_some_and(|l| !l.is_empty())
    }

    /// Ids of the devices online now.
    pub fn online(&self) -> Vec<Id> {
        self.sockets.locked().keys().cloned().collect()
    }

    /// Closes the sockets of a revoked device's calls (they fail as Offline).
    pub fn forget_device(&self, device_id: &str) {
        self.sockets.locked().remove(device_id);
        self.pending
            .locked()
            .retain(|_, p| p.device_id != device_id);
    }

    /// A `DeviceFrame::ToolResult` from `device_id`: completes the waiting call. Answers
    /// for unknown or cancelled requests, or from another device, are ignored.
    pub fn deliver(&self, device_id: &str, request_id: &str, result: DeviceToolResult) {
        let call = {
            let mut pending = self.pending.locked();
            match pending.get(request_id) {
                Some(p) if p.device_id == device_id => pending.remove(request_id),
                Some(_) => {
                    tracing::warn!(device = %device_id, "ignored a tool result for another device's request");
                    None
                }
                None => None,
            }
        };
        if let Some(call) = call {
            let _ = call.answer.send(result);
        }
    }

    /// The newest socket of `device_id`.
    fn newest(&self, device_id: &str) -> Option<(u64, FrameSender)> {
        self.sockets
            .locked()
            .get(device_id)
            .and_then(|l| l.last().cloned())
    }

    /// Sends `frame` on socket `conn` of `device_id`, if it is still open.
    fn send_on(&self, device_id: &str, conn: u64, frame: DirectedFrame) {
        let sockets = self.sockets.locked();
        if let Some((_, tx)) = sockets
            .get(device_id)
            .and_then(|l| l.iter().find(|(c, _)| *c == conn))
        {
            let _ = tx.send(frame);
        }
    }
}

/// Forgets a request that ended without an answer and tells the device to stop working
/// on it (`ToolCancel`). Dropped when `call` ends: on an answer it is disarmed first.
struct Outstanding<'a> {
    hub: &'a DeviceHub,
    device_id: &'a str,
    conn: u64,
    request_id: Id,
    answered: bool,
}

impl Drop for Outstanding<'_> {
    fn drop(&mut self) {
        if self.answered {
            return;
        }
        let was_pending = self.hub.pending.locked().remove(&self.request_id).is_some();
        if was_pending {
            self.hub.send_on(
                self.device_id,
                self.conn,
                DirectedFrame::ToolCancel {
                    request_id: self.request_id.clone(),
                },
            );
        }
    }
}

impl DeviceBridge for DeviceHub {
    fn is_online(&self, device_id: &str) -> bool {
        DeviceHub::is_online(self, device_id)
    }

    /// Sends a `ToolRequest` (new request id, deadline now + `call.timeout`) on the
    /// device's newest socket and waits for `deliver`; `ToolCancel` on timeout or when
    /// the future is dropped.
    fn call<'a>(
        &'a self,
        device_id: &'a str,
        call: DeviceCall,
    ) -> BoxFuture<'a, Result<DeviceToolOutput, DeviceError>> {
        Box::pin(async move {
            let Some((conn, tx)) = self.newest(device_id) else {
                return Err(DeviceError::Offline);
            };
            let request_id = uuid::Uuid::new_v4().to_string();
            let deadline = chrono::Utc::now()
                + chrono::Duration::from_std(call.timeout).unwrap_or(chrono::Duration::MAX);
            let (answer, answered) = oneshot::channel();
            self.pending.locked().insert(
                request_id.clone(),
                PendingCall {
                    device_id: device_id.to_string(),
                    conn,
                    answer,
                },
            );
            let mut outstanding = Outstanding {
                hub: self,
                device_id,
                conn,
                request_id: request_id.clone(),
                answered: false,
            };
            let request = DirectedFrame::ToolRequest {
                request: ToolRequest {
                    request_id,
                    chat_id: call.chat_id,
                    call_id: call.call_id,
                    deadline,
                    call: call.call,
                },
            };
            if tx.send(request).is_err() {
                return Err(DeviceError::Offline);
            }
            match tokio::time::timeout(call.timeout, answered).await {
                Ok(Ok(result)) => {
                    outstanding.answered = true;
                    match result {
                        DeviceToolResult::Ok { output } => Ok(normalised(output)),
                        DeviceToolResult::Err { message } => Err(DeviceError::Failed(
                            margin_shared::text::for_model(&message),
                        )),
                    }
                }
                // The socket closed (or the device was revoked) before it answered.
                Ok(Err(_)) => {
                    outstanding.answered = true;
                    Err(DeviceError::Offline)
                }
                Err(_) => Err(DeviceError::TimedOut),
            }
        })
    }
}

/// A device's answer with the text it wrote normalised (`text::for_model`) before it
/// becomes a tool output. File paths stay as the device sent them: the sync matches
/// them against the device's own files.
fn normalised(output: DeviceToolOutput) -> DeviceToolOutput {
    use margin_shared::text::for_model;
    match output {
        DeviceToolOutput::Done { message } => DeviceToolOutput::Done {
            message: for_model(&message),
        },
        DeviceToolOutput::FileText {
            path,
            start_line,
            end_line,
            total_lines,
            text,
        } => DeviceToolOutput::FileText {
            path,
            start_line,
            end_line,
            total_lines,
            text: for_model(&text),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_follows_sockets() {
        let hub = DeviceHub::new();
        assert!(!hub.is_online("d1"));
        let (a, _rx_a, first) = hub.register("d1");
        assert!(first);
        let (b, _rx_b, second) = hub.register("d1");
        assert!(!second);
        assert!(hub.is_online("d1"));
        assert_eq!(hub.online(), vec!["d1".to_string()]);
        assert!(!hub.unregister("d1", a));
        assert!(hub.is_online("d1"));
        assert!(hub.unregister("d1", b));
        assert!(!hub.is_online("d1"));
        assert!(!hub.unregister("d1", b));
    }

    fn sidebar(timeout_ms: u64) -> DeviceCall {
        DeviceCall {
            chat_id: "c1".into(),
            call_id: "call_1".into(),
            call: margin_shared::DeviceToolCall::ToggleSidebar {
                visible: Some(true),
            },
            timeout: std::time::Duration::from_millis(timeout_ms),
        }
    }

    fn done(message: &str) -> DeviceToolResult {
        DeviceToolResult::Ok {
            output: DeviceToolOutput::Done {
                message: message.into(),
            },
        }
    }

    /// The request id of a `ToolRequest` frame.
    fn request_id(frame: DirectedFrame) -> Id {
        match frame {
            DirectedFrame::ToolRequest { request } => {
                assert_eq!(request.chat_id, "c1");
                assert_eq!(request.call_id, "call_1");
                // Read after the call started: within its timeout of a moment ago.
                assert!(request.deadline > chrono::Utc::now() - chrono::Duration::seconds(1));
                request.request_id
            }
            other => panic!("expected a request, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn calls_go_to_the_newest_socket_and_wait_for_the_answer() {
        let hub = std::sync::Arc::new(DeviceHub::new());
        let (_old, mut old_rx, _) = hub.register("d1");
        let (_new, mut new_rx, _) = hub.register("d1");
        let call = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.call("d1", sidebar(5_000)).await })
        };
        let id = request_id(new_rx.recv().await.unwrap());
        assert!(old_rx.try_recv().is_err());
        // Another device can't answer for it; unknown ids are ignored.
        hub.deliver("d2", &id, done("wrong device"));
        hub.deliver("d1", "unknown", done("unknown"));
        hub.deliver("d1", &id, done("Sidebar shown."));
        assert_eq!(
            call.await.unwrap(),
            Ok(DeviceToolOutput::Done {
                message: "Sidebar shown.".into()
            })
        );
        // Answered: no cancel follows.
        assert!(new_rx.try_recv().is_err());
        assert!(hub.pending.lock().unwrap().is_empty());
    }

    /// What a device writes reaches the model normalised (`docs/phase5-brief.md`
    /// D2.1); a manifest's paths stay as the device sent them.
    #[tokio::test]
    async fn device_answers_are_normalised() {
        let hub = std::sync::Arc::new(DeviceHub::new());
        let (_c, mut rx, _) = hub.register("d1");
        for (answer, expected) in [
            (
                done("Opened \u{202E}“Tides”\u{200B}."),
                Ok(DeviceToolOutput::Done {
                    message: "Opened “Tides”.".into(),
                }),
            ),
            (
                DeviceToolResult::Err {
                    message: "No ac\u{200B}cess.\u{2066}".into(),
                },
                Err(DeviceError::Failed("No access.".into())),
            ),
            (
                DeviceToolResult::Ok {
                    output: DeviceToolOutput::FileText {
                        path: "a\u{200B}.md".into(),
                        start_line: 1,
                        end_line: 1,
                        total_lines: 1,
                        text: "de\u{200B}lete".into(),
                    },
                },
                Ok(DeviceToolOutput::FileText {
                    path: "a\u{200B}.md".into(),
                    start_line: 1,
                    end_line: 1,
                    total_lines: 1,
                    text: "delete".into(),
                }),
            ),
        ] {
            let call = {
                let hub = hub.clone();
                tokio::spawn(async move { hub.call("d1", sidebar(5_000)).await })
            };
            let id = request_id(rx.recv().await.unwrap());
            hub.deliver("d1", &id, answer);
            assert_eq!(call.await.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn device_errors_are_failures() {
        let hub = std::sync::Arc::new(DeviceHub::new());
        let (_c, mut rx, _) = hub.register("d1");
        let call = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.call("d1", sidebar(5_000)).await })
        };
        let id = request_id(rx.recv().await.unwrap());
        hub.deliver(
            "d1",
            &id,
            DeviceToolResult::Err {
                message: "No access.".into(),
            },
        );
        assert_eq!(
            call.await.unwrap(),
            Err(DeviceError::Failed("No access.".into()))
        );
    }

    #[tokio::test]
    async fn offline_devices_and_closed_sockets() {
        let hub = std::sync::Arc::new(DeviceHub::new());
        assert_eq!(
            hub.call("d1", sidebar(1_000)).await,
            Err(DeviceError::Offline)
        );
        let (conn, mut rx, _) = hub.register("d1");
        let call = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.call("d1", sidebar(5_000)).await })
        };
        request_id(rx.recv().await.unwrap());
        assert!(hub.unregister("d1", conn));
        assert_eq!(call.await.unwrap(), Err(DeviceError::Offline));
    }

    #[tokio::test]
    async fn timeouts_and_dropped_calls_send_cancel() {
        let hub = std::sync::Arc::new(DeviceHub::new());
        let (_c, mut rx, _) = hub.register("d1");
        assert_eq!(
            hub.call("d1", sidebar(30)).await,
            Err(DeviceError::TimedOut)
        );
        let id = request_id(rx.recv().await.unwrap());
        assert_eq!(
            rx.recv().await.unwrap(),
            DirectedFrame::ToolCancel {
                request_id: id.clone()
            }
        );
        // A late answer is ignored.
        hub.deliver("d1", &id, done("late"));

        // Stop drops the waiting call.
        let call = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.call("d1", sidebar(5_000)).await })
        };
        let id = request_id(rx.recv().await.unwrap());
        call.abort();
        let _ = call.await;
        assert_eq!(
            rx.recv().await.unwrap(),
            DirectedFrame::ToolCancel { request_id: id }
        );
        assert!(hub.pending.lock().unwrap().is_empty());
    }
}
