//! Text generation and agent steps for Margin, on two API shapes behind one seam
//! ([`model`]: [`TextModel`], [`Provider`], [`CallOptions`]):
//! - an OpenAI-compatible **Responses API** (`POST {base_url}/responses`,
//!   [`ResponsesClient`]): OpenAI with an API key, ChatGPT with a plan token;
//! - Anthropic's **Messages API** (`POST {base_url}/v1/messages`, [`ClaudeClient`]).
//!
//! The service owns configuration and the credentials; this crate is stateless and
//! knows nothing about storage.
//!
//! Every request is streamed (Server-Sent Events); Responses calls are sent with
//! `store: false`.
//!
//! Generation calls take [`CallOptions`] (the model route, the cache key, an output cap,
//! the constant prefix's length) and their instructions are laid out for prompt caching:
//! a versioned constant block first, byte-identical across calls, then the variable
//! part, then the input (`docs/agent-rag-architecture.md` §2.6). The prompts live in
//! [`prompts`].

use std::time::Duration;

use futures_util::StreamExt;
use margin_shared::pipeline::Route;
use margin_shared::{Level, OutlineLesson, Usage};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

mod citations;
mod clean;
mod messages;
pub mod model;
pub mod prompts;
mod responses;
mod sse;
pub mod windows;

pub use prompts::evals;
pub use prompts::{
    CoursePlan, DocumentContents, JsonFormat, PlanRequest, PlannedLesson, Prompt, Ranked,
    RerankCandidate, SectionRef,
};

pub use messages::{
    ANTHROPIC_BASE_URL, ANTHROPIC_VERSION, CLAUDE_BETAS, CLAUDE_MAX_OUTPUT_TOKENS,
    CLAUDE_TEXT_MAX_OUTPUT_TOKENS, ClaudeClient, WEB_FETCH_MAX_CONTENT_TOKENS, WEB_FETCH_MAX_USES,
    WEB_FETCH_TOOL, WEB_SEARCH_MAX_USES, WEB_SEARCH_TOOL,
};
pub use model::{
    CallOptions, Credential, Delta, ItemView, MODEL_WINDOW_FALLBACK, ModelEvent, ModelInfo,
    ModelItem, ModelRequest, ModelStream, ModelWindow, Next, Provider, ProviderConfig,
    ProviderKind, RouteModels, StartKind, TextModel, TextOutput, ToolChoice, ToolDef, WebTools,
};
pub use responses::{
    Backoff, ContextManagement, ReasoningConfig, ReasoningSummary, ResponseRequest, ResponseStream,
    ResponsesToolChoice, StreamEvent, function_tool, model_events, responses_request,
    to_model_events,
};

/// 429 codes that waiting doesn't cure: Anthropic's spend limit and Margin's share of a
/// ChatGPT plan used up (the docs say not to infer a reset time).
pub const NOT_RETRYABLE_429: [&str; 2] = [
    "enforced_spend_limit_reached",
    "subscription_sharing_usage_limit_exceeded",
];

#[derive(Debug, thiserror::Error)]
pub enum AiError {
    /// The API answered with a non-2xx status. `message` is the API's error message
    /// when it sent one; `retry_after` is the `Retry-After` header (seconds) when sent;
    /// `code` is the API's machine-readable code: `error.details.error_code` when
    /// present (`enforced_spend_limit_reached`), else OpenAI's `error.code`, else
    /// Anthropic's `error.type`.
    #[error("the model API returned {status}: {message}")]
    Http {
        status: u16,
        message: String,
        retry_after: Option<Duration>,
        code: Option<String>,
    },
    #[error("couldn't reach the model API: {0}")]
    Network(String),
    /// The response could not be parsed.
    #[error("the model's answer couldn't be read: {0}")]
    Parse(String),
    /// The model refused or the response ended as `incomplete`/`failed`.
    #[error("the model didn't finish: {0}")]
    Incomplete(String),
}

impl AiError {
    /// Worth retrying after a pause: rate limits (429, except the codes waiting doesn't
    /// cure: `NOT_RETRYABLE_429`), server errors (5xx, Anthropic's 529 overloaded among
    /// them) and network failures. Everything else (bad key, bad request, refusals)
    /// fails the same way again.
    pub fn is_retryable(&self) -> bool {
        match self {
            AiError::Http {
                status: 429, code, ..
            } => !code
                .as_deref()
                .is_some_and(|c| NOT_RETRYABLE_429.contains(&c)),
            AiError::Http { status, .. } => *status >= 500,
            AiError::Network(_) => true,
            AiError::Parse(_) | AiError::Incomplete(_) => false,
        }
    }

