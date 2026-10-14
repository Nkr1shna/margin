//! `/api/events`: live `Event`s as JSON text frames, plus the frames directed at this
//! device (`DirectedFrame`: tool requests and cancels) and the device's answers
//! (`DeviceFrame::ToolResult`, handed to `devices::DeviceHub::deliver`). While a socket
//! is open its device counts as online (`Event::DeviceChanged`).

use std::time::Duration;

use axum::Extension;
use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::response::Response;
use margin_shared::{DeviceFrame, DirectedFrame, Event, Id};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc, watch};

use crate::auth::{self, CurrentDevice};
use crate::db;
use crate::state::{AppState, DeviceRow};

const PING_INTERVAL: Duration = Duration::from_secs(30);
/// "Try again later": the client fell behind and should reload.
const CLOSE_LAGGED: u16 = 1013;

pub async fn events(
    State(state): State<AppState>,
    Extension(CurrentDevice(device)): Extension<CurrentDevice>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |socket| run(state, device.id, socket))
}

async fn close(socket: &mut WebSocket, code: u16, reason: &'static str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}

/// What one socket listens to.
struct Feeds {
    events: broadcast::Receiver<Event>,
    revocations: broadcast::Receiver<Id>,
    shutdown: watch::Receiver<bool>,
    directed: mpsc::UnboundedReceiver<DirectedFrame>,
}

async fn run(state: AppState, device_id: String, mut socket: WebSocket) {
    let revocations = state.revocations.subscribe();
    let shutdown = state.shutdown.clone();
    // A revoke that landed between authenticating and subscribing.
    if !matches!(
        state.store.get::<DeviceRow>(db::DEVICE, &device_id).await,
        Ok(Some(_))
    ) {
        close(&mut socket, close_code::POLICY, "device revoked").await;
        return;
    }

    // Other devices hear that this one came online; it knows that itself.
    let (conn, directed, came_online) = state.devices.register(&device_id);
    if came_online {
        emit_presence(&state, &device_id).await;
    }
    // Subscribe before Hello so nothing between them is missed.
    let events = state.events.subscribe();
    let feeds = Feeds {
        events,
        revocations,
        shutdown,
        directed,
    };
    serve(&state, &device_id, &mut socket, feeds).await;
    if state.devices.unregister(&device_id, conn) {
        emit_presence(&state, &device_id).await;
    }
}

/// `DeviceChanged` with the device's presence now (nothing for a revoked device).
async fn emit_presence(state: &AppState, device_id: &str) {
    if let Ok(Some(row)) = state.store.get::<DeviceRow>(db::DEVICE, device_id).await {
        state.emit(Event::DeviceChanged {
            device: auth::to_device(&row, "", state.devices.is_online(device_id)),
        });
    }
}

/// The socket's life once registered: Hello, then events and directed frames out and
/// tool results in, until it closes.
async fn serve(state: &AppState, device_id: &str, socket: &mut WebSocket, mut feeds: Feeds) {
    let hello = Event::Hello {
        server_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    if send_json(socket, &hello).await.is_err() {
        return;
    }
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await;

    loop {
        tokio::select! {
            ev = feeds.events.recv() => match ev {
                Ok(ev) => {
                    if send_json(socket, &ev).await.is_err() {
                        return;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::info!("event socket fell {n} events behind; closing so the app reloads");
                    close(socket, CLOSE_LAGGED, "fell behind; reload").await;
                    return;
                }
                Err(RecvError::Closed) => {
                    close(socket, close_code::AWAY, "service stopping").await;
                    return;
                }
            },
            // `None` once the hub forgot the device (revoked): the revocation closes it.
            Some(frame) = feeds.directed.recv() => {
                if send_json(socket, &frame).await.is_err() {
                    return;
                }
            }
            revoked = feeds.revocations.recv() => match revoked {
                Ok(id) if id == device_id => {
                    close(socket, close_code::POLICY, "device revoked").await;
                    return;
                }
                Ok(_) => {}
                // Can't tell whether this device was revoked; the client re-authenticates on reconnect.
                Err(RecvError::Lagged(_)) => {
                    close(socket, CLOSE_LAGGED, "reconnect").await;
                    return;
                }
                Err(RecvError::Closed) => return,
            },
            _ = feeds.shutdown.changed() => {
                close(socket, close_code::AWAY, "service stopping").await;
                return;
            }
            _ = ping.tick() => {
                if socket.send(Message::Ping(Default::default())).await.is_err() {
                    return;
                }
            }
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(Message::Text(text))) => receive(state, device_id, text.as_str()),
                Some(Ok(_)) => {}
            },
        }
    }
}

/// A text frame from the device: a tool result goes to the hub; anything else is
/// ignored.
fn receive(state: &AppState, device_id: &str, text: &str) {
    match serde_json::from_str::<DeviceFrame>(text) {
        Ok(DeviceFrame::ToolResult { request_id, result }) => {
            state.devices.deliver(device_id, &request_id, result);
        }
        Err(e) => tracing::debug!(device = %device_id, "ignored a frame from the device: {e}"),
    }
}

async fn send_json(socket: &mut WebSocket, value: &impl serde::Serialize) -> Result<(), ()> {
    let text = serde_json::to_string(value).map_err(|_| ())?;
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| ())
}
