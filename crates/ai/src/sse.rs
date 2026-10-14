//! An incremental parser for `text/event-stream` (Server-Sent Events) bodies.
//!
//! Bytes arrive in arbitrary network chunks: a line, a field, or even a multi-byte
//! UTF-8 character can be split across two chunks. The parser buffers raw bytes and
//! only decodes complete lines, so splits anywhere are handled. Line endings may be
//! `\n`, `\r\n` or `\r` (as the SSE specification allows), and several `data:` lines
//! in one event are joined with `\n`.

/// One dispatched event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field, if the server sent one.
    pub event: Option<String>,
    /// The `data:` lines, joined with `\n`.
    pub data: String,
}

#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
    has_data: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds a chunk and returns the events it completed, in order.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut start = 0;
        let mut i = 0;
        while i < self.buf.len() {
            match self.buf[i] {
                b'\n' => {
                    let line = self.buf[start..i].to_vec();
                    self.line(&line, &mut out);
                    i += 1;
                    start = i;
                }
                b'\r' => {
                    if i + 1 == self.buf.len() {
                        // Can't tell yet whether a `\n` follows; wait for more bytes.
                        break;
                    }
                    let line = self.buf[start..i].to_vec();
                    self.line(&line, &mut out);
                    i += if self.buf[i + 1] == b'\n' { 2 } else { 1 };
                    start = i;
                }
                _ => i += 1,
            }
        }
        self.buf.drain(..start);
        out
    }

    /// Call when the body ends: dispatches a final event that lacked its blank line.
    pub fn finish(&mut self) -> Option<SseEvent> {
        let rest = std::mem::take(&mut self.buf);
        let mut out = Vec::new();
        if !rest.is_empty() {
            let rest = rest.strip_suffix(b"\r").unwrap_or(&rest).to_vec();
            self.line(&rest, &mut out);
        }
        self.line(b"", &mut out);
        out.pop()
    }

    fn line(&mut self, raw: &[u8], out: &mut Vec<SseEvent>) {
        if raw.is_empty() {
            if self.has_data {
                out.push(SseEvent {
                    event: self.event.take(),
                    data: self.data.join("\n"),
                });
            }
            self.event = None;
            self.data.clear();
            self.has_data = false;
            return;
        }
        let line = String::from_utf8_lossy(raw);
        if line.starts_with(':') {
            return; // comment / keep-alive
        }
        let (field, value) = match line.find(':') {
            Some(at) => {
                let value = &line[at + 1..];
                (&line[..at], value.strip_prefix(' ').unwrap_or(value))
            }
            None => (&line[..], ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => {
                self.data.push(value.to_string());
                self.has_data = true;
            }
            _ => {} // id, retry and unknown fields don't matter here
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = "event: response.output_text.delta\r\n\
data: {\"type\":\"response.output_text.delta\",\"delta\":\"h\u{e9}llo\"}\r\n\
\r\n\
: keep-alive\n\
\n\
event: multi\n\
data: line one\n\
data: line two\n\
\n\
data:no-space\r\r";

    fn expected() -> Vec<SseEvent> {
        vec![
            SseEvent {
                event: Some("response.output_text.delta".into()),
                data: "{\"type\":\"response.output_text.delta\",\"delta\":\"h\u{e9}llo\"}".into(),
            },
            SseEvent {
                event: Some("multi".into()),
                data: "line one\nline two".into(),
            },
            SseEvent {
                event: None,
                data: "no-space".into(),
            },
        ]
    }

    #[test]
    fn whole_body() {
        let mut p = SseParser::new();
        let mut got = p.push(BODY.as_bytes());
        got.extend(p.finish());
        assert_eq!(got, expected());
    }

    #[test]
    fn every_split_point() {
        let bytes = BODY.as_bytes();
        for a in 0..=bytes.len() {
            let mut p = SseParser::new();
            let mut got = p.push(&bytes[..a]);
            got.extend(p.push(&bytes[a..]));
            got.extend(p.finish());
            assert_eq!(got, expected(), "split at {a}");
        }
    }

    #[test]
    fn byte_by_byte() {
        let mut p = SseParser::new();
        let mut got = Vec::new();
        for b in BODY.as_bytes() {
            got.extend(p.push(std::slice::from_ref(b)));
        }
        got.extend(p.finish());
        assert_eq!(got, expected());
    }

    #[test]
    fn unterminated_final_event_is_dispatched() {
        let mut p = SseParser::new();
        assert!(p.push(b"data: tail").is_empty());
        assert_eq!(
            p.finish(),
            Some(SseEvent {
                event: None,
                data: "tail".into()
            })
        );
    }
}