    /// The API's error code, when it sent one.
    pub fn code(&self) -> Option<&str> {
        match self {
            AiError::Http { code, .. } => code.as_deref(),
            _ => None,
        }
    }

    /// The pause the API asked for, if it did.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            AiError::Http { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

/// How long to wait for a TCP + TLS connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// A stalled connection: no bytes at all for this long. Streams have no overall limit;
/// the Responses API sends events (and keep-alives) while the model works.
const READ_TIMEOUT: Duration = Duration::from_secs(300);

/// A configured Responses API endpoint. Cheap to clone.
#[derive(Clone)]
pub struct ResponsesClient {
    http: Result<reqwest::Client, String>,
    url: String,
    api_key: String,
    model: String,
    /// The model of each route, when built from a `ProviderConfig`; `None` calls `model`
    /// on every route.
    models: Option<RouteModels>,
    /// OpenAI, or ChatGPT under a plan token.
    kind: ProviderKind,
}

impl std::fmt::Debug for ResponsesClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponsesClient")
            .field("url", &self.url)
            .field("model", &self.model)
            .field("kind", &self.kind)
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
            models: None,
            kind: ProviderKind::OpenAi,
        }
    }

    /// The client of an OpenAI or ChatGPT `ProviderConfig`: each call's model is
    /// `config.models.get(opts.route)`.
    pub fn from_config(config: &ProviderConfig) -> Self {
        let mut client = Self::new(
            config.base_url.clone(),
            config.credential.secret(),
            config.models.chat.clone(),
        );
        client.models = Some(config.models.clone());
        client.kind = config.provider;
        client
    }

    /// The model `route` calls.
    pub fn model_for(&self, route: Route) -> &str {
        match &self.models {
            Some(models) => models.get(route),
            None => &self.model,
        }
    }

    /// OpenAI, or ChatGPT.
    pub fn kind(&self) -> ProviderKind {
        self.kind
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

    /// The `POST /responses` body of a generation call: always streamed, never stored;
    /// `prompt_cache_key`, `max_output_tokens` and a structured-output `text.format`
    /// only when given.
    fn body(
        &self,
        instructions: &str,
        input: &[InputPart],
        opts: &CallOptions,
        format: Option<&JsonFormat>,
    ) -> Value {
        let mut body = json!({
            "model": self.model_for(opts.route),
            "instructions": instructions,
            "input": input_json(input),
            "stream": true,
            "store": false,
        });
        if let Some(key) = &opts.cache_key {
            body["prompt_cache_key"] = json!(key);
        }
        if let Some(max) = opts.max_output_tokens {
            body["max_output_tokens"] = json!(max);
        }
        if let Some(format) = format {
            body["text"] = json!({"format": {
                "type": "json_schema",
                "name": format.name,
                "strict": true,
                "schema": format.schema,
            }});
        }
        body
    }

    /// The model this client calls.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Streams a plain-text answer (`stream: true`), calling `on_delta` with each
    /// `response.output_text.delta`. Returns the full text.
    pub async fn stream_text(
        &self,
        instructions: &str,
        input: &str,
        opts: &CallOptions,
        on_delta: impl FnMut(&str) + Send,
    ) -> Result<String, AiError> {
        let input = [InputPart::Text(input.to_string())];
        let body = self.body(instructions, &input, opts, None);
        Ok(self.stream_body(&body, on_delta).await?.text)
    }

    /// Streams the answer to a [`Prompt`]: its instructions (constant block, then the
    /// variable part) and its input. The cache key is `opts.cache_key`, or the
    /// prompt's own (`"{job}:{version}"`) when the caller gave none.
    pub async fn generate(
        &self,
        prompt: &Prompt,
        opts: &CallOptions,
        on_delta: impl FnMut(&str) + Send,
    ) -> Result<Generated<String>, AiError> {
        let opts = opts.keyed(prompt);
        let body = self.body(&prompt.instructions(), &prompt.input, &opts, None);
        Ok(self.stream_body(&body, on_delta).await?.generated())
    }

    /// Asks for a structured answer (`text.format`: the prompt's strict JSON schema)
    /// and parses it as `T`. An answer that isn't JSON of that shape is
    /// `AiError::Parse`.
    pub async fn generate_json<T: DeserializeOwned>(
        &self,
        prompt: &Prompt,
        opts: &CallOptions,
    ) -> Result<Generated<T>, AiError> {
        let format = prompt.format.as_ref().ok_or_else(|| {
            AiError::Parse(format!("the {} prompt has no JSON schema", prompt.job))
        })?;
        let opts = opts.keyed(prompt);
        let body = self.body(&prompt.instructions(), &prompt.input, &opts, Some(format));
        let Generated { value: text, usage } = self.stream_body(&body, |_| {}).await?.generated();
        let value = serde_json::from_str(json_answer(&text)).map_err(|e| {
            AiError::Parse(format!(
                "the {} answer isn't the JSON asked for: {e}",
                prompt.job
            ))
        })?;
        Ok(Generated { value, usage })
    }

    async fn stream_body(
        &self,
        body: &Value,
        mut on_delta: impl FnMut(&str) + Send,
    ) -> Result<TextOutput, AiError> {
        let resp = self.request(body)?.send().await.map_err(network)?;
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
}

/// A call's answer and what it cost.
#[derive(Debug, Clone, PartialEq)]
pub struct Generated<T> {
    pub value: T,
    /// `response.usage` (zeros when the server sent none); `model_calls` is 1.
    pub usage: Usage,
}

impl TextOutput {
    fn generated(self) -> Generated<String> {
        Generated {
            value: self.text,
            usage: self.usage,
        }
    }
}

impl TextModel for ResponsesClient {
    fn stream_parts<'a>(
        &'a self,
        instructions: &'a str,
        input: &'a [InputPart],
        opts: &'a CallOptions,
        on_delta: Box<dyn FnMut(&str) + Send + 'a>,
    ) -> model::BoxFuture<'a, Result<TextOutput, AiError>> {
        Box::pin(async move {
            let body = self.body(instructions, input, opts, None);
            self.stream_body(&body, on_delta).await
        })
    }
}

