//! Text generation for Margin through an OpenAI-compatible **Responses API**
//! (`POST {base_url}/responses`). No other API shape is supported.
//!
//! The service owns configuration and the API key; this crate is stateless and
//! knows nothing about storage.
//!
//! Every request is sent with `store: false`. Plain text is streamed (Server-Sent
//! Events); structured output is a single JSON response using a strict
//! `json_schema` text format.

use std::time::Duration;

use futures_util::StreamExt;
use margin_shared::{AskRequest, Level, Outline, OutlineLesson};
use serde_json::{Value, json};

mod clean;
mod prompts;
mod sse;

#[derive(Debug, thiserror::Error)]
pub enum AiError {
    /// The API answered with a non-2xx status. `message` is the API's error message
    /// when it sent one.
    #[error("the model API returned {status}: {message}")]
    Http { status: u16, message: String },
    #[error("couldn't reach the model API: {0}")]
    Network(String),
    /// The response could not be parsed, or structured output didn't match the schema.
    #[error("the model's answer couldn't be read: {0}")]
    Parse(String),
    /// The model refused or the response ended as `incomplete`/`failed`.
    #[error("the model didn't finish: {0}")]
    Incomplete(String),
}

/// How long to wait for a TCP + TLS connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// A stalled connection: no bytes at all for this long. Streams have no overall limit;
/// the Responses API sends events (and keep-alives) while the model works.
const READ_TIMEOUT: Duration = Duration::from_secs(300);
/// Overall limit for one non-streaming request (structured output).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

/// A configured Responses API endpoint. Cheap to clone.
#[derive(Clone)]
pub struct ResponsesClient {
    http: Result<reqwest::Client, String>,
    url: String,
    api_key: String,
    model: String,
}

impl std::fmt::Debug for ResponsesClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponsesClient")
            .field("url", &self.url)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl ResponsesClient {
    /// `base_url` like "https://api.openai.com/v1" (no trailing `/responses`).
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        let base_url = base_url.into();
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .map_err(|e| format!("couldn't set up the HTTP client: {}", error_chain(&e)));
        Self {
            http,
            url: format!("{}/responses", base_url.trim().trim_end_matches('/')),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    fn request(&self, body: &Value) -> Result<reqwest::RequestBuilder, AiError> {
        let http = self
            .http
            .as_ref()
            .map_err(|e| AiError::Network(e.clone()))?;
        Ok(http
            .post(&self.url)
            .bearer_auth(&self.api_key)
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .json(body))
    }

    fn body(&self, instructions: &str, input: &str, stream: bool) -> Value {
        json!({
            "model": self.model,
            "instructions": instructions,
            "input": input,
            "stream": stream,
            "store": false,
        })
    }

    /// Streams a plain-text answer (`stream: true`), calling `on_delta` with each
    /// `response.output_text.delta`. Returns the full text.
    pub async fn stream_text(
        &self,
        instructions: &str,
        input: &str,
        mut on_delta: impl FnMut(&str) + Send,
    ) -> Result<String, AiError> {
        let body = self.body(instructions, input, true);
        let resp = self.request(&body)?.send().await.map_err(network)?;
        let resp = check_status(resp).await?;

        let mut parser = sse::SseParser::new();
        let mut state = StreamState::default();
        let mut bytes = resp.bytes_stream();
        while let Some(chunk) = bytes.next().await {
            let chunk = chunk.map_err(network)?;
            for event in parser.push(&chunk) {
                if let Some(done) = state.handle(&event, &mut on_delta)? {
                    return Ok(done);
                }
            }
        }
        if let Some(event) = parser.finish()
            && let Some(done) = state.handle(&event, &mut on_delta)?
        {
            return Ok(done);
        }
        Err(AiError::Network(
            "the connection closed before the response finished".into(),
        ))
    }

    /// Non-streaming structured output (`text.format = {type: "json_schema", strict: true}`),
    /// deserialized into `T`.
    pub async fn structured<T: serde::de::DeserializeOwned>(
        &self,
        instructions: &str,
        input: &str,
        schema_name: &str,
        schema: serde_json::Value,
    ) -> Result<T, AiError> {
        let mut body = self.body(instructions, input, false);
        body["text"] = json!({
            "format": {
                "type": "json_schema",
                "name": schema_name,
                "schema": schema,
                "strict": true,
            }
        });
        let resp = self
            .request(&body)?
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(network)?;
        let resp = check_status(resp).await?;
        let raw = resp.bytes().await.map_err(network)?;
        let response: Value = serde_json::from_slice(&raw)
            .map_err(|e| AiError::Parse(format!("the response isn't JSON: {e}")))?;
        let text = response_text(&response)?;
        serde_json::from_str(&text).map_err(|e| {
            AiError::Parse(format!(
                "the structured answer doesn't match the {schema_name} schema: {e}"
            ))
        })
    }
}

// ---------------------------------------------------------------------------
// Transport helpers
// ---------------------------------------------------------------------------

/// An error and its sources, e.g. "error sending request: client error (Connect):
/// connection refused".
fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut msg = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        let text = s.to_string();
        if !msg.contains(&text) {
            msg.push_str(": ");
            msg.push_str(&text);
        }
        source = s.source();
    }
    msg
}

