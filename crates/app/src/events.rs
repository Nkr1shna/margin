//! The `/api/events` WebSocket: connect with the device token as a Bearer header, read
//! JSON `ServerFrame`s (broadcast `Event`s, and `DirectedFrame`s for this device), send
//! `DeviceFrame`s (device-tool answers), and back off between reconnect attempts.

use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use margin_shared::{DeviceFrame, ServerFrame};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, header::AUTHORIZATION};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
/// The socket's read half. The live loop splits the socket once connected and owns both
/// halves.
pub type Frames = SplitStream<Socket>;
/// The socket's write half.
pub type Sender = SplitSink<Socket, Message>;

#[derive(Debug, Clone, PartialEq)]
pub enum ConnectError {
    /// The service refused the token (HTTP 401 on the upgrade).
    Unauthorized,
    Other(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Unauthorized => f.write_str("This device is no longer signed in."),
            ConnectError::Other(msg) => f.write_str(msg),
        }
    }
}

fn tls_connector() -> Connector {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("aws-lc-rs supports the default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Connector::Rustls(Arc::new(config))
}

pub async fn connect(events_url: &str, token: &str) -> Result<Socket, ConnectError> {
    let mut request = events_url
        .into_client_request()
        .map_err(|e| ConnectError::Other(format!("Bad events address: {e}")))?;
    let value = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| ConnectError::Other("The device token is not valid text.".into()))?;
    request.headers_mut().insert(AUTHORIZATION, value);
    let connector = events_url.starts_with("wss://").then(tls_connector);
    match tokio_tungstenite::connect_async_tls_with_config(request, None, false, connector).await {
        Ok((socket, _response)) => Ok(socket),
        Err(WsError::Http(response)) if response.status().as_u16() == 401 => {
            Err(ConnectError::Unauthorized)
        }
        Err(WsError::Http(response)) => Err(ConnectError::Other(format!(
            "The live updates connection was refused ({}).",
            response.status().as_u16()
        ))),
        Err(e) => Err(ConnectError::Other(format!(
            "Live updates are disconnected ({e})."
        ))),
    }
}

