//! The general streaming Responses API call the agent loop uses: input items, function
//! tools and reasoning in, a typed stream of events out.
//!
//! Requests are always `stream: true` and `store: false`; the caller (the service) keeps
//! the transcript and replays it as `input`. Input items and output items are carried
//! as `serde_json::Value` exactly as the API sends them, so encrypted reasoning,
//! compaction items and function calls round-trip byte for byte.

use std::collections::VecDeque;
use std::pin::Pin;
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use margin_shared::{ReasoningEffort, Usage};
use serde_json::{Value, json};

use crate::sse::{SseEvent, SseParser};
use crate::{AiError, ResponsesClient, check_status, error_chain, error_message, network};

/// One `POST /responses` of the agent loop.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResponseRequest {
    /// Overrides the client's model (Settings' agent model); `None` uses it.
    pub model: Option<String>,
    /// `instructions`, if any. The agent sends its system prompt as the first input item
    /// (a `developer` message) instead, so this is usually `None`.
    pub instructions: Option<String>,
    /// `input`: items verbatim (messages, `function_call`, `function_call_output`,
    /// `reasoning` with `encrypted_content`, `compaction`, …).
    pub input: Vec<Value>,
    /// `tools`: function tool definitions verbatim, e.g.
    /// `{"type":"function","name":…,"description":…,"parameters":…,"strict":true}`.
    pub tools: Vec<Value>,
    pub tool_choice: Option<ToolChoice>,
    pub reasoning: Option<ReasoningConfig>,
    /// Adds `"include": ["reasoning.encrypted_content"]`, so reasoning items can be
    /// replayed with `store: false`.
    pub include_encrypted_reasoning: bool,
    pub max_output_tokens: Option<u32>,
    /// `prompt_cache_key`: the chat id, so a chat's calls share a cache.
    pub prompt_cache_key: Option<String>,
    pub context_management: Option<ContextManagement>,
    pub parallel_tool_calls: Option<bool>,
}

/// `tool_choice`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChoice {
    /// `"auto"`.
    Auto,
    /// `"none"`: answer with text only.
    None,
    /// `"required"`.
    Required,
    /// `{"type":"function","name":…}`.
    Function(String),
    /// `{"type":"allowed_tools","mode":…,"tools":[{"type":"function","name":…},…]}`:
    /// restricts this call to some tools while `tools` stays the same (cache-friendly).
    AllowedTools {
        /// Requires a call (`"required"`) instead of letting the model choose (`"auto"`).
        required: bool,
        names: Vec<String>,
    },
}

impl ToolChoice {
    /// The JSON value of `tool_choice`.
    pub fn to_json(&self) -> Value {
        match self {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::None => json!("none"),
            ToolChoice::Required => json!("required"),
            ToolChoice::Function(name) => json!({"type": "function", "name": name}),
            ToolChoice::AllowedTools { required, names } => json!({
                "type": "allowed_tools",
                "mode": if *required { "required" } else { "auto" },
                "tools": names
                    .iter()
                    .map(|n| json!({"type": "function", "name": n}))
                    .collect::<Vec<_>>(),
            }),
        }
    }
}

/// `reasoning`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReasoningConfig {
    pub effort: ReasoningEffort,
    /// `reasoning.summary`; `None` asks for no summary.
    pub summary: Option<ReasoningSummary>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningSummary {
    Auto,
    Concise,
    Detailed,
}

impl ReasoningSummary {
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningSummary::Auto => "auto",
            ReasoningSummary::Concise => "concise",
            ReasoningSummary::Detailed => "detailed",
        }
    }
}

/// `context_management`: server-side compaction of long conversations. The compacted
/// history arrives as an output item (stored and replayed like any other).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextManagement {
    /// Compact once the input passes `compact_threshold` tokens.
    Compaction { compact_threshold: u32 },
}

impl ContextManagement {
    /// The JSON value of `context_management`, per the Responses API compaction guide.
    pub fn to_json(&self) -> Value {
        match self {
            ContextManagement::Compaction { compact_threshold } => json!([{
                "type": "compaction",
                "compact_threshold": compact_threshold,
            }]),
        }
    }
}