fn network(e: reqwest::Error) -> AiError {
    if e.is_timeout() {
        AiError::Network(format!("the request timed out ({})", error_chain(&e)))
    } else if e.is_decode() {
        AiError::Parse(error_chain(&e))
    } else {
        AiError::Network(error_chain(&e))
    }
}

/// Passes 2xx responses through; turns anything else into `AiError::Http` with the
/// API's `{error: {message}}` when present.
async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response, AiError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let message = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| error_message(&v))
        .unwrap_or_else(|| {
            let trimmed = body.trim();
            if trimmed.is_empty() {
                status
                    .canonical_reason()
                    .unwrap_or("no message")
                    .to_string()
            } else {
                truncate(trimmed, 500)
            }
        });
    Err(AiError::Http {
        status: status.as_u16(),
        message,
    })
}

/// `{error: {message}}`, `{error: "text"}`, or `{message}`.
fn error_message(v: &Value) -> Option<String> {
    let e = v.get("error").unwrap_or(v);
    if let Some(s) = e.as_str() {
        return Some(s.to_string());
    }
    e.get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| e.get("code").and_then(Value::as_str).map(str::to_string))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

// ---------------------------------------------------------------------------
// Responses API payloads
// ---------------------------------------------------------------------------

/// Why a response that ended as `incomplete` stopped.
fn incomplete_reason(response: &Value) -> String {
    let reason = response
        .pointer("/incomplete_details/reason")
        .and_then(Value::as_str);
    match reason {
        Some("max_output_tokens") => "the answer hit the output token limit".into(),
        Some("content_filter") => "the answer was stopped by the content filter".into(),
        Some(other) => format!("the response is incomplete ({other})"),
        None => "the response is incomplete".into(),
    }
}

fn failed_reason(response: &Value) -> String {
    response
        .get("error")
        .and_then(error_message)
        .map(|m| format!("the response failed: {m}"))
        .unwrap_or_else(|| "the response failed".into())
}

/// The text of a finished response object: every `output_text` part of every
/// `message` item, in order. A refusal or a non-completed status is an error.
fn response_text(response: &Value) -> Result<String, AiError> {
    match response.get("status").and_then(Value::as_str) {
        Some("incomplete") => return Err(AiError::Incomplete(incomplete_reason(response))),
        Some("failed") => return Err(AiError::Incomplete(failed_reason(response))),
        Some("cancelled") => return Err(AiError::Incomplete("the response was cancelled".into())),
        _ => {}
    }
    if let Some(err) = response.get("error").filter(|e| !e.is_null()) {
        let msg = error_message(err).unwrap_or_else(|| err.to_string());
        return Err(AiError::Incomplete(format!("the response failed: {msg}")));
    }
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| AiError::Parse("the response has no output".into()))?;
    let mut text = String::new();
    let mut found = false;
    for item in output {
        if item.get("type").and_then(Value::as_str) != Some("message") {
            continue; // reasoning, tool calls, …
        }
        for part in item
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match part.get("type").and_then(Value::as_str) {
                Some("output_text") => {
                    found = true;
                    text.push_str(part.get("text").and_then(Value::as_str).unwrap_or(""));
                }
                Some("refusal") => {
                    let why = part.get("refusal").and_then(Value::as_str).unwrap_or("");
                    return Err(AiError::Incomplete(refused(why)));
                }
                _ => {}
            }
        }
    }
    if !found {
        return Err(AiError::Parse("the response has no output text".into()));
    }
    Ok(text)
}

