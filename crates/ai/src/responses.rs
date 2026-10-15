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

use crate::model::{
    BoxFuture, Delta, ItemView, ModelEvent, ModelInfo, ModelItem, ModelRequest, ModelStream, Next,
    Provider, ProviderKind, StartKind, ToolChoice, ToolDef,
};
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
    pub tool_choice: Option<ResponsesToolChoice>,
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
pub enum ResponsesToolChoice {
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
    /// A name that is a hosted tool's type (`"web_search"`, [`HOSTED_TOOLS`]) is
    /// rendered as that tool (`{"type":"web_search"}`), not as a function.
    AllowedTools {
        /// Requires a call (`"required"`) instead of letting the model choose (`"auto"`).
        required: bool,
        names: Vec<String>,
    },
}

impl ResponsesToolChoice {
    /// The JSON value of `tool_choice`.
    pub fn to_json(&self) -> Value {
        match self {
            ResponsesToolChoice::Auto => json!("auto"),
            ResponsesToolChoice::None => json!("none"),
            ResponsesToolChoice::Required => json!("required"),
            ResponsesToolChoice::Function(name) => json!({"type": "function", "name": name}),
            ResponsesToolChoice::AllowedTools { required, names } => json!({
                "type": "allowed_tools",
                "mode": if *required { "required" } else { "auto" },
                "tools": names
                    .iter()
                    .map(|n| if HOSTED_TOOLS.contains(&n.as_str()) {
                        json!({"type": n})
                    } else {
                        json!({"type": "function", "name": n})
                    })
                    .collect::<Vec<_>>(),
            }),
        }
    }
}

/// The hosted tools a Responses request may offer, by their `type`. No Margin function
/// tool has one of these names.
pub const HOSTED_TOOLS: [&str; 1] = [WEB_SEARCH];

/// The hosted web search tool's `type` (and its name in `allowed_tools`).
pub const WEB_SEARCH: &str = "web_search";

/// `search_context_size` of the hosted web search (D1).
pub const WEB_SEARCH_CONTEXT_SIZE: &str = "medium";

/// `include` entry that returns the sources a `web_search_call` read (its results).
pub const INCLUDE_WEB_SOURCES: &str = "web_search_call.action.sources";

/// `include` entry that returns reasoning's encrypted content, so reasoning items can
/// be replayed under `store: false`.
pub const INCLUDE_ENCRYPTED_REASONING: &str = "reasoning.encrypted_content";

