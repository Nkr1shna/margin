//! Anthropic's **Messages API** (`POST {base_url}/v1/messages`), called directly over
//! HTTP (`docs/agent-providers-brief.md` D2, D12): the Claude provider.
//!
//! Requests are streamed (SSE) with `anthropic-version: 2023-06-01` and, on an agent
//! step, the betas [`CLAUDE_BETAS`]; thinking is adaptive with summarized display and
//! `block_binding.prefix_mismatch_behavior: "drop_block"`; the effort is Settings'
//! reasoning effort (`output_config.effort`); the hosted web tools are
//! [`WEB_SEARCH_TOOL`] and [`WEB_FETCH_TOOL`]. Prompt caching puts `cache_control`
//! breakpoints on the system prompt (or the end of a prompt's constant block), the last
//! tool and the last message.
//!
//! The stream becomes neutral [`ModelEvent`]s. Each finished item's `stored` entries are
//! `{"role": "assistant", "block": {…}}` with the content block exactly as the API sent
//! it (thinking signatures, `encrypted_content`, `encrypted_index` included), which is
//! what the chat keeps in `model_items` and what `replay::claude` renders back.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, OnceLock};

use futures_util::{Stream, StreamExt};
use margin_shared::{Usage, WebCitation, WebResult};
use serde_json::{Value, json};

use crate::model::{
    BoxFuture, CallOptions, Delta, ItemView, ModelEvent, ModelInfo, ModelItem, ModelRequest,
    ModelStream, Next, Provider, ProviderConfig, ProviderKind, RouteModels, StartKind, TextModel,
    TextOutput, ToolChoice,
};
use crate::sse::{SseEvent, SseParser};
use crate::{
    AiError, CONNECT_TIMEOUT, InputPart, READ_TIMEOUT, check_status, error_chain, error_code,
    error_message, network,
};

/// Default `base_url`.
pub const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
/// The `anthropic-version` header.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// `anthropic-beta`: server-side compaction (`compact_20260112` edits) and the thinking
/// binding controls (`thinking.block_binding`, `input_transformations`).
pub const CLAUDE_BETAS: &str = "compact-2026-01-12,thinking-binding-controls-2026-08-01";
/// `max_tokens` of an agent step unless the options set one.
pub const CLAUDE_MAX_OUTPUT_TOKENS: u32 = 32_000;
/// `max_tokens` of a plain text call (a lesson, a scene, the planner) unless set.
pub const CLAUDE_TEXT_MAX_OUTPUT_TOKENS: u32 = 64_000;
/// The hosted web search tool's type (dynamic filtering and response inclusion). The
/// live web search tool docs list three versions with this one the newest (rechecked
/// 2026-10-14); the claude-api skill's cached table still names `web_search_20260209`,
/// which is older, so the docs win.
pub const WEB_SEARCH_TOOL: &str = "web_search_20260318";
/// The hosted web fetch tool's type (checked against the web fetch tool docs on
/// 2026-10-14).
pub const WEB_FETCH_TOOL: &str = "web_fetch_20260318";
/// `max_uses` of web search per request.
pub const WEB_SEARCH_MAX_USES: u32 = 8;
/// `max_uses` of web fetch per request.
pub const WEB_FETCH_MAX_USES: u32 = 8;
/// `max_content_tokens` of one fetched page.
pub const WEB_FETCH_MAX_CONTENT_TOKENS: u32 = 50_000;

/// The one `cache_control` marker Margin sends (5-minute ephemeral entries).
fn ephemeral() -> Value {
    json!({"type": "ephemeral"})
}

/// `(base_url, model)` pairs whose web tools need `allowed_callers: ["direct"]` (models
/// before 4.6 reject the dynamic-filtering default). Learned from a 400 once per process.
fn direct_callers() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(Default::default)
}

/// A configured Messages API endpoint. Cheap to clone; its `Debug` prints no key.
#[derive(Clone)]
pub struct ClaudeClient {
    http: Result<reqwest::Client, String>,
    base_url: String,
    api_key: String,
    models: RouteModels,
}

impl std::fmt::Debug for ClaudeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeClient")
            .field("base_url", &self.base_url)
            .field("models", &self.models)
            .finish_non_exhaustive()
    }
}