/// One event of a streamed response, in the order the API sends them. Events the loop
/// doesn't need (`response.created`, `in_progress`, `content_part.*`, `*.done` text
/// events, keep-alives) are not yielded.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// `response.output_item.added`: an item started (`message`, `reasoning`,
    /// `function_call`, `compaction`, …). `item` is partial.
    OutputItemAdded { output_index: u32, item: Value },
    /// `response.output_item.done`: the item, complete and verbatim. This is what is
    /// stored and replayed.
    OutputItemDone { output_index: u32, item: Value },
    /// `response.function_call_arguments.delta`.
    FunctionCallArgumentsDelta {
        output_index: u32,
        item_id: String,
        delta: String,
    },
    /// `response.function_call_arguments.done`.
    FunctionCallArgumentsDone {
        output_index: u32,
        item_id: String,
        arguments: String,
    },
    /// `response.output_text.delta`, and `response.refusal.delta` (a refusal is shown as
    /// the message's text).
    OutputTextDelta {
        output_index: u32,
        item_id: String,
        content_index: u32,
        delta: String,
    },
    /// `response.reasoning_summary_text.delta`.
    ReasoningSummaryTextDelta {
        output_index: u32,
        item_id: String,
        summary_index: u32,
        delta: String,
    },
    /// `response.completed`. Terminal. `usage.model_calls` is 1.
    Completed { response_id: String, usage: Usage },
    /// `response.incomplete` (e.g. `max_output_tokens`). Terminal. Items done so far were
    /// yielded and are valid.
    Incomplete { reason: String, usage: Usage },
    /// `response.failed`. Terminal.
    Failed {
        code: Option<String>,
        message: String,
    },
    /// A top-level `error` event. Terminal.
    Error {
        code: Option<String>,
        message: String,
    },
}

impl StreamEvent {
    /// Whether nothing follows this event.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            StreamEvent::Completed { .. }
                | StreamEvent::Incomplete { .. }
                | StreamEvent::Failed { .. }
                | StreamEvent::Error { .. }
        )
    }
}

/// The events of one response. Ends after a terminal event; a transport failure or an
/// unparseable event is an `Err` item (and the end). A connection that closes before a
/// terminal event yields `Err(AiError::Network(..))`.
pub type ResponseStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, AiError>> + Send>>;

impl ResponsesClient {
    /// The JSON body sent for `req`: always `stream: true` and `store: false`; optional
    /// fields are left out when `None`/empty.
    pub fn response_body(&self, req: &ResponseRequest) -> Value {
        let mut body = json!({
            "model": req.model.as_deref().unwrap_or(&self.model),
            "input": req.input,
            "stream": true,
            "store": false,
        });
        if let Some(instructions) = &req.instructions {
            body["instructions"] = json!(instructions);
        }
        if !req.tools.is_empty() {
            body["tools"] = json!(req.tools);
        }
        if let Some(choice) = &req.tool_choice {
            body["tool_choice"] = choice.to_json();
        }
        if let Some(reasoning) = &req.reasoning {
            let mut r = json!({"effort": reasoning.effort.as_str()});
            if let Some(summary) = reasoning.summary {
                r["summary"] = json!(summary.as_str());
            }
            body["reasoning"] = r;
        }
        if req.include_encrypted_reasoning {
            body["include"] = json!(["reasoning.encrypted_content"]);
        }
        if let Some(max) = req.max_output_tokens {
            body["max_output_tokens"] = json!(max);
        }
        if let Some(key) = &req.prompt_cache_key {
            body["prompt_cache_key"] = json!(key);
        }
        if let Some(cm) = &req.context_management {
            body["context_management"] = cm.to_json();
        }
        if let Some(parallel) = req.parallel_tool_calls {
            body["parallel_tool_calls"] = json!(parallel);
        }
        body
    }

    /// Sends `req` and returns its events as they stream in. A non-2xx answer is
    /// `Err(AiError::Http { status, message, retry_after })` before any event; retrying
    /// is the caller's decision (`AiError::is_retryable`, [`Backoff`]).
    pub async fn stream_response(&self, req: &ResponseRequest) -> Result<ResponseStream, AiError> {
        let body = self.response_body(req);
        let resp = self.request(&body)?.send().await.map_err(network)?;
        let resp = check_status(resp).await?;
        let pump = Pump {
            bytes: Box::pin(resp.bytes_stream()),
            parser: SseParser::new(),
            queue: VecDeque::new(),
            eof: false,
            done: false,
        };
        Ok(Box::pin(futures_util::stream::unfold(pump, Pump::next)))
    }