fn refused(why: &str) -> String {
    let why = why.trim();
    if why.is_empty() {
        "the model refused".into()
    } else {
        format!("the model refused: {why}")
    }
}

/// State of one streamed response.
#[derive(Default)]
struct StreamState {
    text: String,
    refusal: String,
}

impl StreamState {
    /// Handles one SSE event. `Ok(Some(text))` when the response completed.
    fn handle(
        &mut self,
        event: &sse::SseEvent,
        on_delta: &mut (impl FnMut(&str) + Send),
    ) -> Result<Option<String>, AiError> {
        let data = event.data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(None);
        }
        let v: Value = serde_json::from_str(data)
            .map_err(|e| AiError::Parse(format!("a stream event isn't JSON: {e}")))?;
        let kind = v
            .get("type")
            .and_then(Value::as_str)
            .or(event.event.as_deref())
            .unwrap_or("");
        match kind {
            "response.output_text.delta" => {
                if let Some(delta) = v.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    self.text.push_str(delta);
                    on_delta(delta);
                }
            }
            "response.refusal.delta" => {
                if let Some(delta) = v.get("delta").and_then(Value::as_str) {
                    self.refusal.push_str(delta);
                }
            }
            "response.refusal.done" => {
                let why = v
                    .get("refusal")
                    .and_then(Value::as_str)
                    .unwrap_or(&self.refusal);
                return Err(AiError::Incomplete(refused(why)));
            }
            "response.completed" => {
                if !self.refusal.is_empty() {
                    return Err(AiError::Incomplete(refused(&self.refusal)));
                }
                if self.text.is_empty()
                    && let Some(response) = v.get("response")
                {
                    // Some compatible servers send the text only in the final object.
                    let text = response_text(response)?;
                    if !text.is_empty() {
                        on_delta(&text);
                    }
                    return Ok(Some(text));
                }
                return Ok(Some(std::mem::take(&mut self.text)));
            }
            "response.incomplete" => {
                let response = v.get("response").unwrap_or(&Value::Null);
                return Err(AiError::Incomplete(incomplete_reason(response)));
            }
            "response.failed" => {
                let response = v.get("response").unwrap_or(&Value::Null);
                return Err(AiError::Incomplete(failed_reason(response)));
            }
            "error" => {
                let msg = error_message(&v).unwrap_or_else(|| "unknown error".into());
                return Err(AiError::Incomplete(format!(
                    "the stream reported an error: {msg}"
                )));
            }
            _ => {} // created, in_progress, output_item.*, content_part.*, reasoning, …
        }
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Margin's generation tasks
// ---------------------------------------------------------------------------

/// An excerpt of one of the learner's sources, retrieved because it is relevant to what
/// is being written. Outlines and lessons are based on these where they cover the topic.
#[derive(Debug, Clone, PartialEq)]
pub struct Passage {
    /// The source's name, e.g. "notes/ch1.md".
    pub source: String,
    /// PDF page the passage is on.
    pub page: Option<u32>,
    pub text: String,
}

/// Proposes a course outline. `existing_subjects` are the titles of root topics, so the
/// model can file the topic under one of them (it may also invent a new subject).
/// `passages` are the excerpts of the learner's sources most relevant to the request
/// (empty without sources). The result has exactly `req.lesson_count` lessons.
pub async fn propose_outline(
    client: &ResponsesClient,
    req: &AskRequest,
    existing_subjects: &[String],
    passages: &[Passage],
) -> Result<Outline, AiError> {
    let outline: Outline = client
        .structured(
            &prompts::outline_instructions(req.lesson_count, req.level, !passages.is_empty()),
            &prompts::outline_input(req, existing_subjects, passages),
            prompts::OUTLINE_SCHEMA_NAME,
            prompts::outline_schema(),
        )
        .await?;
    validate_outline(outline, req.lesson_count)
}