/// The hosted web search tool definition.
pub fn web_search_tool() -> Value {
    json!({"type": WEB_SEARCH, "search_context_size": WEB_SEARCH_CONTEXT_SIZE})
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
    /// `response.completed`. Terminal. `usage.model_calls` is 1; `model` and `status`
    /// are the response's (empty when the server sent none).
    Completed {
        response_id: String,
        usage: Usage,
        model: String,
        status: String,
    },
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
    /// fields are left out when `None`/empty. `include` asks for the encrypted
    /// reasoning when `include_encrypted_reasoning`, and for the web search sources
    /// whenever `tools` holds the hosted `web_search`.
    ///
    /// Under a ChatGPT bearer token (`is_bearer`): `max_output_tokens` is never sent,
    /// and a request with tools always sends `parallel_tool_calls: false`.
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
        let mut include = Vec::new();
        if req.include_encrypted_reasoning {
            include.push(INCLUDE_ENCRYPTED_REASONING);
        }
        if req.tools.iter().any(|t| t["type"] == WEB_SEARCH) {
            include.push(INCLUDE_WEB_SOURCES);
        }
        if !include.is_empty() {
            body["include"] = json!(include);
        }
        if let Some(max) = req.max_output_tokens
            && !self.bearer
        {
            body["max_output_tokens"] = json!(max);
        }
        if let Some(key) = &req.prompt_cache_key {
            body["prompt_cache_key"] = json!(key);
        }
        if let Some(cm) = &req.context_management {
            body["context_management"] = cm.to_json();
        }
        let parallel = if self.bearer && !req.tools.is_empty() {
            Some(false)
        } else {
            req.parallel_tool_calls
        };
        if let Some(parallel) = parallel {
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
        "response.completed" => {
            let field = |key: &str| {
                response
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            StreamEvent::Completed {
                response_id: field("id"),
                usage: usage(response),
                model: field("model"),
                status: field("status"),
            }
        }
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

// ---------------------------------------------------------------------------
// The provider seam (`model::Provider`) on the Responses API
// ---------------------------------------------------------------------------

impl ResponsesClient {
    /// The Responses request of an agent step on this client: `responses_request` on
    /// the route's model.
    pub fn agent_request(&self, req: &ModelRequest) -> ResponseRequest {
        responses_request(req, self.model_for(req.opts.route))
    }
}

/// The Responses request of an agent step on `model`: the system prompt as a leading
/// developer message, then the history (stored items verbatim: `web_search_call`s
/// replay like any output item); the function tools (strict), then the hosted
/// `web_search` (`search_context_size: medium`) when `req.web.search`; reasoning with
/// summaries and its encrypted content (and the search sources, `response_body`); the
/// chat's cache key; server compaction when asked. `req.web.fetch` is Claude's: the
/// Responses search opens pages itself.
///
/// `ToolChoice::Allowed` keeps web search allowed when it is offered (the gating is for
/// Margin's own tools). The bearer rules are the client's (`response_body`), so the
/// same request renders for OpenAI and ChatGPT.
pub fn responses_request(req: &ModelRequest, model: &str) -> ResponseRequest {
    let mut input = Vec::with_capacity(req.history.len() + 1);
    input.push(
        json!({"role": "developer", "content": [{"type": "input_text", "text": req.system}]}),
    );
    input.extend(req.history.iter().cloned());
    let mut tools: Vec<Value> = req.tools.iter().map(function_tool).collect();
    if req.web.search {
        tools.push(web_search_tool());
    }
    let tool_choice = (!tools.is_empty()).then(|| match &req.tool_choice {
        ToolChoice::Auto => ResponsesToolChoice::Auto,
        ToolChoice::None => ResponsesToolChoice::None,
        ToolChoice::Allowed(names) => {
            let mut names = names.clone();
            if req.web.search && !names.iter().any(|n| n == WEB_SEARCH) {
                names.push(WEB_SEARCH.to_string());
            }
            ResponsesToolChoice::AllowedTools {
                required: false,
                names,
            }
        }
    });
    ResponseRequest {
        model: Some(model.to_string()),
        instructions: None,
        input,
        parallel_tool_calls: (!tools.is_empty()).then_some(req.parallel_tool_calls),
        tools,
        tool_choice,
        reasoning: Some(ReasoningConfig {
            effort: req.effort,
            summary: Some(ReasoningSummary::Auto),
        }),
        include_encrypted_reasoning: true,
        max_output_tokens: req.opts.max_output_tokens,
        prompt_cache_key: req.opts.cache_key.clone(),
        context_management: req
            .compaction
            .map(|compact_threshold| ContextManagement::Compaction { compact_threshold }),
    }
}

/// A `ToolDef` as a Responses function tool.
pub fn function_tool(tool: &ToolDef) -> Value {
    json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.parameters,
        "strict": true,
    })
}

impl Provider for ResponsesClient {
    fn kind(&self) -> ProviderKind {
        ResponsesClient::kind(self)
    }

    fn stream(&self, req: ModelRequest) -> BoxFuture<'_, Result<ModelStream, AiError>> {
        Box::pin(async move {
            let events = self.stream_response(&self.agent_request(&req)).await?;
            Ok(model_events(events))
        })
    }

    /// `GET {base_url}/models`. An API key gets OpenAI's list (`data[].id`); a ChatGPT
    /// bearer token gets the plan's (`models[]`, those with `visibility == "list"`, by
    /// `slug` with their `display_name`). Neither carries a window: `windows.rs`' table
    /// fills it, `None` for an id it doesn't know.
    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AiError>> {
        Box::pin(async move { Ok(models_list(&self.get_json("models").await?)) })
    }
}