/// The JSON of a structured answer: the text itself, or the inside of a code fence a
/// compatible server wrapped it in.
fn json_answer(text: &str) -> &str {
    let t = text.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    let Some((lang, body)) = rest.split_once('\n') else {
        return t;
    };
    if !matches!(lang.trim(), "" | "json") {
        return t;
    }
    body.trim_end().strip_suffix("```").map_or(t, str::trim)
}

/// A piece of a request's input.
#[derive(Clone, PartialEq)]
pub enum InputPart {
    Text(String),
    Image(Image),
}

/// A picture sent to the model as an `input_image` part (a base64 data URL).
#[derive(Clone, PartialEq)]
pub struct Image {
    /// "image/jpeg", "image/png", "image/webp" or "image/gif".
    pub media_type: String,
    pub data: Vec<u8>,
}

impl Image {
    /// `data:{media_type};base64,{data}`.
    pub fn data_url(&self) -> String {
        use base64::Engine;
        format!(
            "data:{};base64,{}",
            self.media_type,
            base64::engine::general_purpose::STANDARD.encode(&self.data)
        )
    }
}

impl std::fmt::Debug for Image {
    // Not the bytes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Image")
            .field("media_type", &self.media_type)
            .field("bytes", &self.data.len())
            .finish()
    }
}

impl std::fmt::Debug for InputPart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InputPart::Text(t) => f.debug_tuple("Text").field(t).finish(),
            InputPart::Image(i) => i.fmt(f),
        }
    }
}