impl ClaudeClient {
    /// `base_url` like "https://api.anthropic.com" (no trailing `/v1/messages`).
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        models: RouteModels,
    ) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .map_err(|e| format!("couldn't set up the HTTP client: {}", error_chain(&e)));
        Self {
            http,
            base_url: base_url.into().trim().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            models,
        }
    }

    /// The client of a Claude `ProviderConfig`: each call's model is
    /// `config.models.get(opts.route)`.
    pub fn from_config(config: &ProviderConfig) -> Self {
        Self::new(
            config.base_url.clone(),
            config.credential.secret(),
            config.models.clone(),
        )
    }

    /// The model each route calls.
    pub fn models(&self) -> &RouteModels {
        &self.models
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn get(&self, path: &str) -> Result<reqwest::RequestBuilder, AiError> {
        let http = self
            .http
            .as_ref()
            .map_err(|e| AiError::Network(e.clone()))?;
        Ok(http
            .get(format!("{}{path}", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION))
    }

    /// `POST /v1/messages` with `body`; a non-2xx answer is `AiError::Http`.
    async fn post(&self, body: &Value, betas: Option<&str>) -> Result<reqwest::Response, AiError> {
        let http = self
            .http
            .as_ref()
            .map_err(|e| AiError::Network(e.clone()))?;
        let mut request = http
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(body);
        if let Some(betas) = betas {
            request = request.header("anthropic-beta", betas);
        }
        let resp = request.send().await.map_err(network)?;
        check_status(resp).await.map_err(spend_limit)
    }

    /// Posts the body `build(direct)` makes. When the web tools are on and the API
    /// answers that they need `allowed_callers` (a model before 4.6), retries once with
    /// `["direct"]` and remembers that for this model.
    async fn open(
        &self,
        model: &str,
        has_web: bool,
        betas: Option<&str>,
        build: impl Fn(bool) -> Value,
    ) -> Result<reqwest::Response, AiError> {
        let key = format!("{} {model}", self.base_url);
        let direct = has_web && direct_callers().lock().unwrap().contains(&key);
        match self.post(&build(direct), betas).await {
            Err(e) if has_web && !direct && asks_for_allowed_callers(&e) => {
                tracing::info!(
                    model,
                    "the web tools need allowed_callers [\"direct\"] on this model; retrying"
                );
                direct_callers().lock().unwrap().insert(key);
                self.post(&build(true), betas).await
            }
            other => other,
        }
    }

    /// The request body of one agent step (D2). `direct` adds
    /// `allowed_callers: ["direct"]` to the web tools.
    fn agent_body(&self, req: &ModelRequest, direct: bool) -> Value {
        let mut body = json!({
            "model": self.models.get(req.opts.route),
            "max_tokens": req.opts.max_output_tokens.unwrap_or(CLAUDE_MAX_OUTPUT_TOKENS),
            "stream": true,
        });
        if !req.system.is_empty() {
            body["system"] = json!([{
                "type": "text",
                "text": req.system,
                "cache_control": ephemeral(),
            }]);
        }
        body["messages"] = Value::Array(messages_with_breakpoint(&req.history));
        let tools = agent_tools(req, direct);
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
            body["tool_choice"] = match req.tool_choice {
                ToolChoice::None => json!({"type": "none"}),
                // Claude has no `allowed_tools`; the tools gate themselves (D5).
                ToolChoice::Auto | ToolChoice::Allowed(_) => {
                    let mut choice = json!({"type": "auto"});
                    if !req.parallel_tool_calls {
                        choice["disable_parallel_tool_use"] = json!(true);
                    }
                    choice
                }
            };
        }
        body["thinking"] = json!({
            "type": "adaptive",
            "display": "summarized",
            "block_binding": {"prefix_mismatch_behavior": "drop_block"},
        });
        body["output_config"] = json!({"effort": req.effort.as_str()});
        if let Some(threshold) = req.compaction {
            body["context_management"] = json!({"edits": [{
                "type": "compact_20260112",
                "trigger": {"type": "input_tokens", "value": threshold},
            }]});
        }
        body
    }

    /// The request body of a plain text call (D2, D12): the instructions as one cached
    /// system block, or two split at `opts.cache_prefix_len` with the marker on the
    /// constant one; one user message of text and image blocks.
    fn text_body(&self, instructions: &str, input: &[InputPart], opts: &CallOptions) -> Value {
        let mut body = json!({
            "model": self.models.get(opts.route),
            "max_tokens": opts.max_output_tokens.unwrap_or(CLAUDE_TEXT_MAX_OUTPUT_TOKENS),
            "stream": true,
        });
        let system = system_blocks(instructions, opts.cache_prefix_len);
        if !system.is_empty() {
            body["system"] = Value::Array(system);
        }
        body["messages"] = json!([{"role": "user", "content": user_content(input)}]);
        body
    }

    /// Streams a text call's events.
    async fn text_events(
        &self,
        instructions: &str,
        input: &[InputPart],
        opts: &CallOptions,
    ) -> Result<ModelStream, AiError> {
        let body = self.text_body(instructions, input, opts);
        let resp = self
            .open(self.models.get(opts.route), false, None, |_| body.clone())
            .await?;
        Ok(pump(resp, Adapter::default()))
    }
}

impl Provider for ClaudeClient {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Claude
    }

    fn stream(&self, req: ModelRequest) -> BoxFuture<'_, Result<ModelStream, AiError>> {
        Box::pin(async move {
            let model = self.models.get(req.opts.route);
            let has_web = req.web.search || req.web.fetch;
            let resp = self
                .open(model, has_web, Some(CLAUDE_BETAS), |direct| {
                    self.agent_body(&req, direct)
                })
                .await?;
            Ok(pump(resp, Adapter::with_history(&req.history)))
        })
    }

    /// `GET /v1/models`, every page: the id, display name, context window
    /// (`max_input_tokens`) and output cap (`max_tokens`) of each model.
    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AiError>> {
        Box::pin(async move {
            let mut models = Vec::new();
            let mut after: Option<String> = None;
            loop {
                let path = match &after {
                    Some(id) => format!("/v1/models?limit=1000&after_id={}", query_value(id)),
                    None => "/v1/models?limit=1000".to_string(),
                };
                let resp = self.get(&path)?.send().await.map_err(network)?;
                let page: Value = check_status(resp).await?.json().await.map_err(network)?;
                let data = page["data"].as_array().cloned().unwrap_or_default();
                let window = |m: &Value, key: &str| {
                    m[key]
                        .as_u64()
                        .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
                };
                for m in &data {
                    let Some(id) = m["id"].as_str() else { continue };
                    models.push(ModelInfo {
                        id: id.to_string(),
                        display_name: m["display_name"].as_str().map(str::to_string),
                        context_window: window(m, "max_input_tokens"),
                        max_output_tokens: window(m, "max_tokens"),
                    });
                }
                let last = page["last_id"]
                    .as_str()
                    .or_else(|| data.last().and_then(|m| m["id"].as_str()));
                match (page["has_more"].as_bool(), last) {
                    (Some(true), Some(last)) if after.as_deref() != Some(last) => {
                        after = Some(last.to_string());
                    }
                    _ => return Ok(models),
                }
            }
        })
    }
}

