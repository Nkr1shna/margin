//! `/api/events`: live `Event`s as JSON text frames.

use std::time::Duration;

use axum::Extension;
use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::response::Response;
use margin_shared::Event;
use tokio::sync::broadcast::error::RecvError;

use crate::auth::CurrentDevice;
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

async fn run(state: AppState, device_id: String, mut socket: WebSocket) {
    // Subscribe before Hello so nothing between them is missed.
    let mut events = state.events.subscribe();
    let mut revocations = state.revocations.subscribe();
    let mut shutdown = state.shutdown.clone();
    // A revoke that landed between authenticating and subscribing.
    if !matches!(
        state.store.get::<DeviceRow>(db::DEVICE, &device_id).await,
        Ok(Some(_))
    ) {
        close(&mut socket, close_code::POLICY, "device revoked").await;
        return;
    }

    let hello = Event::Hello {
        server_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    if send_event(&mut socket, &hello).await.is_err() {
        return;
    }
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await;

    loop {
        tokio::select! {
            ev = events.recv() => match ev {
                Ok(ev) => {
                    if send_event(&mut socket, &ev).await.is_err() {
                        return;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::info!("event socket fell {n} events behind; closing so the app reloads");
                    close(&mut socket, CLOSE_LAGGED, "fell behind; reload").await;
                    return;
                }
                Err(RecvError::Closed) => {
                    close(&mut socket, close_code::AWAY, "service stopping").await;
                    return;
                }
            },
            revoked = revocations.recv() => match revoked {
                Ok(id) if id == device_id => {
                    close(&mut socket, close_code::POLICY, "device revoked").await;
                    return;
                }
                Ok(_) => {}
                // Can't tell whether this device was revoked; the client re-authenticates on reconnect.
                Err(RecvError::Lagged(_)) => {
                    close(&mut socket, CLOSE_LAGGED, "reconnect").await;
                    return;
                }
                Err(RecvError::Closed) => return,
            },
            _ = shutdown.changed() => {
                close(&mut socket, close_code::AWAY, "service stopping").await;
                return;
            }
            _ = ping.tick() => {
                if socket.send(Message::Ping(Default::default())).await.is_err() {
                    return;
                }
            }
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {}
            },
        }
    }
}

async fn send_event(socket: &mut WebSocket, ev: &Event) -> Result<(), ()> {
    let text = serde_json::to_string(ev).map_err(|_| ())?;
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| ())
}
