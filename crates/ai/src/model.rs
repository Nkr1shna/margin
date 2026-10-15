//! The provider boundary (`docs/agent-providers-brief.md` D1): one seam for every
//! model call, whichever API answers it.
//!
//! - [`CallOptions`] travel with every call: the route it runs on, the cache key, an
//!   output cap, where the constant (cacheable) part of the instructions ends, and the
//!   prompt version for the trace.
//! - [`TextModel`] streams plain text: lessons, scenes, the planner, titles, condense
//!   summaries, rerank.
//! - [`Provider`] makes one agent step: the system prompt, the chat's stored items for
//!   this provider, function tools and the hosted web tools in; neutral [`ModelEvent`]s
//!   out, each finished item carrying the provider's wire blocks verbatim
//!   ([`ModelItem::stored`]) for `model_items`.
//!
//! `ResponsesClient` (OpenAI, and ChatGPT under a bearer token) and `ClaudeClient`
//! (Anthropic Messages) implement both traits. This file is frozen after the providers
//! phase's first commit: a change goes through the orchestrator.

use std::pin::Pin;

use futures_util::Stream;
use margin_shared::pipeline::Route;
use margin_shared::{ReasoningEffort, Usage, WebCitation, WebResult};
use serde_json::Value;

use crate::prompts::JsonFormat;
use crate::{AiError, InputPart, Prompt};

pub use margin_shared::{ModelInfo, RouteModels};

/// Which API a client talks to. The same enum as `Chat.provider`.
pub type ProviderKind = margin_shared::Provider;

pub type BoxFuture<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// What authenticates the calls. Never printed.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    /// An API key: `Authorization: Bearer` on OpenAI, `x-api-key` on Claude.
    ApiKey(String),
    /// A ChatGPT plan access token ("Sign in with ChatGPT"); the Responses client drops
    /// the parameters that route doesn't take.
    Bearer(String),
}

impl Credential {
    /// The secret itself, for the request header.
    pub fn secret(&self) -> &str {
        match self {
            Credential::ApiKey(s) | Credential::Bearer(s) => s,
        }
    }
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credential::ApiKey(_) => f.write_str("ApiKey(…)"),
            Credential::Bearer(_) => f.write_str("Bearer(…)"),
        }
    }
}

/// Where and how to call one provider, built by the service per call from Settings and
/// the Keychain. Its `Debug` prints no credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfig {
    pub provider: ProviderKind,
    /// "https://api.openai.com/v1" (Responses) or "https://api.anthropic.com" (Messages).
    pub base_url: String,
    pub credential: Credential,
    /// The model each route uses (Settings' three slots, after any pipeline override).
    pub models: RouteModels,
}

/// A model's context window and output cap (D13), from the provider's model list or
/// `windows.rs`' table; `MODEL_WINDOW_FALLBACK` when neither knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelWindow {
    pub input: u32,
    pub output: u32,
}

/// The window a model gets when its provider reports none and the table doesn't know
/// it (logged, and shown in Settings as "window unknown, using 200k").
pub const MODEL_WINDOW_FALLBACK: ModelWindow = ModelWindow {
    input: 200_000,
    output: 32_000,
};

/// How one model call is made (R1-ai's shape, extended for the providers): the route it
/// runs on, the cache key that groups calls sharing a prefix, an output cap, where the
/// constant part of the instructions ends, and the prompt version for the trace.
///
/// `route` picks the model (`ProviderConfig.models.get(route)`); it also travels to the
/// trace. Nothing here names a model id or a token cap of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallOptions {
    pub route: Route,
    /// OpenAI `prompt_cache_key`; part of the trace's query hash. `None` lets
    /// `ResponsesClient::generate` use the prompt's own key (`lesson:lesson-v2`, …);
    /// plain text calls then send none.
    pub cache_key: Option<String>,
    /// `max_output_tokens` (Responses) / `max_tokens` (Messages); `None` leaves it to the
    /// client's default. Never sent under a ChatGPT bearer token.
    pub max_output_tokens: Option<u32>,
    /// Byte length of the constant part of `instructions` (R1's `Prompt.constant.len()`):
    /// the Claude client ends its first cache block there. `None`: the whole
    /// instructions block. OpenAI caches by automatic prefix and ignores it.
    pub cache_prefix_len: Option<usize>,
    /// The prompt's version (e.g. "lesson-v2", "agent-v2"), for the trace; never sent to
    /// a provider.
    pub prompt_version: Option<String>,
}

impl CallOptions {
    pub fn new(route: Route) -> Self {
        Self {
            route,
            cache_key: None,
            max_output_tokens: None,
            cache_prefix_len: None,
            prompt_version: None,
        }
    }