/// Checks the model's outline: the requested number of lessons, no empty text, a
/// positive reading time. Whitespace around fields is trimmed and summaries are kept
/// to one line.
fn validate_outline(outline: Outline, lesson_count: u32) -> Result<Outline, AiError> {
    let one_line = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let outline = Outline {
        topic_title: one_line(&outline.topic_title),
        subject_title: one_line(&outline.subject_title),
        lessons: outline
            .lessons
            .into_iter()
            .map(|l| OutlineLesson {
                title: one_line(&l.title),
                summary: one_line(&l.summary),
                minutes: l.minutes,
            })
            .collect(),
    };
    if outline.lessons.len() != lesson_count as usize {
        return Err(AiError::Parse(format!(
            "asked for {lesson_count} lessons but the outline has {}",
            outline.lessons.len()
        )));
    }
    if outline.topic_title.is_empty() {
        return Err(AiError::Parse("the outline has no topic title".into()));
    }
    if outline.subject_title.is_empty() {
        return Err(AiError::Parse("the outline has no subject title".into()));
    }
    for (i, l) in outline.lessons.iter().enumerate() {
        if l.title.is_empty() {
            return Err(AiError::Parse(format!("lesson {} has no title", i + 1)));
        }
        if l.summary.is_empty() {
            return Err(AiError::Parse(format!("lesson {} has no summary", i + 1)));
        }
        if l.minutes == 0 {
            return Err(AiError::Parse(format!(
                "lesson {} has a reading time of 0 minutes",
                i + 1
            )));
        }
    }
    Ok(outline)
}

/// Everything needed to write one lesson of an accepted outline.
#[derive(Debug, Clone)]
pub struct LessonContext {
    pub subject_title: String,
    pub topic_title: String,
    /// The learner's original request.
    pub prompt: String,
    pub level: Level,
    pub with_quiz: bool,
    /// The whole outline, so the lesson can reference neighbours.
    pub outline: Vec<OutlineLesson>,
    /// Index into `outline` of the lesson to write.
    pub index: usize,
    /// Excerpts of the learner's sources most relevant to this lesson (empty without
    /// sources).
    pub passages: Vec<Passage>,
}

/// Streams a lesson body as Markdown (see `Lesson::body_md` for conventions).
///
/// The returned body is the streamed text with two mistakes removed if the model made
/// them despite the instructions: a code fence around the whole answer and a leading
/// `# ` title. Callers should store the returned text (the `LessonChanged` that follows
/// the deltas is authoritative). An `index` outside `outline` is `AiError::Parse`.
pub async fn write_lesson(
    client: &ResponsesClient,
    ctx: &LessonContext,
    on_delta: impl FnMut(&str) + Send,
) -> Result<String, AiError> {
    if ctx.index >= ctx.outline.len() {
        return Err(AiError::Parse(format!(
            "lesson {} doesn't exist in an outline of {} lessons",
            ctx.index + 1,
            ctx.outline.len()
        )));
    }
    let text = client
        .stream_text(
            &prompts::lesson_instructions(ctx),
            &prompts::lesson_input(ctx),
            on_delta,
        )
        .await?;
    let body = clean::lesson_markdown(&text);
    if body.trim().is_empty() {
        return Err(AiError::Parse("the lesson came back empty".into()));
    }
    Ok(body)
}

#[derive(Debug, Clone)]
pub struct SceneRequest {
    pub lesson_title: String,
    pub lesson_body_md: String,
    /// A previous scene and the error ManimGX reported for it, to repair instead of
    /// starting over.
    pub previous_attempt: Option<FailedScene>,
}

#[derive(Debug, Clone)]
pub struct FailedScene {
    pub code: String,
    pub error: String,
}

