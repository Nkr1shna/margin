//! The desktop app's half of "Sign in with ChatGPT" (`docs/agent-siwc-spike.md`,
//! "Signing in when the service is remote"). ChatGPT's sign-in only redirects to
//! `http://127.0.0.1:<port>/auth/callback`, which reaches the device whose browser
//! signs in, not the service. So during a sign-in the app listens there itself: the one
//! request carrying the sign-in's `state` is the callback, and its address (code and
//! state, no token) goes to the service's `complete` route, which holds the PKCE
//! verifier, exchanges the code and keeps the tokens. The browser tab is answered with
//! a short page once the service has answered.
//!
//! Phones don't run this (an app in the background can't count on listening): there the
//! person pastes the address the browser ends on.

use std::time::Duration;

use margin_shared::{CHATGPT_CALLBACK_PATH, CHATGPT_CALLBACK_PORT};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// How long the app listens for the callback (as long as the service keeps the
/// sign-in's state).
pub const WAIT: Duration = Duration::from_secs(10 * 60);
/// How long one connection may take to send its request line and headers.
const READ_WAIT: Duration = Duration::from_secs(5);
/// The most of a request read.
const MAX_REQUEST: usize = 16 * 1024;

/// The loopback listener for the callback: on `CHATGPT_CALLBACK_PORT`, else on a free
/// port (OpenAI's sign-in lets the port vary). `None` when neither binds: the person
/// pastes the address instead.
pub async fn bind() -> Option<TcpListener> {
    match TcpListener::bind(("127.0.0.1", CHATGPT_CALLBACK_PORT)).await {
        Ok(listener) => Some(listener),
        Err(_) => TcpListener::bind(("127.0.0.1", 0)).await.ok(),
    }
}

/// The callback's request, held open until the service answers.
pub struct Callback {
    /// The address the browser came back to (`http://127.0.0.1:<port>/auth/callback?…`).
    /// It carries the authorization code: never logged.
    pub url: String,
    stream: TcpStream,
}

impl Callback {
    /// Answers the browser tab with a page saying how the sign-in went.
    pub async fn finish(mut self, ok: bool, title: &str, text: &str) {
        let status = if ok { "200 OK" } else { "400 Bad Request" };
        let _ = write_page(&mut self.stream, status, title, text).await;
    }
}

/// Waits up to `wait` for the callback of the sign-in whose state is `state`. Other
/// requests (a favicon, an old tab's callback) are answered and skipped.
pub async fn accept(listener: &TcpListener, state: &str, wait: Duration) -> Option<Callback> {
    let port = listener.local_addr().ok()?.port();
    let found = async {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            };
            let Ok(Some(target)) = tokio::time::timeout(READ_WAIT, read_target(&mut stream)).await
            else {
                continue;
            };
            let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
            if path != CHATGPT_CALLBACK_PATH {
                let _ = write_page(&mut stream, "404 Not Found", "Not found", "").await;
                continue;
            }
            if query_value(query, "state").as_deref() != Some(state) {
                let _ = write_page(
                    &mut stream,
                    "400 Bad Request",
                    "This sign-in link is out of date",
                    "Start again from Margin's Settings → AI → ChatGPT.",
                )
                .await;
                continue;
            }
            return Callback {
                url: format!("http://127.0.0.1:{port}{target}"),
                stream,
            };
        }
    };
    tokio::time::timeout(wait, found).await.ok()
}

/// The target of a `GET` request (`/auth/callback?…`), from its request line.
async fn read_target(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 || buf.len() + n > MAX_REQUEST {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let head = std::str::from_utf8(&buf).ok()?;
    let mut parts = head.lines().next()?.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some("GET"), Some(target)) if target.starts_with('/') => Some(target.to_string()),
        _ => None,
    }
}

/// One query parameter, percent-decoded.
fn query_value(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (decode(k) == name).then(|| decode(v))
    })
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => match bytes
                .get(i + 1..i + 3)
                .and_then(|hex| std::str::from_utf8(hex).ok())
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            {
                Some(b) => {
                    out.push(b);
                    i += 2;
                }
                None => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn write_page(
    stream: &mut TcpStream,
    status: &str,
    title: &str,
    text: &str,
) -> std::io::Result<()> {
    let (title, text) = (escape(title), escape(text));
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\">\
         <title>{title}</title><body style=\"font-family:-apple-system,system-ui,sans-serif;margin:3em\">\
         <h1 style=\"font-size:1.4em\">{title}</h1><p>{text}</p></body>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn get(port: u16, target: &str) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        let code = out[9..12].parse().unwrap();
        (code, out)
    }

    #[tokio::test]
    async fn the_callback_with_the_state_is_taken_and_answered_after() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let waiting = tokio::spawn(async move { accept(&listener, "st+1", WAIT).await });
        // Other paths and an old tab's state are answered and skipped.
        assert_eq!(get(port, "/favicon.ico").await.0, 404);
        let (code, page) = get(port, "/auth/callback?code=old&state=st-0").await;
        assert_eq!(code, 400);
        assert!(page.contains("out of date"), "{page}");
        // The sign-in's own callback (state percent-encoded) is held until answered.
        let browser = tokio::spawn(get(port, "/auth/callback?code=c1&state=st%2B1&scope=a+b"));
        let cb = waiting.await.unwrap().expect("the callback");
        assert_eq!(
            cb.url,
            format!("http://127.0.0.1:{port}/auth/callback?code=c1&state=st%2B1&scope=a+b")
        );
        cb.finish(true, "Signed in <ok>", "You can close this tab.")
            .await;
        let (code, page) = browser.await.unwrap();
        assert_eq!(code, 200);
        assert!(page.contains("Signed in &lt;ok&gt;"), "{page}");
    }

    #[tokio::test]
    async fn no_callback_in_time_is_none() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        assert!(
            accept(&listener, "s", Duration::from_millis(50))
                .await
                .is_none()
        );
    }

    #[test]
    fn query_values_are_decoded() {
        assert_eq!(
            query_value("a=1&state=x%2By+z", "state").as_deref(),
            Some("x+y z")
        );
        assert_eq!(query_value("a=1", "state"), None);
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("%zz"), "%zz");
    }
}