/// The models of a `GET /models` answer, either shape: OpenAI's `{data: [{id}]}` or
/// ChatGPT's `{models: [{slug, display_name, visibility}]}`. Ids keep the API's order,
/// each once.
pub fn models_list(v: &Value) -> Vec<ModelInfo> {
    let entries: Vec<(String, Option<String>)> = match v.get("models").and_then(Value::as_array) {
        Some(models) => models
            .iter()
            .filter(|m| m["visibility"] == "list")
            .filter_map(|m| {
                let id = m["slug"].as_str().or_else(|| m["id"].as_str())?;
                Some((
                    id.to_string(),
                    m["display_name"].as_str().map(str::to_string),
                ))
            })
            .collect(),
        None => v["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| Some((m["id"].as_str()?.to_string(), None)))
            .collect(),
    };
    let mut seen = std::collections::HashSet::new();
    entries
        .into_iter()
        .filter(|(id, _)| !id.is_empty() && seen.insert(id.clone()))
        .map(|(id, display_name)| {
            let window = crate::windows::openai_window(&id);
            ModelInfo {
                context_window: window.map(|w| w.input),
                max_output_tokens: window.map(|w| w.output),
                id,
                display_name,
            }
        })
        .collect()
}

/// A Responses stream as neutral `ModelEvent`s.
pub fn model_events(events: ResponseStream) -> ModelStream {
    let mut adapter = Adapter::default();
    Box::pin(events.flat_map(move |event| {
        let out: Vec<Result<ModelEvent, AiError>> = match event {
            Ok(event) => adapter.event(event).into_iter().map(Ok).collect(),
            Err(e) => vec![Err(e)],
        };
        futures_util::stream::iter(out)
    }))
}

/// Responses events as the `ModelEvent`s `model_events` would stream (for tests that
/// script a Responses-shaped reply).
pub fn to_model_events(events: Vec<StreamEvent>) -> Vec<ModelEvent> {
    let mut adapter = Adapter::default();
    events.into_iter().flat_map(|e| adapter.event(e)).collect()
}

/// Per-stream state of the adaptation.
#[derive(Default)]
struct Adapter {
    /// By output index: the summary part last streamed, and whether text came yet.
    summaries: std::collections::HashMap<u32, (u32, bool)>,
    /// `web_search_call` items finished in this response (for `usage`, when the final
    /// response object doesn't list its output).
    web_searches: u64,
}

impl Adapter {
    /// The `ModelEvent`s of one Responses event: none for the events the loop doesn't
    /// need, two for a finished `web_search_call` (the call, then its result).
    fn event(&mut self, event: StreamEvent) -> Vec<ModelEvent> {
        vec![match event {
            StreamEvent::OutputItemAdded { output_index, item } => ModelEvent::Started {
                index: output_index,
                kind: start_kind(&item),
            },
            StreamEvent::OutputTextDelta {
                output_index,
                delta,
                ..
            } => ModelEvent::Delta {
                index: output_index,
                delta: Delta::Text(delta),
            },
            StreamEvent::FunctionCallArgumentsDelta {
                output_index,
                delta,
                ..
            } => ModelEvent::Delta {
                index: output_index,
                delta: Delta::Arguments(delta),
            },
            StreamEvent::ReasoningSummaryTextDelta {
                output_index,
                summary_index,
                delta,
                ..
            } => {
                let (part, any) = self.summaries.entry(output_index).or_default();
                let mut text = String::new();
                if summary_index != *part {
                    // A new summary part: separated as the finished item's summary is.
                    *part = summary_index;
                    if *any {
                        text.push_str("\n\n");
                    }
                }
                text.push_str(&delta);
                *any |= !text.is_empty();
                if text.is_empty() {
                    return Vec::new();
                }
                ModelEvent::Delta {
                    index: output_index,
                    delta: Delta::Reasoning(text),
                }
            }
            StreamEvent::FunctionCallArgumentsDone { .. } => return Vec::new(),
            StreamEvent::OutputItemDone { output_index, item }
                if item_type(&item) == Some("web_search_call") =>
            {
                self.web_searches += 1;
                return web_search_done(output_index, item);
            }
            StreamEvent::OutputItemDone { output_index, item } => ModelEvent::Done {
                index: output_index,
                item: ModelItem {
                    view: item_view(&item),
                    stored: vec![item],
                },
            },
            StreamEvent::Completed {
                mut usage,
                model,
                status,
                ..
            } => {
                usage.web_search_requests = usage.web_search_requests.max(self.web_searches);
                ModelEvent::Completed {
                    usage,
                    model,
                    stop_reason: status,
                    next: Next::Finished,
                }
            }
            StreamEvent::Incomplete { reason, usage } => ModelEvent::Incomplete { reason, usage },
            StreamEvent::Failed { code, message } | StreamEvent::Error { code, message } => {
                ModelEvent::Failed { code, message }
            }
        }]
    }
}

/// A finished `web_search_call` as two `Done` events on its index, the way Claude
/// sends a search as two blocks: the call (`ItemView::WebSearch` with the query; it
/// carries the item verbatim for `model_items`) and its result
/// (`ItemView::WebSearchResult`: `action.sources` as the results, the status as the
/// error when it isn't `completed`; nothing stored, the call's item already holds it).
fn web_search_done(index: u32, item: Value) -> Vec<ModelEvent> {
    let id = str_field(&item, "id");
    let action = &item["action"];
    let query = action["query"]
        .as_str()
        .map(str::to_string)
        .or_else(|| {
            let queries: Vec<&str> = action["queries"]
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .collect();
            (!queries.is_empty()).then(|| queries.join("; "))
        })
        .or_else(|| action["url"].as_str().map(str::to_string))
        .or_else(|| action["pattern"].as_str().map(str::to_string))
        .unwrap_or_default();
    let results = action["sources"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            Some(margin_shared::WebResult {
                url: s["url"].as_str().filter(|u| !u.is_empty())?.to_string(),
                title: s["title"].as_str().map(str::to_string),
                page_age: None,
            })
        })
        .collect();
    let error = match item["status"].as_str() {
        None | Some("completed") => None,
        Some(status) => Some(status.to_string()),
    };
    vec![
        ModelEvent::Done {
            index,
            item: ModelItem {
                stored: vec![item],
                view: ItemView::WebSearch {
                    id: id.clone(),
                    query,
                },
            },
        },
        ModelEvent::Done {
            index,
            item: ModelItem {
                stored: Vec::new(),
                view: ItemView::WebSearchResult { id, results, error },
            },
        },
    ]
}