    pub fn with_cache_key(mut self, key: impl Into<String>) -> Self {
        self.cache_key = Some(key.into());
        self
    }

    pub fn with_max_output_tokens(mut self, max: u32) -> Self {
        self.max_output_tokens = Some(max);
        self
    }

    pub fn with_cache_prefix_len(mut self, len: usize) -> Self {
        self.cache_prefix_len = Some(len);
        self
    }

    pub fn with_prompt_version(mut self, version: impl Into<String>) -> Self {
        self.prompt_version = Some(version.into());
        self
    }

    /// These options completed from `prompt` where the caller left them out: its cache
    /// key, its constant block's length and its version.
    pub fn keyed(&self, prompt: &Prompt) -> CallOptions {
        let mut opts = self.clone();
        if opts.cache_key.is_none() {
            opts.cache_key = Some(prompt.cache_key());
        }
        if opts.cache_prefix_len.is_none() {
            opts.cache_prefix_len = Some(prompt.constant.len());
        }
        if opts.prompt_version.is_none() {
            opts.prompt_version = Some(prompt.version.to_string());
        }
        opts
    }
}

/// What a plain text call returned. `model` and `stop_reason` are what the provider
/// answered with (for the trace), not what was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextOutput {
    pub text: String,
    pub usage: Usage,
    pub model: String,
    pub stop_reason: String,
}

/// Plain streamed text: lessons, scenes, the planner, titles, condense summaries,
/// rerank. `on_delta` gets each piece of text as it streams.
pub trait TextModel: Send + Sync {
    fn stream_parts<'a>(
        &'a self,
        instructions: &'a str,
        input: &'a [InputPart],
        opts: &'a CallOptions,
        on_delta: Box<dyn FnMut(&str) + Send + 'a>,
    ) -> BoxFuture<'a, Result<TextOutput, AiError>>;

    /// A structured answer: JSON following `format`. A provider with a strict-schema
    /// mode uses it (the Responses client sends `text.format`); the default asks in
    /// words, appending the schema to the input as a last text part (Messages has no
    /// `text.format`; the part comes after the cached constant block, so the cached
    /// prefix is unchanged). The caller parses and checks the answer.
    fn stream_json<'a>(
        &'a self,
        instructions: &'a str,
        input: &'a [InputPart],
        format: &'a JsonFormat,
        opts: &'a CallOptions,
    ) -> BoxFuture<'a, Result<TextOutput, AiError>> {
        Box::pin(async move {
            let mut input = input.to_vec();
            input.push(InputPart::Text(format!(
                "Answer with only a JSON object (no prose, no code fence) that follows this JSON schema:\n{}",
                format.schema
            )));
            self.stream_parts(instructions, &input, opts, Box::new(|_| {}))
                .await
        })
    }
}

/// The events of one agent step. Ends after `Completed`, `Incomplete` or `Failed`; a
/// transport failure is an `Err` item (and the end).
pub type ModelStream = Pin<Box<dyn Stream<Item = Result<ModelEvent, AiError>> + Send>>;

/// One agent step's model call.
pub trait Provider: Send + Sync + 'static {
    fn kind(&self) -> ProviderKind;
    /// Starts the call. `Err` before any event (HTTP status, connection) is retried by
    /// the loop when `AiError::is_retryable`.
    fn stream(&self, req: ModelRequest) -> BoxFuture<'_, Result<ModelStream, AiError>>;
    /// The models this provider offers, with their windows when known (D13).
    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AiError>>;
}

/// One agent step, provider-neutral. The client renders it as a Responses or a
/// Messages request.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRequest {
    /// `route` = Chat, `cache_key` = the chat id, `prompt_version` = the agent prompt's.
    pub opts: CallOptions,
    /// The system prompt; the provider places it (a leading developer item on OpenAI,
    /// the cached `system` field on Claude).
    pub system: String,
    /// The chat's stored items for this provider, in replay order (D4).
    pub history: Vec<Value>,
    /// Function tools (strict schemas).
    pub tools: Vec<ToolDef>,
    pub tool_choice: ToolChoice,
    /// The hosted web tools to offer.
    pub web: WebTools,
    pub effort: ReasoningEffort,
    /// Server-side compaction threshold in input tokens; `None` = don't ask the server.
    pub compaction: Option<u32>,
    pub parallel_tool_calls: bool,
}

