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
use margin_shared::{DeviceToolOutput, DeviceToolResult, DirectedFrame, Id};
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
            let mut next = self.next_conn.lock().unwrap();
            *next += 1;
            *next
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let mut sockets = self.sockets.lock().unwrap();
        let list = sockets.entry(device_id.to_string()).or_default();
        let came_online = list.is_empty();
        list.push((conn, tx));
        (Conn(conn), rx, came_online)
    }

    /// Forgets a closed socket; calls waiting on it fail as Offline. Returns whether the
    /// device just went offline (emit `DeviceChanged`).
    pub fn unregister(&self, device_id: &str, conn: Conn) -> bool {
        let went_offline = {
            let mut sockets = self.sockets.lock().unwrap();
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
            .lock()
            .unwrap()
            .retain(|_, p| !(p.device_id == device_id && p.conn == conn.0));
        went_offline
    }

    pub fn is_online(&self, device_id: &str) -> bool {
        self.sockets
            .lock()
            .unwrap()
            .get(device_id)
            .is_some_and(|l| !l.is_empty())
    }

    /// Ids of the devices online now.
    pub fn online(&self) -> Vec<Id> {
        self.sockets.lock().unwrap().keys().cloned().collect()
    }

    /// Closes the sockets of a revoked device's calls (they fail as Offline).
    pub fn forget_device(&self, device_id: &str) {
        self.sockets.lock().unwrap().remove(device_id);
        self.pending
            .lock()
            .unwrap()
            .retain(|_, p| p.device_id != device_id);
    }

    /// A `DeviceFrame::ToolResult` from `device_id`: completes the waiting call. Answers
    /// for unknown or cancelled requests, or from another device, are ignored.
    pub fn deliver(&self, device_id: &str, request_id: &str, result: DeviceToolResult) {
        let _ = (device_id, request_id, result);
        unimplemented!("service: DeviceHub::deliver (phase 3, service owner)")
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
        let _ = (device_id, call);
        unimplemented!("service: DeviceHub::call (phase 3, service owner)")
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
}
