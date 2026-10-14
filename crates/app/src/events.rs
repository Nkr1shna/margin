//! The `/api/events` WebSocket: connect with the device token as a Bearer header, read
//! JSON `ServerFrame`s (broadcast `Event`s, and `DirectedFrame`s for this device), and
//! back off between reconnect attempts.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use margin_shared::ServerFrame;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, header::AUTHORIZATION};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

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
/// answered by tungstenite itself. Cancel-safe: it only awaits the stream's `next()`.
pub async fn next_frame(socket: &mut Socket) -> Option<ServerFrame> {
    loop {
        match socket.next().await? {
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
    async fn next_event(socket: &mut Socket) -> Option<Event> {
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
        let mut socket = connect(&api.events_url(), TOKEN).await.unwrap();
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
        let mut socket = connect(&Api::with_token(&fake.url, TOKEN).events_url(), TOKEN)
            .await
            .unwrap();
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

    #[tokio::test]
    async fn rejects_bad_token() {
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, "nope");
        let err = connect(&api.events_url(), "nope").await.unwrap_err();
        assert_eq!(err, ConnectError::Unauthorized);
    }
}