/// The Responses API `input`: a plain string when it's all text, otherwise one user
/// message whose content is `input_text` and `input_image` parts in order (adjacent
/// texts joined).
fn input_json(parts: &[InputPart]) -> Value {
    let mut content: Vec<Value> = Vec::new();
    let mut text = String::new();
    let mut has_image = false;
    for part in parts {
        match part {
            InputPart::Text(t) => text.push_str(t),
            InputPart::Image(image) => {
                has_image = true;
                if !text.is_empty() {
                    content.push(json!({"type": "input_text", "text": std::mem::take(&mut text)}));
                }
                content.push(json!({
                    "type": "input_image",
                    "image_url": image.data_url(),
                    "detail": "auto",
                }));
            }
        }
    }
    if !has_image {
        return Value::String(text);
    }
    if !text.is_empty() {
        content.push(json!({"type": "input_text", "text": text}));
    }
    json!([{"role": "user", "content": content}])
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
    let retry_after = retry_after(resp.headers(), std::time::SystemTime::now());
    let body = resp.text().await.unwrap_or_default();
    let parsed = serde_json::from_str::<Value>(&body).ok();
    let code = parsed.as_ref().and_then(error_code);
    let message = parsed.as_ref().and_then(error_message).unwrap_or_else(|| {
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
        retry_after,
        code,
    })
}

/// The machine-readable code of an error body: `error.details.error_code` (Anthropic's
/// spend limit), else `error.code` (OpenAI), else `error.type` (Anthropic:
/// `rate_limit_error`, `overloaded_error`, `authentication_error`, …).
fn error_code(v: &Value) -> Option<String> {
    let e = v.get("error").filter(|e| e.is_object())?;
    e.pointer("/details/error_code")
        .and_then(Value::as_str)
        .or_else(|| e.get("code").and_then(Value::as_str))
        .or_else(|| e.get("type").and_then(Value::as_str))
        .map(str::to_string)
}