impl TextModel for ClaudeClient {
    fn stream_parts<'a>(
        &'a self,
        instructions: &'a str,
        input: &'a [InputPart],
        opts: &'a CallOptions,
        mut on_delta: Box<dyn FnMut(&str) + Send + 'a>,
    ) -> BoxFuture<'a, Result<TextOutput, AiError>> {
        Box::pin(async move {
            let mut events = self.text_events(instructions, input, opts).await?;
            let mut text = String::new();
            while let Some(event) = events.next().await {
                match event? {
                    ModelEvent::Delta {
                        delta: Delta::Text(t),
                        ..
                    } => on_delta(&t),
                    ModelEvent::Done {
                        item:
                            ModelItem {
                                view: ItemView::Text { text: t, .. },
                                ..
                            },
                        ..
                    } => text.push_str(&t),
                    ModelEvent::Completed {
                        usage,
                        model,
                        stop_reason,
                        ..
                    } => {
                        return Ok(TextOutput {
                            text,
                            usage,
                            model,
                            stop_reason,
                        });
                    }
                    ModelEvent::Incomplete { reason, .. } => {
                        return Err(AiError::Incomplete(incomplete_words(&reason)));
                    }
                    ModelEvent::Failed { code, message } => {
                        return Err(stream_error(code, message));
                    }
                    _ => {}
                }
            }
            Err(AiError::Network(
                "the connection closed before the response finished".into(),
            ))
        })
    }
}

// ---------------------------------------------------------------------------
// Request rendering
// ---------------------------------------------------------------------------

/// The agent step's tools: each function tool strict, the last one cached; then the
/// hosted web tools asked for.
fn agent_tools(req: &ModelRequest, direct: bool) -> Vec<Value> {
    let mut tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters,
                "strict": true,
            })
        })
        .collect();
    if let Some(last) = tools.last_mut() {
        last["cache_control"] = ephemeral();
    }
    let direct_callers = |mut tool: Value| {
        if direct {
            tool["allowed_callers"] = json!(["direct"]);
        }
        tool
    };
    if req.web.search {
        tools.push(direct_callers(json!({
            "type": WEB_SEARCH_TOOL,
            "name": "web_search",
            "max_uses": WEB_SEARCH_MAX_USES,
        })));
    }
    if req.web.fetch {
        tools.push(direct_callers(json!({
            "type": WEB_FETCH_TOOL,
            "name": "web_fetch",
            "max_uses": WEB_FETCH_MAX_USES,
            "citations": {"enabled": true},
            "max_content_tokens": WEB_FETCH_MAX_CONTENT_TOKENS,
        })));
    }
    tools
}

/// The history as Messages API messages, with a `cache_control` breakpoint on the last
/// block of the last message (moving a marker is not an edit).
///
/// `history` is what `replay::claude` rendered: `{role, content}` messages, taken as
/// they are. Stored entries (`{role, block}`) are accepted too: consecutive blocks of
/// one role become one message.
fn messages_with_breakpoint(history: &[Value]) -> Vec<Value> {
    let mut messages: Vec<Value> = Vec::new();
    let mut last_from_blocks = false;
    for entry in history {
        match (
            entry.get("role").and_then(Value::as_str),
            entry.get("block"),
        ) {
            (Some(role), Some(block)) => {
                match messages.last_mut() {
                    Some(m) if last_from_blocks && m["role"] == role => {
                        if let Some(content) = m["content"].as_array_mut() {
                            content.push(block.clone());
                        }
                    }
                    _ => messages.push(json!({"role": role, "content": [block.clone()]})),
                }
                last_from_blocks = true;
            }
            _ => {
                messages.push(entry.clone());
                last_from_blocks = false;
            }
        }
    }
    if let Some(last) = messages.last_mut() {
        if let Some(text) = last["content"].as_str().map(str::to_string) {
            last["content"] = json!([{"type": "text", "text": text}]);
        }
        if let Some(content) = last["content"].as_array_mut() {
            // Thinking blocks can't carry a marker, nor can an empty text block.
            let target = content.iter_mut().rev().find(|b| {
                let kind = b["type"].as_str().unwrap_or_default();
                !matches!(kind, "thinking" | "redacted_thinking")
                    && !(kind == "text" && b["text"].as_str().is_some_and(str::is_empty))
            });
            if let Some(block) = target.and_then(Value::as_object_mut) {
                block.insert("cache_control".into(), ephemeral());
            }
        }
    }
    messages
}