/// A function tool: name, description, strict JSON schema of its arguments
/// (`additionalProperties: false`, every property required).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Which tools this step may call. Claude has no `allowed_tools`: `Allowed` is sent as
/// `auto` there and the tools gate themselves (D5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChoice {
    Auto,
    /// Answer with text only (the last step, or the tool output budget is used up).
    None,
    /// Only these tools this step (the tool list itself stays constant for the cache).
    Allowed(Vec<String>),
}

/// The provider's hosted web tools to offer on a step.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WebTools {
    pub search: bool,
    /// Claude only (OpenAI's `web_search` opens pages itself).
    pub fetch: bool,
}

/// One event of an agent step, in stream order. `index` identifies the item within the
/// step (Claude's consecutive text blocks share one).
#[derive(Debug, Clone, PartialEq)]
pub enum ModelEvent {
    /// An item started.
    Started { index: u32, kind: StartKind },
    /// More of an item's text, reasoning summary or arguments.
    Delta { index: u32, delta: Delta },
    /// An item finished: what to store, verbatim, and what to show.
    Done { index: u32, item: ModelItem },
    /// The response finished. Terminal.
    Completed {
        usage: Usage,
        /// The model that answered (`response.model`, `message.model`).
        model: String,
        /// As the provider sent it (`completed`, `end_turn`, `pause_turn`, …).
        stop_reason: String,
        next: Next,
    },
    /// The response stopped early (`max_output_tokens`/`max_tokens`, the context window,
    /// a refusal). Items done so far are valid. Terminal.
    Incomplete { reason: String, usage: Usage },
    /// The response failed (`response.failed`, an `error` event). Terminal.
    Failed {
        code: Option<String>,
        message: String,
    },
}

/// What kind of item started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartKind {
    Reasoning,
    Text,
    ToolCall { call_id: String, name: String },
    WebSearch { id: String },
    WebFetch { id: String },
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delta {
    Text(String),
    /// Reasoning summary text; a new summary part starts with a blank line.
    Reasoning(String),
    Arguments(String),
}

/// What comes after a completed response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// The answer is complete (any client tool calls in it are for the loop to run).
    Finished,
    /// Claude's `pause_turn`: call again at once with the same history.
    Paused,
    /// A server tool call is still to run; its result arrives in the next call, after
    /// the client tool results.
    ServerToolPending,
}

/// A finished item: the provider's wire items or blocks to store in `model_items`,
/// verbatim, and the neutral view the loop shows.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelItem {
    pub stored: Vec<Value>,
    pub view: ItemView,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ItemView {
    Reasoning {
        summary: String,
    },
    Text {
        text: String,
        citations: Vec<WebCitation>,
    },
    ToolCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    /// The web search call.
    WebSearch {
        id: String,
        query: String,
    },
    WebSearchResult {
        id: String,
        results: Vec<WebResult>,
        error: Option<String>,
    },
    WebFetch {
        id: String,
        url: String,
    },
    WebFetchResult {
        id: String,
        url: String,
        title: Option<String>,
        error: Option<String>,
    },
    Compaction,
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_never_printed() {
        let config = ProviderConfig {
            provider: ProviderKind::Claude,
            base_url: "https://api.anthropic.com".into(),
            credential: Credential::ApiKey("sk-ant-secret".into()),
            models: RouteModels::claude_default(),
        };
        let printed = format!("{config:?}");
        assert!(!printed.contains("sk-ant-secret"), "{printed}");
        assert!(!format!("{:?}", Credential::Bearer("tok".into())).contains("tok"));
        assert_eq!(config.credential.secret(), "sk-ant-secret");
        assert_eq!(config.models.get(Route::Small), "claude-haiku-5-5");
    }

    #[test]
    fn options_are_completed_from_the_prompt() {
        let prompt = Prompt {
            job: "lesson",
            version: "lesson-v9",
            constant: "CONSTANT",
            variable: "variable".into(),
            input: vec![],
            format: None,
        };
        let opts = CallOptions::new(Route::Write).keyed(&prompt);
        assert_eq!(opts.cache_key.as_deref(), Some("lesson:lesson-v9"));
        assert_eq!(opts.cache_prefix_len, Some("CONSTANT".len()));
        assert_eq!(opts.prompt_version.as_deref(), Some("lesson-v9"));
        assert_eq!(
            &prompt.instructions()[..opts.cache_prefix_len.unwrap()],
            "CONSTANT"
        );
        let given = CallOptions::new(Route::Write)
            .with_cache_key("k")
            .with_cache_prefix_len(3)
            .with_prompt_version("v")
            .keyed(&prompt);
        assert_eq!(
            (given.cache_key.as_deref(), given.cache_prefix_len),
            (Some("k"), Some(3))
        );
        assert_eq!(given.prompt_version.as_deref(), Some("v"));
    }
}
