//! Anthropic's **Messages API** (`POST {base_url}/v1/messages`), called directly over
//! HTTP (`docs/agent-providers-brief.md` D2, D12): the Claude provider.
//!
//! Requests are streamed (SSE) with `anthropic-version: 2023-06-01` and the betas
//! [`CLAUDE_BETAS`]; thinking is adaptive with summarized display and
//! `block_binding.prefix_mismatch_behavior: "drop_block"`; the effort is Settings'
//! reasoning effort (`output_config.effort`); the hosted web tools are
//! [`WEB_SEARCH_TOOL`] and [`WEB_FETCH_TOOL`]. Prompt caching puts `cache_control`
//! breakpoints on the system prompt (or the end of a prompt's constant block), the last
//! tool and the last message.
//!
//! The client itself is area P-ai-claude's; this file holds its frozen constants and
//! constructor.

use crate::model::{
    BoxFuture, CallOptions, ModelInfo, ModelRequest, ModelStream, Provider, ProviderKind,
    RouteModels, TextModel, TextOutput,
};
use crate::{AiError, InputPart};

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
/// The hosted web search tool's type (dynamic filtering and response inclusion; checked
/// against the web search tool docs on 2026-10-14).
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

/// A configured Messages API endpoint. Cheap to clone; its `Debug` prints no key.
#[derive(Clone)]
pub struct ClaudeClient {
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
        Self {
            base_url: base_url.into().trim().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            models,
        }
    }

    /// The model each route calls.
    pub fn models(&self) -> &RouteModels {
        &self.models
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl Provider for ClaudeClient {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Claude
    }

    fn stream(&self, _req: ModelRequest) -> BoxFuture<'_, Result<ModelStream, AiError>> {
        let _ = &self.api_key;
        unimplemented!("providers, P-ai-claude")
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AiError>> {
        unimplemented!("providers, P-ai-claude")
    }
}

impl TextModel for ClaudeClient {
    fn stream_parts<'a>(
        &'a self,
        _instructions: &'a str,
        _input: &'a [InputPart],
        _opts: &'a CallOptions,
        _on_delta: Box<dyn FnMut(&str) + Send + 'a>,
    ) -> BoxFuture<'a, Result<TextOutput, AiError>> {
        unimplemented!("providers, P-ai-claude")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_never_printed() {
        let c = ClaudeClient::new(
            "https://api.anthropic.com/ ",
            "sk-ant-secret",
            RouteModels::claude_default(),
        );
        assert_eq!(c.base_url(), "https://api.anthropic.com");
        assert!(!format!("{c:?}").contains("sk-ant-secret"));
        assert_eq!(c.kind(), ProviderKind::Claude);
        assert_eq!(c.models().chat, "claude-opus-5-5");
    }
}