/// The instructions as `system` blocks: `[constant + marker, variable]` when the
/// constant part's length is given (and falls inside the text on a character
/// boundary), else one block with the marker. Empty instructions send no system.
fn system_blocks(instructions: &str, prefix_len: Option<usize>) -> Vec<Value> {
    if instructions.is_empty() {
        return vec![];
    }
    let split =
        prefix_len.filter(|&n| n > 0 && n < instructions.len() && instructions.is_char_boundary(n));
    match split {
        Some(n) => vec![
            json!({"type": "text", "text": &instructions[..n], "cache_control": ephemeral()}),
            json!({"type": "text", "text": &instructions[n..]}),
        ],
        None => vec![json!({"type": "text", "text": instructions, "cache_control": ephemeral()})],
    }
}

/// A user message's content: text and base64 image blocks in order (adjacent texts
/// joined, empty texts left out unless nothing else is there).
fn user_content(parts: &[InputPart]) -> Vec<Value> {
    use base64::Engine;
    let mut content = Vec::new();
    let mut text = String::new();
    for part in parts {
        match part {
            InputPart::Text(t) => text.push_str(t),
            InputPart::Image(image) => {
                if !text.is_empty() {
                    content.push(json!({"type": "text", "text": std::mem::take(&mut text)}));
                }
                content.push(json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": image.media_type,
                        "data": base64::engine::general_purpose::STANDARD.encode(&image.data),
                    },
                }));
            }
        }
    }
    if !text.is_empty() || content.is_empty() {
        content.push(json!({"type": "text", "text": text}));
    }
    content
}