fn item_type(item: &Value) -> Option<&str> {
    item.get("type").and_then(Value::as_str)
}

fn str_field(item: &Value, key: &str) -> String {
    item[key].as_str().unwrap_or_default().to_string()
}

fn start_kind(item: &Value) -> StartKind {
    match item_type(item) {
        Some("reasoning") => StartKind::Reasoning,
        Some("message") => StartKind::Text,
        Some("function_call") => StartKind::ToolCall {
            call_id: str_field(item, "call_id"),
            name: str_field(item, "name"),
        },
        Some("web_search_call") => StartKind::WebSearch {
            id: str_field(item, "id"),
        },
        _ => StartKind::Other,
    }
}

/// What a finished output item shows.
fn item_view(item: &Value) -> ItemView {
    match item_type(item) {
        Some("reasoning") => ItemView::Reasoning {
            summary: item["summary"]
                .as_array()
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|p| p["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n")
                })
                .unwrap_or_default(),
        },
        Some("message") => {
            let parts = item["content"].as_array().map(Vec::as_slice).unwrap_or(&[]);
            let mut text = String::new();
            let mut citations = Vec::new();
            for p in parts {
                match p["type"].as_str() {
                    Some("output_text") => {
                        let offset = text.len() as u32;
                        text.push_str(p["text"].as_str().unwrap_or_default());
                        for a in p["annotations"].as_array().into_iter().flatten() {
                            if a["type"] != "url_citation" {
                                continue;
                            }
                            let at = |key: &str| a[key].as_u64().map(|n| offset + n as u32);
                            citations.push(margin_shared::WebCitation {
                                url: str_field(a, "url"),
                                title: a["title"].as_str().map(str::to_string),
                                cited_text: None,
                                start: at("start_index"),
                                end: at("end_index"),
                            });
                        }
                    }
                    Some("refusal") => text.push_str(p["refusal"].as_str().unwrap_or_default()),
                    _ => {}
                }
            }
            ItemView::Text { text, citations }
        }
        Some("function_call") => ItemView::ToolCall {
            call_id: str_field(item, "call_id"),
            name: str_field(item, "name"),
            arguments: str_field(item, "arguments"),
        },
        Some("compaction") => ItemView::Compaction,
        _ => ItemView::Other,
    }
}