    /// `stream_response`, retrying a call that fails before its stream starts (429, 5xx,
    /// network) as `backoff` says. Errors inside the stream are not retried here: by
    /// then the caller may have shown output, so starting over is its decision. The
    /// agent loop retries itself (it also retries a stream that fails before any output)
    /// and calls `stream_response`, so retries don't multiply.
    pub async fn stream_response_with_retries(
        &self,
        req: &ResponseRequest,
        backoff: &Backoff,
    ) -> Result<ResponseStream, AiError> {
        let mut attempt = 0;
        loop {
            match self.stream_response(req).await {
                Ok(stream) => return Ok(stream),
                Err(e) => match backoff.delay(attempt, &e) {
                    Some(pause) => {
                        tracing::debug!(attempt, ?pause, error = %e, "retrying the model call");
                        tokio::time::sleep(pause).await;
                        attempt += 1;
                    }
                    None => return Err(e),
                },
            }
        }
    }
}

/// How a failed model call is retried: `retries` more attempts after the first, each
/// after the pause the API asked for (`Retry-After`) or else `base_delay · 2^n`, capped
/// at `max_delay`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub retries: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for Backoff {
    /// 3 retries, 2 s doubling up to 60 s.
    fn default() -> Self {
        Self {
            retries: 3,
            base_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(60),
        }
    }
}

impl Backoff {
    /// The pause before the next attempt after `err`, where `attempt` counts the retries
    /// already made; `None` when `err` isn't worth retrying or the retries are used up.
    /// A `Retry-After` from the API is honoured up to `max_delay`. (The agent loop's
    /// `RetryPolicy` no longer caps it: a turn waits as asked and its chat says so.)
    pub fn delay(&self, attempt: u32, err: &AiError) -> Option<Duration> {
        if attempt >= self.retries || !err.is_retryable() {
            return None;
        }
        if let Some(after) = err.retry_after() {
            return Some(after.min(self.max_delay));
        }
        let factor = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
        Some(
            self.base_delay
                .checked_mul(factor)
                .unwrap_or(self.max_delay)
                .min(self.max_delay),
        )
    }
}

/// Reads the body of one streamed response and turns its SSE events into
/// `StreamEvent`s.
struct Pump<S> {
    bytes: S,
    parser: SseParser,
    /// Events parsed from the last chunk and not yet yielded.
    queue: VecDeque<Result<StreamEvent, AiError>>,
    /// The body ended.
    eof: bool,
    /// A terminal event or an error was yielded; nothing follows.
    done: bool,
}

impl<S, B> Pump<S>
where
    S: Stream<Item = reqwest::Result<B>> + Unpin,
    B: AsRef<[u8]>,
{
    async fn next(mut self) -> Option<(Result<StreamEvent, AiError>, Self)> {
        loop {
            if self.done {
                return None;
            }
            if let Some(item) = self.queue.pop_front() {
                if !matches!(&item, Ok(e) if !e.is_terminal()) {
                    self.done = true;
                    self.queue.clear();
                }
                return Some((item, self));
            }
            if self.eof {
                return None;
            }
            match self.bytes.next().await {
                Some(Ok(chunk)) => {
                    for event in self.parser.push(chunk.as_ref()) {
                        self.accept(&event);
                    }
                }
                // reqwest reports a body cut mid-chunk as a decode error; it is the
                // connection that failed, not the content.
                Some(Err(e)) if e.is_decode() || e.is_body() => self.queue.push_back(Err(
                    AiError::Network(format!("the connection broke: {}", error_chain(&e))),
                )),
                Some(Err(e)) => self.queue.push_back(Err(network(e))),
                None => {
                    self.eof = true;
                    if let Some(event) = self.parser.finish() {
                        self.accept(&event);
                    }
                    // Only reached when no terminal event came first.
                    self.queue.push_back(Err(AiError::Network(
                        "the connection closed before the response finished".into(),
                    )));
                }
            }
        }
    }

    fn accept(&mut self, event: &SseEvent) {
        match parse_event(event) {
            Ok(Some(e)) => self.queue.push_back(Ok(e)),
            Ok(None) => {}
            Err(e) => self.queue.push_back(Err(e)),
        }
    }
}

