//! `/api/events`: live `Event`s as JSON text frames, plus the frames directed at this
//! device (`DirectedFrame`: tool requests and cancels) and the device's answers
//! (`DeviceFrame::ToolResult`, handed to `devices::DeviceHub::deliver`). While a socket
//! is open its device counts as online (`Event::DeviceChanged`).
//!
//! Each socket sends only the events its principal may see (`Audience::allows`, L1 D8).
//! The principal is the one `require_principal` built at upgrade, rebuilt whenever the
//! expansions are invalidated (`groups::Expansions::watch`), so a person removed from a
//! group stops receiving that group's events without reconnecting.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{Query, State};
use axum::response::Response;
use margin_shared::context::TraceRow;
use margin_shared::{DeviceFrame, DirectedFrame, Event, Id};
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc, watch};

use crate::auth;
use crate::principal::{BOOTSTRAP_PERSON_ID, CurrentDevice, CurrentPrincipal, Principal};
use crate::state::{AppState, Audience, DeviceRow, Emitted};
use crate::{db, groups};

const PING_INTERVAL: Duration = Duration::from_secs(30);
/// "Try again later": the client fell behind and should reload.
const CLOSE_LAGGED: u16 = 1013;

/// `?traces=1`: also send `Event::Trace` for `model_call` and `retrieval` rows as they
/// are written (nothing is pushed by default). Admins only: anyone else's socket opens
/// without them (owner Q6).
#[derive(Debug, Default, Deserialize)]
pub struct EventsQuery {
    #[serde(default)]
    pub traces: Option<String>,
}

pub async fn events(
    State(state): State<AppState>,
    CurrentPrincipal(principal): CurrentPrincipal,
    CurrentDevice(device): CurrentDevice,
    Query(q): Query<EventsQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let traces = matches!(q.traces.as_deref(), Some("1" | "true"));
    ws.on_upgrade(move |socket| run(state, device.id, principal, traces, socket))
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
    events: broadcast::Receiver<Emitted>,
    revocations: broadcast::Receiver<Id>,
    shutdown: watch::Receiver<bool>,
    directed: mpsc::UnboundedReceiver<DirectedFrame>,
    /// Trace rows, when the socket asked for them (sent while the principal is an
    /// admin).
    traces: Option<broadcast::Receiver<TraceRow>>,
    /// Changes whenever expansions are invalidated: the principal is rebuilt.
    access: watch::Receiver<u64>,
}

async fn run(
    state: AppState,
    device_id: String,
    principal: Arc<Principal>,
    traces: bool,
    mut socket: WebSocket,
) {
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

    // Watch before subscribing, so a change in between still rebuilds the principal.
    let access = state.expansions.watch();
    // Read the access once more after watching: a change since `require_principal`
    // built the principal is then never missed.
    let principal = Arc::new(re_expand(&state, &principal).await);
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
        traces: (traces && principal.is_admin()).then(|| state.tracer.subscribe()),
        access,
    };
    serve(&state, &device_id, principal, &mut socket, feeds).await;
    if state.devices.unregister(&device_id, conn) {
        emit_presence(&state, &device_id).await;
    }
}

/// `DeviceChanged` with the device's presence now (nothing for a revoked device), to
/// its person's devices and the admins.
async fn emit_presence(state: &AppState, device_id: &str) {
    if let Ok(Some(row)) = state.store.get::<DeviceRow>(db::DEVICE, device_id).await {
        state.emit_to(
            device_audience(&row),
            Event::DeviceChanged {
                device: auth::to_device(&row, "", state.devices.is_online(device_id)),
            },
        );
    }
}

/// Who hears about a device (`DeviceChanged`, `DeviceDeleted`): its person and the
/// admins.
pub fn device_audience(row: &DeviceRow) -> Audience {
    Audience::PersonOrAdmins(
        row.person_id
            .clone()
            .unwrap_or_else(|| BOOTSTRAP_PERSON_ID.to_string()),
    )
}

/// `principal` with its expansion read again (memberships or role changed). Only a
/// person holds a socket. When the expansion can't be read the socket keeps nothing
/// but the person's own id until the next change: it fails closed.
pub async fn re_expand(state: &AppState, principal: &Principal) -> Principal {
    let mut p = principal.clone();
    match groups::expansion(state, &principal.person_id).await {
        Ok(e) => {
            p.paths = e.paths.clone();
            p.deny_ids = e.deny_ids.clone();
            p.org_role = e.org_role;
        }
        Err(e) => {
            tracing::warn!(
                person = %principal.person_id,
                "couldn't re-read a socket's access: {}",
                e.message
            );
            p.paths = vec![principal.id.clone()];
            p.deny_ids = p.paths.clone();
            p.org_role = margin_shared::OrgRole::Member;
        }
    }
    p
}

/// The socket's life once registered: Hello, then events and directed frames out and
/// tool results in, until it closes.
async fn serve(
    state: &AppState,
    device_id: &str,
    mut principal: Arc<Principal>,
    socket: &mut WebSocket,
    mut feeds: Feeds,
) {
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
                // A filtered-out event is still consumed, so lag accounting is as before.
                Ok(ev) => {
                    if ev.audience.allows(&principal)
                        && send_json(socket, &ev.event).await.is_err()
                    {
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
            changed = feeds.access.changed() => {
                // The sender lives as long as the service.
                if changed.is_err() {
                    return;
                }
                feeds.access.mark_unchanged();
                principal = Arc::new(re_expand(state, &principal).await);
            }
            row = next_trace(&mut feeds.traces) => match row {
                Ok(row) => {
                    // Traces are for admins (a demoted admin's socket stops getting them).
                    if principal.is_admin()
                        && send_json(socket, &Event::Trace { row: Box::new(row) }).await.is_err()
                    {
                        return;
                    }
                }
                // Traces are a best-effort tail: a slow socket skips what it missed.
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => feeds.traces = None,
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

/// The next trace row, or never when the socket didn't ask for them.
async fn next_trace(
    traces: &mut Option<broadcast::Receiver<TraceRow>>,
) -> Result<TraceRow, RecvError> {
    match traces {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
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