/// `response.usage` of a finished response, as one model call; `web_search_requests`
/// counts the `web_search_call` items in its `output` (OpenAI bills each).
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
        web_search_requests: response["output"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item_type(item) == Some("web_search_call"))
            .count() as u64,
        ..Default::default()
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
            tool_choice: Some(ResponsesToolChoice::AllowedTools {
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
    fn an_agent_step_renders_as_today_and_its_events_adapt() {
        use crate::model::{CallOptions, WebTools};
        use margin_shared::pipeline::Route;
        let config = crate::ProviderConfig {
            provider: ProviderKind::OpenAi,
            base_url: "https://example.test/v1".into(),
            credential: crate::Credential::ApiKey("k".into()),
            models: crate::RouteModels {
                chat: "chat-m".into(),
                write: "write-m".into(),
                small: "small-m".into(),
            },
        };
        let c = ResponsesClient::from_config(&config);
        let req = ModelRequest {
            opts: CallOptions::new(Route::Chat).with_cache_key("chat-1"),
            system: "You are Margin.".into(),
            history: vec![json!({"role": "user", "content": "hi"})],
            tools: vec![ToolDef {
                name: "search".into(),
                description: "Search.".into(),
                parameters: json!({"type": "object"}),
            }],
            tool_choice: ToolChoice::Allowed(vec!["search".into()]),
            web: WebTools::default(),
            effort: ReasoningEffort::High,
            compaction: Some(200_000),
            parallel_tool_calls: true,
        };
        let body = c.response_body(&c.agent_request(&req));
        assert_eq!(body["model"], "chat-m");
        assert_eq!(
            body["input"][0],
            json!({"role": "developer", "content": [{"type": "input_text", "text": "You are Margin."}]})
        );
        assert_eq!(body["input"][1]["content"], "hi");
        assert_eq!(body["tools"][0]["strict"], true);
        assert_eq!(body["tool_choice"]["type"], "allowed_tools");
        assert_eq!(
            body["reasoning"],
            json!({"effort": "high", "summary": "auto"})
        );
        assert_eq!(body["prompt_cache_key"], "chat-1");
        assert_eq!(body["context_management"][0]["compact_threshold"], 200_000);
        assert_eq!(body["parallel_tool_calls"], true);
        let small = ModelRequest {
            opts: CallOptions::new(Route::Small),
            tools: vec![],
            ..req
        };
        let body = c.response_body(&c.agent_request(&small));
        assert_eq!(body["model"], "small-m");
        assert!(body.get("tools").is_none() && body.get("tool_choice").is_none());

        let message = json!({"type": "message", "content": [{"type": "output_text", "text": "Rust 1.99.",
            "annotations": [{"type": "url_citation", "url": "https://r.example", "title": "R", "start_index": 0, "end_index": 4}]}]});
        let events = to_model_events(vec![
            StreamEvent::OutputItemAdded {
                output_index: 0,
                item: json!({"type": "reasoning"}),
            },
            StreamEvent::ReasoningSummaryTextDelta {
                output_index: 0,
                item_id: "rs".into(),
                summary_index: 0,
                delta: "a".into(),
            },
            StreamEvent::ReasoningSummaryTextDelta {
                output_index: 0,
                item_id: "rs".into(),
                summary_index: 1,
                delta: "b".into(),
            },
            StreamEvent::OutputItemDone {
                output_index: 1,
                item: message.clone(),
            },
            StreamEvent::Completed {
                response_id: "r".into(),
                usage: Usage::default(),
                model: "gpt-x".into(),
                status: "completed".into(),
            },
        ]);
        assert_eq!(
            events[0],
            ModelEvent::Started {
                index: 0,
                kind: StartKind::Reasoning
            }
        );
        assert_eq!(
            events[2],
            ModelEvent::Delta {
                index: 0,
                delta: Delta::Reasoning("\n\nb".into())
            }
        );
        let ModelEvent::Done { item, .. } = &events[3] else {
            panic!("{events:?}");
        };
        assert_eq!(item.stored, vec![message]);
        let ItemView::Text { text, citations } = &item.view else {
            panic!("{item:?}");
        };
        assert_eq!(text, "Rust 1.99.");
        assert_eq!(citations[0].url, "https://r.example");
        assert_eq!((citations[0].start, citations[0].end), (Some(0), Some(4)));
        assert!(matches!(
            &events[4],
            ModelEvent::Completed { model, stop_reason, next: Next::Finished, .. }
                if model == "gpt-x" && stop_reason == "completed"
        ));
    }

    #[test]
    fn tool_choices() {
        assert_eq!(ResponsesToolChoice::None.to_json(), json!("none"));
        assert_eq!(
            ResponsesToolChoice::Function("x".into()).to_json(),
            json!({"type": "function", "name": "x"})
        );
        assert_eq!(
            ResponsesToolChoice::AllowedTools {
                required: true,
                names: vec![]
            }
            .to_json()["mode"],
            "required"
        );
    }
}