/// Next frame from the socket: an event for every device, or a frame addressed to this
/// one (a device-tool request or its cancellation). `None` when the socket closed or
/// failed; frames that parse as neither (from a newer service) are skipped. Pings are
/// answered by tungstenite itself (queued, and flushed with the next frame read or
/// sent). Cancel-safe: it only awaits the stream's `next()`.
pub async fn next_frame(frames: &mut Frames) -> Option<ServerFrame> {
    loop {
        match frames.next().await? {
            Ok(Message::Text(text)) => match serde_json::from_str::<ServerFrame>(text.as_str()) {
                Ok(frame) => return Some(frame),
                Err(_) => continue,
            },
            Ok(Message::Close(_)) => return None,
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

/// Send one frame to the service as JSON text.
pub async fn send_frame(sender: &mut Sender, frame: &DeviceFrame) -> Result<(), WsError> {
    let json = serde_json::to_string(frame).expect("device frames serialize");
    sender.send(Message::Text(json.into())).await
}

/// Close the socket with a Close frame, so the service drops this device's presence at
/// once instead of when its pings go unanswered.
pub async fn close(sender: &mut Sender) {
    let _ = sender.send(Message::Close(None)).await;
    let _ = sender.close().await;
}

/// Delay before reconnect attempt `attempt` (0-based): 1 s, 2 s, 4 s … capped at 30 s.
pub fn backoff(attempt: u32) -> Duration {
    let secs = 1u64 << attempt.min(5);
    Duration::from_secs(secs.min(30))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Api;
    use crate::fake::{FakeService, TOKEN};
    use margin_shared::{DeviceToolCall, DirectedFrame, Event, NewNote, ServerFrame, ToolRequest};

    /// The next broadcast event, skipping frames addressed to this device.
    async fn next_event(socket: &mut Frames) -> Option<Event> {
        loop {
            match next_frame(socket).await? {
                ServerFrame::Event(event) => return Some(event),
                ServerFrame::Directed(_) => continue,
            }
        }
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff(0), Duration::from_secs(1));
        assert_eq!(backoff(1), Duration::from_secs(2));
        assert_eq!(backoff(4), Duration::from_secs(16));
        assert_eq!(backoff(5), Duration::from_secs(30));
        assert_eq!(backoff(50), Duration::from_secs(30));
    }

    #[test]
    fn builds_tls_connector() {
        assert!(matches!(tls_connector(), Connector::Rustls(_)));
    }

    #[tokio::test]
    async fn receives_hello_and_live_events() {
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, TOKEN);
        let (_sender, mut socket) = connect(&api.events_url(), TOKEN).await.unwrap().split();
        assert_eq!(
            next_event(&mut socket).await,
            Some(Event::Hello {
                server_version: "fake-0.1".into()
            })
        );
        let note = api
            .create_note(&NewNote {
                topic_id: "fourier".into(),
                lesson_id: None,
                title: "Live".into(),
                body_md: String::new(),
                quote: None,
            })
            .await
            .unwrap();
        assert_eq!(
            next_event(&mut socket).await,
            Some(Event::NoteChanged { note })
        );
    }

    #[tokio::test]
    async fn frames_for_this_device_arrive_between_events() {
        let fake = FakeService::start().await;
        let (_sender, mut socket) = connect(&Api::with_token(&fake.url, TOKEN).events_url(), TOKEN)
            .await
            .unwrap()
            .split();
        assert!(matches!(
            next_frame(&mut socket).await,
            Some(ServerFrame::Event(Event::Hello { .. }))
        ));
        let request = DirectedFrame::ToolRequest {
            request: ToolRequest {
                request_id: "r1".into(),
                chat_id: "c1".into(),
                call_id: "call_1".into(),
                deadline: chrono::Utc::now(),
                call: DeviceToolCall::ToggleSidebar { visible: None },
            },
        };
        fake.direct(request.clone());
        fake.direct(DirectedFrame::ToolCancel {
            request_id: "r1".into(),
        });
        assert_eq!(
            next_frame(&mut socket).await,
            Some(ServerFrame::Directed(request))
        );
        assert!(matches!(
            next_frame(&mut socket).await,
            Some(ServerFrame::Directed(DirectedFrame::ToolCancel { .. }))
        ));
        // The socket goes on with events after them.
        let _ = fake.events.send(Event::ChatDeleted { id: "c1".into() });
        assert_eq!(
            next_frame(&mut socket).await,
            Some(ServerFrame::Event(Event::ChatDeleted { id: "c1".into() }))
        );
    }

    /// A WebSocket server that reports what a device sends it: each text frame, then
    /// `None` when the device closes.
    async fn listening_server() -> (String, tokio::sync::mpsc::UnboundedReceiver<Option<String>>) {
        use axum::extract::ws::{Message as AxumMessage, WebSocketUpgrade};
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let app = axum::Router::new().route(
            "/api/events",
            axum::routing::get(move |ws: WebSocketUpgrade| {
                let tx = tx.clone();
                async move {
                    ws.on_upgrade(move |mut socket| async move {
                        while let Some(Ok(message)) = socket.recv().await {
                            match message {
                                AxumMessage::Text(text) => {
                                    let _ = tx.send(Some(text.to_string()));
                                }
                                AxumMessage::Close(_) => break,
                                _ => {}
                            }
                        }
                        let _ = tx.send(None);
                    })
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/api/events", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, rx)
    }

    #[tokio::test]
    async fn sends_device_frames_and_closes() {
        let (url, mut received) = listening_server().await;
        let (mut sender, _frames) = connect(&url, TOKEN).await.unwrap().split();
        let frame = DeviceFrame::ToolResult {
            request_id: "r1".into(),
            result: margin_shared::DeviceToolResult::Err {
                message: "nope".into(),
            },
        };
        send_frame(&mut sender, &frame).await.unwrap();
        let text = received.recv().await.unwrap().unwrap();
        assert_eq!(serde_json::from_str::<DeviceFrame>(&text).unwrap(), frame);
        close(&mut sender).await;
        assert_eq!(received.recv().await, Some(None));
    }

    #[tokio::test]
    async fn rejects_bad_token() {
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, "nope");
        let err = connect(&api.events_url(), "nope").await.unwrap_err();
        assert_eq!(err, ConnectError::Unauthorized);
    }
}