/// One SSE event of a Responses stream as a `StreamEvent`; `None` for the events the
/// loop doesn't need.
fn parse_event(event: &SseEvent) -> Result<Option<StreamEvent>, AiError> {
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
    let index = |key: &str| v.get(key).and_then(Value::as_u64).unwrap_or(0) as u32;
    let text = |key: &str| v.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let item = || {
        v.get("item")
            .filter(|i| i.is_object())
            .cloned()
            .ok_or_else(|| AiError::Parse(format!("a {kind} event has no item")))
    };
    let response = v.get("response").unwrap_or(&Value::Null);
    Ok(Some(match kind {
        "response.output_item.added" => StreamEvent::OutputItemAdded {
            output_index: index("output_index"),
            item: item()?,
        },
        "response.output_item.done" => StreamEvent::OutputItemDone {
            output_index: index("output_index"),
            item: item()?,
        },
        "response.function_call_arguments.delta" => StreamEvent::FunctionCallArgumentsDelta {
            output_index: index("output_index"),
            item_id: text("item_id"),
            delta: text("delta"),
        },
        "response.function_call_arguments.done" => StreamEvent::FunctionCallArgumentsDone {
            output_index: index("output_index"),
            item_id: text("item_id"),
            arguments: text("arguments"),
        },
        // A refusal streams like text: the message's refusal is what the chat shows.
        "response.output_text.delta" | "response.refusal.delta" => StreamEvent::OutputTextDelta {
            output_index: index("output_index"),
            item_id: text("item_id"),
            content_index: index("content_index"),
            delta: text("delta"),
        },
        "response.reasoning_summary_text.delta" => StreamEvent::ReasoningSummaryTextDelta {
            output_index: index("output_index"),
            item_id: text("item_id"),
            summary_index: index("summary_index"),
            delta: text("delta"),
        },
        "response.completed" => StreamEvent::Completed {
            response_id: response
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            usage: usage(response),
        },
        "response.incomplete" => StreamEvent::Incomplete {
            reason: response
                .pointer("/incomplete_details/reason")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            usage: usage(response),
        },
        "response.failed" => {
            let err = response.get("error").filter(|e| !e.is_null());
            StreamEvent::Failed {
                code: err
                    .and_then(|e| e.get("code"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                message: err
                    .and_then(error_message)
                    .unwrap_or_else(|| "the response failed".into()),
            }
        }
        "error" => {
            // `{type: "error", code, message}`, or the same nested under `error`.
            let err = v.get("error").filter(|e| e.is_object()).unwrap_or(&v);
            StreamEvent::Error {
                code: err.get("code").and_then(Value::as_str).map(str::to_string),
                message: err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
                    .to_string(),
            }
        }
        _ => return Ok(None), // created, in_progress, content_part.*, text .done, …
    }))
}

/// `response.usage` of a finished response, as one model call.
pub(crate) fn usage(response: &Value) -> Usage {
    let n = |path: &str| {
        response
            .pointer(&format!("/usage/{path}"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    Usage {
        input_tokens: n("input_tokens"),
        cached_input_tokens: n("input_tokens_details/cached_tokens"),
        output_tokens: n("output_tokens"),
        reasoning_tokens: n("output_tokens_details/reasoning_tokens"),
        model_calls: 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_has_only_what_was_asked() {
        let c = ResponsesClient::new("https://example.test/v1", "k", "base-model");
        let body = c.response_body(&ResponseRequest {
            input: vec![json!({"role": "user", "content": "hi"})],
            ..Default::default()
        });
        assert_eq!(
            body,
            json!({"model": "base-model", "input": [{"role": "user", "content": "hi"}], "stream": true, "store": false})
        );

        let body = c.response_body(&ResponseRequest {
            model: Some("agent-model".into()),
            tools: vec![json!({"type": "function", "name": "a"})],
            tool_choice: Some(ToolChoice::AllowedTools {
                required: false,
                names: vec!["a".into()],
            }),
            reasoning: Some(ReasoningConfig {
                effort: ReasoningEffort::Low,
                summary: Some(ReasoningSummary::Auto),
            }),
            include_encrypted_reasoning: true,
            max_output_tokens: Some(1000),
            prompt_cache_key: Some("chat-1".into()),
            context_management: Some(ContextManagement::Compaction {
                compact_threshold: 200_000,
            }),
            parallel_tool_calls: Some(true),
            ..Default::default()
        });
        assert_eq!(body["model"], "agent-model");
        assert_eq!(
            body["tool_choice"],
            json!({"type": "allowed_tools", "mode": "auto", "tools": [{"type": "function", "name": "a"}]})
        );
        assert_eq!(
            body["reasoning"],
            json!({"effort": "low", "summary": "auto"})
        );
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["max_output_tokens"], 1000);
        assert_eq!(body["prompt_cache_key"], "chat-1");
        assert_eq!(body["parallel_tool_calls"], true);
        assert_eq!(body["context_management"][0]["type"], "compaction");
        assert_eq!(body["store"], false);
    }

    #[test]
    fn tool_choices() {
        assert_eq!(ToolChoice::None.to_json(), json!("none"));
        assert_eq!(
            ToolChoice::Function("x".into()).to_json(),
            json!({"type": "function", "name": "x"})
        );
        assert_eq!(
            ToolChoice::AllowedTools {
                required: true,
                names: vec![]
            }
            .to_json()["mode"],
            "required"
        );
    }
}