/// A model id as a query value (ids are `[a-z0-9-.]`; anything else is escaped).
fn query_value(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Is this the 400 the web tools give on a model that needs `allowed_callers` set?
fn asks_for_allowed_callers(e: &AiError) -> bool {
    matches!(e, AiError::Http { status: 400, message, .. } if message.contains("allowed_callers"))
}

/// The other shape of Anthropic's spend limit: a 400 "You have reached your specified
/// API usage limits" gets the 429's code, so both read as `spend_limit_reached`.
fn spend_limit(e: AiError) -> AiError {
    match e {
        AiError::Http {
            status: 400,
            message,
            retry_after,
            ..
        } if message.contains("specified API usage limits") => AiError::Http {
            status: 400,
            message,
            retry_after,
            code: Some("enforced_spend_limit_reached".into()),
        },
        other => other,
    }
}

/// A mid-stream `error` event as an `AiError` for a text call: overloaded, server and
/// rate-limit errors keep their HTTP meaning (so they are retried like the status).
fn stream_error(code: Option<String>, message: String) -> AiError {
    let status = match code.as_deref() {
        Some("overloaded_error") => 529,
        Some("api_error") => 500,
        Some("timeout_error") => 504,
        Some("rate_limit_error") => 429,
        _ => return AiError::Incomplete(format!("the response failed: {message}")),
    };
    AiError::Http {
        status,
        message,
        retry_after: None,
        code,
    }
}

fn incomplete_words(reason: &str) -> String {
    match reason {
        "max_tokens" => "the answer hit the output token limit".into(),
        "model_context_window_exceeded" => "the request exceeds the model's context window".into(),
        "refusal" => "the model declined to answer".into(),
        other if other.starts_with("refusal:") => {
            format!(
                "the model declined to answer ({})",
                &other["refusal:".len()..]
            )
        }
        other => format!("the response is incomplete ({other})"),
    }
}

// ---------------------------------------------------------------------------
// The stream
// ---------------------------------------------------------------------------

fn pump(resp: reqwest::Response, adapter: Adapter) -> ModelStream {
    let pump = Pump {
        bytes: Box::pin(resp.bytes_stream()),
        parser: SseParser::new(),
        adapter,
        queue: VecDeque::new(),
        eof: false,
        done: false,
    };
    Box::pin(futures_util::stream::unfold(pump, Pump::next))
}

/// Reads one streamed response and turns its SSE events into `ModelEvent`s.
struct Pump<S> {
    bytes: S,
    parser: SseParser,
    adapter: Adapter,
    queue: VecDeque<Result<ModelEvent, AiError>>,
    eof: bool,
    /// A terminal event or an error was yielded; nothing follows.
    done: bool,
}

fn is_terminal(event: &ModelEvent) -> bool {
    matches!(
        event,
        ModelEvent::Completed { .. } | ModelEvent::Incomplete { .. } | ModelEvent::Failed { .. }
    )
}

impl<S, B> Pump<S>
where
    S: Stream<Item = reqwest::Result<B>> + Unpin,
    B: AsRef<[u8]>,
{
    async fn next(mut self) -> Option<(Result<ModelEvent, AiError>, Self)> {
        loop {
            if self.done {
                return None;
            }
            if let Some(item) = self.queue.pop_front() {
                if !matches!(&item, Ok(e) if !is_terminal(e)) {
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
        let data = event.data.trim();
        if data.is_empty() {
            return;
        }
        match serde_json::from_str::<Value>(data) {
            Ok(v) => self
                .queue
                .extend(self.adapter.event(&v).into_iter().map(Ok)),
            Err(e) => self.queue.push_back(Err(AiError::Parse(format!(
                "a stream event isn't JSON ({e})"
            )))),
        }
    }
}

/// What a content block is, for its accumulation and its view.
#[derive(Debug, Clone, PartialEq)]
enum BlockKind {
    Text,
    Thinking,
    RedactedThinking,
    ToolUse,
    ServerToolUse,
    WebSearchResult,
    WebFetchResult,
    Compaction,
    Other,
}

/// A content block being streamed.
struct Block {
    /// The neutral item it belongs to.
    index: u32,
    kind: BlockKind,
    /// The block as `content_block_start` sent it.
    raw: Value,
    /// Text, thinking, partial tool input JSON, or compaction content so far.
    buf: String,
    signature: String,
    citations: Vec<Value>,
    /// Where this text block starts in the merged text item.
    text_start: usize,
}

/// The open text item: consecutive `text` blocks of one response are one item.
struct TextItem {
    index: u32,
    stored: Vec<Value>,
    text: String,
    citations: Vec<WebCitation>,
}

/// A fetched page a `char_location` citation can point at.
#[derive(Clone)]
struct FetchedDoc {
    url: String,
    title: Option<String>,
}

/// Per-response state of the SSE → `ModelEvent` adaptation (D2).
#[derive(Default)]
struct Adapter {
    model: String,
    usage: Usage,
    /// Claude's own `input_tokens` (cache reads and writes left out).
    uncached_input: u64,
    stop_reason: Option<String>,
    /// `stop_details.category` of a refusal (`cyber`, `bio`, ...).
    refusal_category: Option<String>,
    next_index: u32,
    blocks: HashMap<u64, Block>,
    text: Option<TextItem>,
    /// `server_tool_use` ids of this response without their result yet.
    pending_server: Vec<String>,
    /// The URL each `web_fetch` call asked for, by call id.
    fetch_urls: HashMap<String, String>,
    /// Pages fetched in the history and in this response, in order (a `char_location`
    /// citation's `document_index` counts them).
    documents: Vec<FetchedDoc>,
}

fn stored(block: Value) -> Value {
    json!({"role": "assistant", "block": block})
}

fn str_of(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().to_string()
}

fn opt_str(v: &Value, key: &str) -> Option<String> {
    v[key].as_str().map(str::to_string)
}

/// A fetched page out of a `web_fetch_tool_result` block, when it succeeded.
fn fetched_doc(block: &Value) -> Option<FetchedDoc> {
    let content = &block["content"];
    (content["type"] == "web_fetch_result").then(|| FetchedDoc {
        url: str_of(content, "url"),
        title: content
            .pointer("/content/title")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

impl Adapter {
    /// Knows the pages fetched earlier in the chat (for web fetch citations).
    fn with_history(history: &[Value]) -> Self {
        let mut adapter = Adapter::default();
        let mut blocks: Vec<&Value> = Vec::new();
        for entry in history {
            if let Some(block) = entry.get("block") {
                blocks.push(block);
            } else if let Some(content) = entry["content"].as_array() {
                blocks.extend(content);
            }
        }
        adapter.documents = blocks
            .into_iter()
            .filter(|b| b["type"] == "web_fetch_tool_result")
            .filter_map(fetched_doc)
            .collect();
        adapter
    }

    fn next_index(&mut self) -> u32 {
        let i = self.next_index;
        self.next_index += 1;
        i
    }

    /// One parsed SSE event's `ModelEvent`s.
    fn event(&mut self, v: &Value) -> Vec<ModelEvent> {
        let mut out = Vec::new();
        match v["type"].as_str().unwrap_or_default() {
            "message_start" => {
                let message = &v["message"];
                self.model = str_of(message, "model");
                self.apply_usage(&message["usage"]);
                if let Some(changes) = message["input_transformations"].as_array()
                    && !changes.is_empty()
                {
                    let changes = Value::Array(changes.clone());
                    tracing::info!(
                        model = %self.model,
                        transformations = %changes,
                        "the API dropped blocks of the replayed history"
                    );
                }
            }
            "content_block_start" => self.block_start(v, &mut out),
            "content_block_delta" => self.block_delta(v, &mut out),
            "content_block_stop" => self.block_stop(v, &mut out),
            "message_delta" => {
                if let Some(reason) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(reason.to_string());
                }
                if let Some(category) = v
                    .pointer("/delta/stop_details/category")
                    .and_then(Value::as_str)
                {
                    tracing::info!(category, "the model declined to answer");
                    self.refusal_category = Some(category.to_string());
                }
                self.apply_usage(&v["usage"]);
                self.flush_text(&mut out);
            }
            "message_stop" => {
                self.flush_text(&mut out);
                out.push(self.terminal());
            }
            "error" => {
                let error = &v["error"];
                out.push(ModelEvent::Failed {
                    code: error_code(v),
                    message: error_message(error)
                        .or_else(|| error["type"].as_str().map(str::to_string))
                        .unwrap_or_else(|| "the stream failed".into()),
                });
            }
            _ => {} // ping and anything new
        }
        out
    }

    /// The terminal event once the message is complete.
    fn terminal(&mut self) -> ModelEvent {
        let stop_reason = self.stop_reason.clone().unwrap_or_default();
        let usage = self.usage;
        let next = match stop_reason.as_str() {
            "max_tokens" | "model_context_window_exceeded" | "refusal" => {
                // A refusal carries its category: `refusal:cyber`.
                let reason = match &self.refusal_category {
                    Some(category) if stop_reason == "refusal" => {
                        format!("refusal:{category}")
                    }
                    _ => stop_reason,
                };
                return ModelEvent::Incomplete { reason, usage };
            }
            "pause_turn" => Next::Paused,
            "tool_use" if !self.pending_server.is_empty() => Next::ServerToolPending,
            _ => Next::Finished,
        };
        ModelEvent::Completed {
            usage,
            model: self.model.clone(),
            stop_reason,
            next,
        }
    }

    /// Usage fields are cumulative: each one present replaces what came before.
    /// Claude's `input_tokens` leaves out cache reads and writes; `Usage.input_tokens`
    /// counts every input token, so the three are added together.
    fn apply_usage(&mut self, u: &Value) {
        if !u.is_object() {
            return;
        }
        let n = |path: &str| u.pointer(path).and_then(Value::as_u64);
        self.usage.model_calls = 1;
        if let Some(x) = n("/input_tokens") {
            self.uncached_input = x;
        }
        let usage = &mut self.usage;
        if let Some(x) = n("/cache_read_input_tokens") {
            usage.cached_input_tokens = x;
        }
        if let Some(x) = n("/cache_creation_input_tokens") {
            usage.cache_write_input_tokens = x;
        }
        if let Some(x) = n("/output_tokens") {
            usage.output_tokens = x;
        }
        if let Some(x) = n("/output_tokens_details/thinking_tokens") {
            usage.reasoning_tokens = x;
        }
        if let Some(x) = n("/server_tool_use/web_search_requests") {
            usage.web_search_requests = x;
        }
        if let Some(x) = n("/server_tool_use/web_fetch_requests") {
            usage.web_fetch_requests = x;
        }
        let mut uncached = self.uncached_input;
        // With server-side compaction the call ran in iterations (the compaction, then
        // the answer); the top-level counts are the last one only.
        if let Some(iterations) = u["iterations"].as_array()
            && !iterations.is_empty()
        {
            let sum =
                |key: &str| -> u64 { iterations.iter().filter_map(|i| i[key].as_u64()).sum() };
            let has = |key: &str| iterations.iter().any(|i| i[key].is_u64());
            uncached = sum("input_tokens");
            usage.output_tokens = sum("output_tokens");
            if has("cache_read_input_tokens") || has("cache_creation_input_tokens") {
                usage.cached_input_tokens = sum("cache_read_input_tokens");
                usage.cache_write_input_tokens = sum("cache_creation_input_tokens");
            }
        }
        usage.input_tokens = uncached + usage.cached_input_tokens + usage.cache_write_input_tokens;
    }

    fn flush_text(&mut self, out: &mut Vec<ModelEvent>) {
        if let Some(item) = self.text.take() {
            out.push(ModelEvent::Done {
                index: item.index,
                item: ModelItem {
                    stored: item.stored,
                    view: ItemView::Text {
                        text: item.text,
                        citations: item.citations,
                    },
                },
            });
        }
    }

    fn block_start(&mut self, v: &Value, out: &mut Vec<ModelEvent>) {
        let api_index = v["index"].as_u64().unwrap_or_default();
        let raw = v["content_block"].clone();
        let kind = match raw["type"].as_str().unwrap_or_default() {
            "text" => BlockKind::Text,
            "thinking" => BlockKind::Thinking,
            "redacted_thinking" => BlockKind::RedactedThinking,
            "tool_use" => BlockKind::ToolUse,
            "server_tool_use" => BlockKind::ServerToolUse,
            "web_search_tool_result" => BlockKind::WebSearchResult,
            "web_fetch_tool_result" => BlockKind::WebFetchResult,
            "compaction" => BlockKind::Compaction,
            _ => BlockKind::Other,
        };
        if kind == BlockKind::Text {
            let item = match self.text.take() {
                Some(item) => item,
                None => {
                    let index = self.next_index();
                    out.push(ModelEvent::Started {
                        index,
                        kind: StartKind::Text,
                    });
                    TextItem {
                        index,
                        stored: vec![],
                        text: String::new(),
                        citations: vec![],
                    }
                }
            };
            let index = item.index;
            let text_start = item.text.len();
            self.text = Some(item);
            let initial = str_of(&raw, "text");
            let citations = raw["citations"].as_array().cloned().unwrap_or_default();
            self.blocks.insert(
                api_index,
                Block {
                    index,
                    kind,
                    raw,
                    buf: String::new(),
                    signature: String::new(),
                    citations,
                    text_start,
                },
            );
            self.text_delta(api_index, &initial, out);
            return;
        }
        self.flush_text(out);
        let index = self.next_index();
        let start = match kind {
            BlockKind::Thinking | BlockKind::RedactedThinking => StartKind::Reasoning,
            BlockKind::ToolUse => StartKind::ToolCall {
                call_id: str_of(&raw, "id"),
                name: str_of(&raw, "name"),
            },
            BlockKind::ServerToolUse => {
                let id = str_of(&raw, "id");
                self.pending_server.push(id.clone());
                match raw["name"].as_str() {
                    Some("web_search") => StartKind::WebSearch { id },
                    Some("web_fetch") => StartKind::WebFetch { id },
                    _ => StartKind::Other,
                }
            }
            _ => StartKind::Other,
        };
        out.push(ModelEvent::Started { index, kind: start });
        let initial = match kind {
            BlockKind::Thinking => str_of(&raw, "thinking"),
            _ => String::new(),
        };
        if !initial.is_empty() {
            out.push(ModelEvent::Delta {
                index,
                delta: Delta::Reasoning(initial.clone()),
            });
        }
        self.blocks.insert(
            api_index,
            Block {
                index,
                kind,
                raw,
                buf: initial,
                signature: String::new(),
                citations: vec![],
                text_start: 0,
            },
        );
    }

    fn text_delta(&mut self, api_index: u64, text: &str, out: &mut Vec<ModelEvent>) {
        if text.is_empty() {
            return;
        }
        let Some(block) = self.blocks.get_mut(&api_index) else {
            return;
        };
        block.buf.push_str(text);
        if let Some(item) = self.text.as_mut() {
            item.text.push_str(text);
        }
        out.push(ModelEvent::Delta {
            index: block.index,
            delta: Delta::Text(text.to_string()),
        });
    }

    fn block_delta(&mut self, v: &Value, out: &mut Vec<ModelEvent>) {
        let api_index = v["index"].as_u64().unwrap_or_default();
        let delta = &v["delta"];
        if delta["type"] == "text_delta" {
            self.text_delta(api_index, delta["text"].as_str().unwrap_or_default(), out);
            return;
        }
        let Some(block) = self.blocks.get_mut(&api_index) else {
            return;
        };
        match delta["type"].as_str().unwrap_or_default() {
            "thinking_delta" => {
                let t = delta["thinking"].as_str().unwrap_or_default();
                block.buf.push_str(t);
                if !t.is_empty() {
                    out.push(ModelEvent::Delta {
                        index: block.index,
                        delta: Delta::Reasoning(t.to_string()),
                    });
                }
            }
            "signature_delta" => {
                block
                    .signature
                    .push_str(delta["signature"].as_str().unwrap_or_default());
            }
            "input_json_delta" => {
                let t = delta["partial_json"].as_str().unwrap_or_default();
                block.buf.push_str(t);
                if !t.is_empty() {
                    out.push(ModelEvent::Delta {
                        index: block.index,
                        delta: Delta::Arguments(t.to_string()),
                    });
                }
            }
            "citations_delta" => block.citations.push(delta["citation"].clone()),
            "compaction_delta" => {
                block
                    .buf
                    .push_str(delta["content"].as_str().unwrap_or_default());
            }
            _ => {}
        }
    }

    fn block_stop(&mut self, v: &Value, out: &mut Vec<ModelEvent>) {
        let api_index = v["index"].as_u64().unwrap_or_default();
        let Some(block) = self.blocks.remove(&api_index) else {
            return;
        };
        let Block {
            index,
            kind,
            mut raw,
            buf,
            signature,
            citations,
            text_start,
        } = block;
        let view = match kind {
            BlockKind::Text => {
                raw["text"] = json!(buf);
                if !citations.is_empty() || raw.get("citations").is_some() {
                    raw["citations"] = Value::Array(citations.clone());
                }
                let start = u32::try_from(text_start).ok();
                let end = u32::try_from(text_start + buf.len()).ok();
                let cites: Vec<WebCitation> = citations
                    .iter()
                    .filter_map(|c| self.citation(c, start, end))
                    .collect();
                if let Some(item) = self.text.as_mut() {
                    item.stored.push(stored(raw));
                    item.citations.extend(cites);
                }
                return; // done with the whole text item
            }
            BlockKind::Thinking => {
                raw["thinking"] = json!(buf);
                if !signature.is_empty() {
                    raw["signature"] = json!(signature);
                }
                ItemView::Reasoning { summary: buf }
            }
            BlockKind::RedactedThinking => ItemView::Reasoning {
                summary: String::new(),
            },
            BlockKind::ToolUse | BlockKind::ServerToolUse => {
                let input = if buf.trim().is_empty() {
                    raw.get("input").cloned().unwrap_or_else(|| json!({}))
                } else {
                    serde_json::from_str(&buf).unwrap_or_else(|_| json!({}))
                };
                let arguments = if buf.trim().is_empty() {
                    input.to_string()
                } else {
                    buf
                };
                raw["input"] = input.clone();
                let id = str_of(&raw, "id");
                match (kind, raw["name"].as_str()) {
                    (BlockKind::ToolUse, _) => ItemView::ToolCall {
                        call_id: id,
                        name: str_of(&raw, "name"),
                        arguments,
                    },
                    (_, Some("web_search")) => ItemView::WebSearch {
                        id,
                        query: str_of(&input, "query"),
                    },
                    (_, Some("web_fetch")) => {
                        let url = str_of(&input, "url");
                        self.fetch_urls.insert(id.clone(), url.clone());
                        ItemView::WebFetch { id, url }
                    }
                    _ => ItemView::Other,
                }
            }
            BlockKind::WebSearchResult => {
                let id = str_of(&raw, "tool_use_id");
                self.pending_server.retain(|p| *p != id);
                let content = &raw["content"];
                let results = content
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|r| r["type"] == "web_search_result")
                    .map(|r| WebResult {
                        url: str_of(r, "url"),
                        title: opt_str(r, "title"),
                        page_age: opt_str(r, "page_age"),
                    })
                    .collect();
                let error = (!content.is_array()).then(|| {
                    content["error_code"]
                        .as_str()
                        .unwrap_or("unavailable")
                        .to_string()
                });
                ItemView::WebSearchResult { id, results, error }
            }
            BlockKind::WebFetchResult => {
                let id = str_of(&raw, "tool_use_id");
                self.pending_server.retain(|p| *p != id);
                let asked = self.fetch_urls.get(&id).cloned().unwrap_or_default();
                match fetched_doc(&raw) {
                    Some(doc) => {
                        self.documents.push(doc.clone());
                        ItemView::WebFetchResult {
                            id,
                            url: if doc.url.is_empty() { asked } else { doc.url },
                            title: doc.title,
                            error: None,
                        }
                    }
                    None => ItemView::WebFetchResult {
                        id,
                        url: asked,
                        title: None,
                        error: Some(
                            raw["content"]["error_code"]
                                .as_str()
                                .unwrap_or("unavailable")
                                .to_string(),
                        ),
                    },
                }
            }
            BlockKind::Compaction => {
                if !buf.is_empty() {
                    raw["content"] = json!(buf);
                }
                ItemView::Compaction
            }
            BlockKind::Other => ItemView::Other,
        };
        out.push(ModelEvent::Done {
            index,
            item: ModelItem {
                stored: vec![stored(raw)],
                view,
            },
        });
    }

    /// A text block's citation as a `WebCitation` spanning the block in the merged
    /// text; `None` for a kind that points at no web page.
    fn citation(&self, c: &Value, start: Option<u32>, end: Option<u32>) -> Option<WebCitation> {
        let cited_text = opt_str(c, "cited_text");
        match c["type"].as_str()? {
            "web_search_result_location" => Some(WebCitation {
                url: str_of(c, "url"),
                title: opt_str(c, "title"),
                cited_text,
                start,
                end,
            }),
            "char_location" => {
                let title = opt_str(c, "document_title");
                let doc = c["document_index"]
                    .as_u64()
                    .and_then(|i| self.documents.get(usize::try_from(i).ok()?))
                    .or_else(|| {
                        self.documents
                            .iter()
                            .find(|d| title.is_some() && d.title == title)
                    });
                let url = opt_str(c, "url").or_else(|| doc.map(|d| d.url.clone()))?;
                Some(WebCitation {
                    url,
                    title: title.or_else(|| doc.and_then(|d| d.title.clone())),
                    cited_text,
                    start,
                    end,
                })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ToolDef, WebTools};
    use margin_shared::ReasoningEffort;
    use margin_shared::pipeline::Route;

    fn client() -> ClaudeClient {
        ClaudeClient::new(
            "https://api.anthropic.com/ ",
            "sk-ant-secret",
            RouteModels::claude_default(),
        )
    }

    #[test]
    fn the_key_is_never_printed() {
        let c = client();
        assert_eq!(c.base_url(), "https://api.anthropic.com");
        assert!(!format!("{c:?}").contains("sk-ant-secret"));
        assert_eq!(c.kind(), ProviderKind::Claude);
        assert_eq!(c.models().chat, "claude-opus-5-5");
    }

    fn request() -> ModelRequest {
        ModelRequest {
            opts: CallOptions::new(Route::Chat).with_cache_key("chat:1"),
            system: "SYSTEM".into(),
            history: vec![
                json!({"role": "user", "block": {"type": "text", "text": "ctx"}}),
                json!({"role": "user", "block": {"type": "text", "text": "hi"}}),
                json!({"role": "assistant", "block": {"type": "text", "text": "hello"}}),
                json!({"role": "assistant", "block": {"type": "thinking", "thinking": "t", "signature": "s"}}),
            ],
            tools: vec![ToolDef {
                name: "a".into(),
                description: "A".into(),
                parameters: json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
            }],
            tool_choice: ToolChoice::Allowed(vec!["a".into()]),
            web: WebTools {
                search: true,
                fetch: false,
            },
            effort: ReasoningEffort::High,
            compaction: None,
            parallel_tool_calls: true,
        }
    }

    #[test]
    fn stored_entries_are_grouped_and_the_marker_skips_thinking() {
        let body = client().agent_body(&request(), false);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["content"].as_array().unwrap().len(), 2);
        let last = messages[1]["content"].as_array().unwrap();
        assert_eq!(last[0]["cache_control"], ephemeral());
        assert!(last[1].get("cache_control").is_none());
        assert_eq!(body["tool_choice"], json!({"type": "auto"}));
        assert!(body["tools"][1].get("allowed_callers").is_none());
        let direct = client().agent_body(&request(), true);
        assert_eq!(direct["tools"][1]["allowed_callers"], json!(["direct"]));
    }

    #[test]
    fn system_split_only_on_a_boundary_inside_the_text() {
        assert_eq!(system_blocks("abcdef", Some(3)).len(), 2);
        assert_eq!(system_blocks("abcdef", Some(6)).len(), 1);
        assert_eq!(system_blocks("abcdef", Some(0)).len(), 1);
        assert_eq!(system_blocks("é", Some(1)).len(), 1);
        assert!(system_blocks("", Some(1)).is_empty());
    }

    #[test]
    fn spend_limit_400_reads_as_the_spend_limit() {
        let e = spend_limit(AiError::Http {
            status: 400,
            message: "You have reached your specified API usage limits.".into(),
            retry_after: None,
            code: Some("invalid_request_error".into()),
        });
        assert_eq!(e.code(), Some("enforced_spend_limit_reached"));
        assert!(!e.is_retryable());
    }

    #[test]
    fn stream_errors_keep_their_http_meaning() {
        assert!(stream_error(Some("overloaded_error".into()), "Overloaded".into()).is_retryable());
        assert!(!stream_error(Some("invalid_request_error".into()), "no".into()).is_retryable());
    }
}