/// Writes a ManimGX scene file (Python) explaining the lesson. Contract with
/// `margin-video`:
/// - `import manimgx as m` and nothing that needs the network or files outside the
///   working directory;
/// - exactly one scene class, named `LessonScene`, deriving `m.Scene`;
/// - narration only through `self.say("…[words]…", animations…)`;
/// - no `voice` attribute (the renderer installs the voice);
/// - returns only code, no Markdown fences.
///
/// The answer is checked for `import manimgx as m` and `class LessonScene(m.Scene)`;
/// without them the result is `AiError::Parse`. Whether the scene runs is for the
/// renderer to find out (and to send back as `FailedScene`).
pub async fn write_scene(client: &ResponsesClient, req: &SceneRequest) -> Result<String, AiError> {
    let text = client
        .stream_text(
            &prompts::scene_instructions(),
            &prompts::scene_input(req),
            |_| {},
        )
        .await?;
    let code = clean::scene_code(&text);
    if !code.lines().any(|l| l.trim() == "import manimgx as m") {
        return Err(AiError::Parse(
            "the scene doesn't import manimgx as m".into(),
        ));
    }
    if !code.contains("class LessonScene(m.Scene)") {
        return Err(AiError::Parse(
            "the scene doesn't define class LessonScene(m.Scene)".into(),
        ));
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outline(n: usize) -> Outline {
        Outline {
            topic_title: " Fourier series ".into(),
            subject_title: "Signals & Systems".into(),
            lessons: (0..n)
                .map(|i| OutlineLesson {
                    title: format!("Lesson {i}"),
                    summary: "One\nline".into(),
                    minutes: 10,
                })
                .collect(),
        }
    }

    #[test]
    fn outline_validation() {
        let ok = validate_outline(outline(3), 3).unwrap();
        assert_eq!(ok.topic_title, "Fourier series");
        assert_eq!(ok.lessons[0].summary, "One line");
        assert!(matches!(
            validate_outline(outline(2), 3),
            Err(AiError::Parse(_))
        ));
        let mut empty = outline(1);
        empty.lessons[0].summary = "  ".into();
        assert!(matches!(validate_outline(empty, 1), Err(AiError::Parse(_))));
        let mut zero = outline(1);
        zero.lessons[0].minutes = 0;
        assert!(matches!(validate_outline(zero, 1), Err(AiError::Parse(_))));
        let mut no_subject = outline(1);
        no_subject.subject_title = String::new();
        assert!(matches!(
            validate_outline(no_subject, 1),
            Err(AiError::Parse(_))
        ));
    }

    #[test]
    fn error_messages() {
        let v = json!({"error": {"message": "bad key", "type": "invalid_request_error", "code": "invalid_api_key"}});
        assert_eq!(error_message(&v).as_deref(), Some("bad key"));
        assert_eq!(
            error_message(&json!({"error": "plain"})).as_deref(),
            Some("plain")
        );
        assert_eq!(
            error_message(&json!({"type": "error", "code": "x", "message": "m"})).as_deref(),
            Some("m")
        );
    }

    #[test]
    fn response_text_variants() {
        let ok = json!({"status": "completed", "output": [
            {"type": "reasoning", "summary": []},
            {"type": "message", "content": [
                {"type": "output_text", "text": "a"},
                {"type": "output_text", "text": "b"}
            ]}
        ]});
        assert_eq!(response_text(&ok).unwrap(), "ab");
        let refusal = json!({"status": "completed", "output": [
            {"type": "message", "content": [{"type": "refusal", "refusal": "no"}]}
        ]});
        assert!(matches!(response_text(&refusal), Err(AiError::Incomplete(m)) if m.contains("no")));
        let incomplete = json!({"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}, "output": []});
        assert!(
            matches!(response_text(&incomplete), Err(AiError::Incomplete(m)) if m.contains("token limit"))
        );
    }

    #[test]
    fn base_url_is_normalised() {
        let c = ResponsesClient::new("https://example.test/v1/ ", "sk-secret", "gpt-test");
        assert_eq!(c.url, "https://example.test/v1/responses");
        assert!(!format!("{c:?}").contains("sk-secret"));
    }
}