/// The pause a 429/503 asks for: OpenAI's `retry-after-ms` when sent (it is more
/// precise), else `Retry-After` in seconds (whole or fractional) or as an HTTP date
/// (a date in the past is no pause).
fn retry_after(
    headers: &reqwest::header::HeaderMap,
    now: std::time::SystemTime,
) -> Option<Duration> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
    };
    let seconds = |v: &str| {
        v.parse::<f64>()
            .ok()
            .filter(|s| s.is_finite() && *s >= 0.0)
            .and_then(|s| Duration::try_from_secs_f64(s).ok())
    };
    if let Some(ms) = header("retry-after-ms").and_then(seconds) {
        return Some(ms / 1000);
    }
    let v = header("retry-after")?;
    seconds(v).or_else(|| {
        let at = httpdate::parse_http_date(v).ok()?;
        Some(at.duration_since(now).unwrap_or(Duration::ZERO))
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
    /// Handles one SSE event. `Ok(Some(text and usage))` when the response completed.
    fn handle(
        &mut self,
        event: &sse::SseEvent,
        on_delta: &mut (impl FnMut(&str) + Send),
    ) -> Result<Option<TextOutput>, AiError> {
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
                let response = v.get("response").unwrap_or(&Value::Null);
                let usage = responses::usage(response);
                let field = |key: &str| {
                    response
                        .get(key)
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                };
                let (model, stop_reason) = (field("model"), field("status"));
                if self.text.is_empty() && !response.is_null() {
                    // Some compatible servers send the text only in the final object.
                    let text = response_text(response)?;
                    if !text.is_empty() {
                        on_delta(&text);
                    }
                    return Ok(Some(TextOutput {
                        text,
                        usage,
                        model,
                        stop_reason,
                    }));
                }
                return Ok(Some(TextOutput {
                    text: std::mem::take(&mut self.text),
                    usage,
                    model,
                    stop_reason,
                }));
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
/// is being written. Lessons are based on these where they cover the topic.
/// Passages are numbered from 1 in the order given, and lessons cite them as `[^n]`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Passage {
    /// The source's name, e.g. "notes/ch1.md".
    pub source: String,
    /// PDF page the passage is on.
    pub page: Option<u32>,
    /// The passage's text: a chunk, or the transcript of a stretch of a recording (with
    /// `start_ms`/`end_ms`), or what was said around a video frame. May be empty for a
    /// picture.
    pub text: String,
    /// Where in an audio or video source it is.
    pub start_ms: Option<u64>,
    pub end_ms: Option<u64>,
    /// A picture the model is shown with it: an image file, a PDF page, a video frame.
    pub image: Option<Image>,
}

pub use citations::{Renumbered, renumber_citations};

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
///
/// Runs on the `Write` route with the prompt's cache key (`lesson:lesson-v2`); use
/// [`write_lesson_with`] to choose the options and get the usage.
pub async fn write_lesson(
    client: &ResponsesClient,
    ctx: &LessonContext,
    on_delta: impl FnMut(&str) + Send,
) -> Result<String, AiError> {
    Ok(
        write_lesson_with(client, ctx, &CallOptions::new(Route::Write), on_delta)
            .await?
            .value,
    )
}

/// [`write_lesson`] with explicit call options; returns the body and the usage.
pub async fn write_lesson_with(
    client: &ResponsesClient,
    ctx: &LessonContext,
    opts: &CallOptions,
    on_delta: impl FnMut(&str) + Send,
) -> Result<Generated<String>, AiError> {
    if ctx.index >= ctx.outline.len() {
        return Err(AiError::Parse(format!(
            "lesson {} doesn't exist in an outline of {} lessons",
            ctx.index + 1,
            ctx.outline.len()
        )));
    }
    let Generated { value: text, usage } = client
        .generate(&prompts::lesson(ctx), opts, on_delta)
        .await?;
    let body = clean::lesson_markdown(&text);
    if body.trim().is_empty() {
        return Err(AiError::Parse("the lesson came back empty".into()));
    }
    Ok(Generated { value: body, usage })
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
///
/// Runs on the `Write` route with the prompt's cache key (`scene:scene-v2`); the
/// repair call shares the first call's cached prefix. [`write_scene_with`] chooses the
/// options and returns the usage.
pub async fn write_scene(client: &ResponsesClient, req: &SceneRequest) -> Result<String, AiError> {
    Ok(
        write_scene_with(client, req, &CallOptions::new(Route::Write))
            .await?
            .value,
    )
}

/// [`write_scene`] with explicit call options; returns the code and the usage.
pub async fn write_scene_with(
    client: &ResponsesClient,
    req: &SceneRequest,
    opts: &CallOptions,
) -> Result<Generated<String>, AiError> {
    let Generated { value: text, usage } =
        client.generate(&prompts::scene(req), opts, |_| {}).await?;
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
    Ok(Generated { value: code, usage })
}

/// Plans a course from the learner's request and a survey of their sources (§2.5):
/// the outline and, per lesson, the retrieval questions its passages are fetched with
/// and the source sections it must cite. Runs on the route the caller gives (`Write`
/// in the product), as one strict-JSON call with the key `plan:plan-v1` unless the
/// options name another.
///
/// The answer is checked against the request ([`CoursePlan::checked`]): the lesson
/// count, non-empty titles, 2–3 queries per lesson, `must_cite` limited to sections
/// listed in `req.contents`.
pub async fn plan_course(
    client: &ResponsesClient,
    req: &PlanRequest,
    opts: &CallOptions,
) -> Result<Generated<CoursePlan>, AiError> {
    let Generated { value, usage } = client
        .generate_json::<CoursePlan>(&prompts::plan(req), opts)
        .await?;
    Ok(Generated {
        value: value.checked(req)?,
        usage,
    })
}

/// Listwise rerank (§2.4, option 2): the candidates graded 0–3 for `query`, best
/// first. Every candidate comes back exactly once: ids the model made up are dropped
/// and candidates it left out follow the ranked ones in their original order with
/// grade 0. Unused by the product until R2; versioned (`rerank-v1`) so the eval can run
/// it now.
pub async fn rerank(
    client: &ResponsesClient,
    query: &str,
    candidates: &[RerankCandidate],
    opts: &CallOptions,
) -> Result<Generated<Vec<Ranked>>, AiError> {
    if candidates.is_empty() {
        return Ok(Generated {
            value: Vec::new(),
            usage: Usage::default(),
        });
    }
    let Generated { value, usage } = client
        .generate_json::<prompts::RerankAnswer>(&prompts::rerank(query, candidates), opts)
        .await?;
    Ok(Generated {
        value: prompts::complete_ranking(value, candidates),
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_and_retries() {
        let anthropic = json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}});
        assert_eq!(error_code(&anthropic).as_deref(), Some("overloaded_error"));
        let spend = json!({"type": "error", "error": {"type": "rate_limit_error", "message": "m",
            "details": {"error_code": "enforced_spend_limit_reached"}}});
        assert_eq!(
            error_code(&spend).as_deref(),
            Some("enforced_spend_limit_reached")
        );
        let openai = json!({"error": {"message": "bad key", "type": "invalid_request_error", "code": "invalid_api_key"}});
        assert_eq!(error_code(&openai).as_deref(), Some("invalid_api_key"));
        assert_eq!(error_code(&json!({"error": "plain"})), None);
        let http = |status, code: Option<&str>| AiError::Http {
            status,
            message: "m".into(),
            retry_after: None,
            code: code.map(str::to_string),
        };
        assert!(http(429, Some("rate_limit_error")).is_retryable());
        assert!(http(429, None).is_retryable());
        assert!(!http(429, Some("enforced_spend_limit_reached")).is_retryable());
        assert!(!http(429, Some("subscription_sharing_usage_limit_exceeded")).is_retryable());
        assert!(http(529, Some("overloaded_error")).is_retryable());
        assert!(http(504, None).is_retryable());
        assert!(!http(401, Some("authentication_error")).is_retryable());
        assert_eq!(http(401, Some("x")).code(), Some("x"));
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
    fn retry_after_forms() {
        use reqwest::header::{HeaderMap, HeaderValue};
        let now = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let headers = |pairs: &[(&'static str, &'static str)]| {
            let mut h = HeaderMap::new();
            for (k, v) in pairs {
                h.insert(*k, HeaderValue::from_static(v));
            }
            h
        };
        assert_eq!(retry_after(&headers(&[]), now), None);
        assert_eq!(
            retry_after(&headers(&[("retry-after", "7")]), now),
            Some(Duration::from_secs(7))
        );
        assert_eq!(
            retry_after(&headers(&[("retry-after", "1.5")]), now),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(
            retry_after(
                &headers(&[("retry-after", "7"), ("retry-after-ms", "250")]),
                now
            ),
            Some(Duration::from_millis(250))
        );
        let date = httpdate::fmt_http_date(now + Duration::from_secs(30));
        let mut h = HeaderMap::new();
        h.insert("retry-after", HeaderValue::from_str(&date).unwrap());
        assert_eq!(retry_after(&h, now), Some(Duration::from_secs(30)));
        assert_eq!(
            retry_after(&h, now + Duration::from_secs(60)),
            Some(Duration::ZERO)
        );
        assert_eq!(retry_after(&headers(&[("retry-after", "soon")]), now), None);
        assert_eq!(retry_after(&headers(&[("retry-after", "-3")]), now), None);
    }

    #[test]
    fn backoff_delays() {
        let b = Backoff {
            retries: 3,
            base_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(5),
        };
        let busy = AiError::Http {
            status: 503,
            message: "busy".into(),
            retry_after: None,
            code: None,
        };
        assert_eq!(b.delay(0, &busy), Some(Duration::from_secs(2)));
        assert_eq!(b.delay(1, &busy), Some(Duration::from_secs(4)));
        assert_eq!(b.delay(2, &busy), Some(Duration::from_secs(5)));
        assert_eq!(b.delay(3, &busy), None);
        let limited = AiError::Http {
            status: 429,
            message: "slow".into(),
            retry_after: Some(Duration::from_secs(9)),
            code: None,
        };
        // Retry-After is honoured up to the cap.
        assert_eq!(b.delay(0, &limited), Some(Duration::from_secs(5)));
        let soon = AiError::Http {
            status: 429,
            message: "slow".into(),
            retry_after: Some(Duration::from_secs(3)),
            code: None,
        };
        assert_eq!(b.delay(0, &soon), Some(Duration::from_secs(3)));
        assert_eq!(
            b.delay(0, &AiError::Network("x".into())),
            Some(Duration::from_secs(2))
        );
        let bad = AiError::Http {
            status: 400,
            message: "bad".into(),
            retry_after: None,
            code: None,
        };
        assert_eq!(b.delay(0, &bad), None);
        assert_eq!(b.delay(0, &AiError::Parse("x".into())), None);
        let huge = Backoff { retries: 100, ..b };
        assert_eq!(huge.delay(60, &busy), Some(Duration::from_secs(5)));
        assert_eq!(Backoff::default().retries, 3);
    }

    #[test]
    fn base_url_is_normalised() {
        let c = ResponsesClient::new("https://example.test/v1/ ", "sk-secret", "gpt-test");
        assert_eq!(c.url, "https://example.test/v1/responses");
        assert!(!format!("{c:?}").contains("sk-secret"));
    }
}
