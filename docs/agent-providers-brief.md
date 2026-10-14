## Switchable model providers — design and the binding build brief

Owner decisions (2026-10-14, superseding `docs/agent-engine-review.md`'s ACP/Codex options): Margin keeps its own loop, tools, cards and confirmations; only the model provider changes. Three providers: **OpenAI** (Responses API, API key), **ChatGPT** (Responses API with a "Sign in with ChatGPT" plan token, SIWC token sharing for local apps), **Claude** (Messages API, API key; no claude.ai login). Claude is called directly from Rust (no official Rust SDK; the Agent SDK is rejected). Web search uses each provider's hosted tool; the old "no OpenAI hosted web search" decision is reopened. Provider chosen per chat at New chat, default in Settings; a chat stays on its provider. Lessons, videos, outlines, the condense and title calls follow the default provider. Latest Claude models: `claude-opus-5-5`, `claude-sonnet-5-5`, `claude-haiku-5-5`.

**Re-cut 2026-10-14 (second version)** against `docs/agent-rag-architecture.md` (R1, at `92b3d7d`) and `docs/agent-phase4-brief.md`, which both merge before this phase is built. Owner decisions since the first version, now binding here: **three model slots** per provider in Settings (chats / writing / small tasks); **no fixed context budget** (budgets scale with the route's model window, `Budget.share_of_window`); **evals outside the app, a trace of every model call in SurrealDB with its cost**; Claude defaults `claude-opus-5-5` for chats and writing, `claude-haiku-5-5` for small tasks; the product is becoming a multi-user "Live context" app (later phase; nothing here assumes one user beyond what phase 3 already does). What changed from the first version: one model seam (`CallOptions` + `Route`, D1) instead of two unrelated traits; Claude cache breakpoints on R1's constant-first prompts (D2, D12); per-route model choice per provider and model windows for the budget (D9, D13); trace fields per provider and `evals/prices.toml` rows per provider (D14); the first commit and areas re-cut against the files R1 and phase 4 own (§3, §4); tests updated (§5); Q3 of the first version is answered by the three slots (§8).

Read: `crates/ai/src/{lib,responses,sse,prompts}.rs`, `crates/agent/src/{model,turn,replay,scripted,sink,prompt,tool,registry}.rs`, `crates/service/src/{engines,state,settings,secrets,turns,chats,pipeline,trace}.rs`, `crates/shared/src/{lib,chat,pipeline,context}.rs`, `tests/common/mod.rs`, `crates/app/src/ui/{settings,chat}.rs`, `evals/prices.toml`, `crates/evals/src/lib.rs`. Facts about the APIs were checked against the live docs when the first version was written: Claude web search (`web_search_20260318`, `pause_turn`, `encrypted_content`, pricing), web fetch, streaming, thinking/preserved thinking, threshold compaction, rate limits, errors, server tools; OpenAI SIWC (sign-in, models and inference, token reference, preview limitations, errors) and the Responses `web_search` tool. For this version: the Models API's `max_input_tokens`/`max_tokens` fields and the per-model minimum cacheable prefix (512 tokens on the 5.5 models, 1 024–4 096 on older ones). Where a fact is unverified it says so. No code edited.

### 1. What the code is today (after R1 and phase 4), and what the switch changes

| After R1 + phase 4 | Change |
|---|---|
| `margin_ai::ResponsesClient` is the only API shape: `stream_parts(instructions, input, &CallOptions, on_delta)` (R1-ai: `CallOptions { route, cache_key, max_output_tokens }`, `prompt_cache_key` sent, constant-first prompts with `prompt_version`), `stream_response` → `StreamEvent` (Responses item types verbatim). | **One seam.** `TextModel` (plain streamed text) and `Provider` (one agent step) are two traits over the same `CallOptions`; the agent's `ModelRequest` carries a `CallOptions` too. `ResponsesClient` and the new `ClaudeClient` implement both. `CallOptions` gains `cache_prefix_len` so the Claude client can put a breakpoint after R1's constant block (D1, D12). |
| `margin_agent::ModelClient::stream(ResponseRequest)`; `turn.rs` builds Responses JSON and consumes `StreamEvent`s by item type; `replay_input` is Responses-only; `ScriptedModel` scripts `StreamEvent`s. | `ModelClient` = `Provider`. The loop builds `ModelRequest`s and consumes `ModelEvent`s; replay is per provider (`replay::openai`, `replay::claude`) over provider-verbatim stored items; `ScriptedModel` scripts `ModelEvent`s. The step/confirmation/Stop/budget machinery and R1's retrieval tool family are untouched. |
| Stored `model_items: Vec<Value>` are Responses items. | Still provider-verbatim JSON, one provider per chat (`Chat.provider`). Claude entries are content blocks with their role; replay regroups them into messages. Nothing stored is rewritten. |
| `AiConfig { base_url, api_key, model }` from Settings + one Keychain key; `Generator::agent_model(ai)`; `AppState::ai_config()`. | `ProviderConfig { provider, base_url, credential, models: RouteModels }`; three Keychain entries; `AppState::model_for(route, provider)` resolves the provider, the model id and its window (D10, D13). |
| Settings: one OpenAI block, `agent_model`, `pipeline.routes: RouteTable` (R1: model ids by route, `None` = the provider's chat model; R1-app's three slots bind to it). | Settings: default provider; OpenAI, ChatGPT and Claude blocks, each with its own three slots (`RouteModels`); `pipeline.routes` stays as the eval-variant override (D9). Effort shared. New chat: provider picker. |
| Traces (R1-trace): every model call is a `trace` row with `route`, `provider`, `model`, `usage`, `cost_usd` from `evals/prices.toml`; `provider` is always `"openai"` today. | `provider` is the chat's or the default; `usage` gains cache writes and web counters; `prices.toml` gets rows per provider; ChatGPT calls carry no `cost_usd` (D14). |
| No web search; the prompt says so. | `web_search` (and on Claude `web_fetch`) as hosted tools; `ToolCall`/`ToolResult` rows, a `WebResults` card, citations on answers. |

### 2. Settled designs

#### D1. The provider boundary (`crates/ai/src/model.rs`, first commit)

```rust
pub enum ProviderKind { OpenAi, ChatGpt, Claude }           // = margin_shared::Provider
pub enum Credential { ApiKey(String), Bearer(String) }        // never Debug-printed
/// The model id each route uses on this provider (Settings' three slots, D9).
pub struct RouteModels { pub chat: String, pub write: String, pub small: String }
impl RouteModels { pub fn get(&self, route: Route) -> &str }
pub struct ProviderConfig { pub provider: ProviderKind, pub base_url: String, pub credential: Credential, pub models: RouteModels }

/// One seam for every model call (R1-ai's shape, extended). Built by the service per call.
pub struct CallOptions {
    pub route: Route,                          // margin_shared::pipeline::Route: Chat | Write | Small
    pub cache_key: String,                     // OpenAI `prompt_cache_key`; part of the trace's query hash
    pub max_output_tokens: Option<u32>,
    /// Byte length of the constant part of `instructions` (R1's `Prompt.constant.len()`); the
    /// Claude client ends its first cache block there. `None`: the whole instructions block.
    pub cache_prefix_len: Option<usize>,
    pub prompt_version: Option<String>,        // for the trace; never sent to a provider
}

/// Plain streamed text: lessons, scenes, the planner, titles, condense summaries, rerank.
pub trait TextModel: Send + Sync {
    fn stream_parts<'a>(&'a self, instructions: &'a str, input: &'a [InputPart], opts: &'a CallOptions,
                        on_delta: Box<dyn FnMut(&str) + Send + 'a>) -> BoxFuture<'a, Result<TextOutput, AiError>>;
}
pub struct TextOutput { pub text: String, pub usage: Usage, pub model: String, pub stop_reason: String }

/// One agent step.
pub trait Provider: Send + Sync + 'static {
    fn kind(&self) -> ProviderKind;
    fn stream(&self, req: ModelRequest) -> BoxFuture<'_, Result<ModelStream, AiError>>;
    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AiError>>;    // D13
}

pub struct ModelRequest {
    pub opts: CallOptions,                    // route = Chat, cache_key = chat id, max_output_tokens
    pub system: String,                       // the system prompt; the provider places it (developer item / `system`)
    pub history: Vec<Value>,                  // the chat's stored items for this provider, in replay order (D4)
    pub tools: Vec<ToolDef>,                  // function tools: name, description, parameters (strict schema)
    pub tool_choice: ToolChoice,              // Auto | None | Allowed(Vec<String>)  (D5 says what Claude does with Allowed)
    pub web: WebTools { search: bool, fetch: bool },
    pub effort: ReasoningEffort,
    pub compaction: Option<u32>,              // threshold in input tokens; None = don't ask the server (D13)
    pub parallel_tool_calls: bool,
}

pub enum ModelEvent {
    Started { index: u32, kind: StartKind },  // Reasoning | Text | ToolCall{call_id,name} | WebSearch{id} | WebFetch{id} | Other
    Delta { index: u32, delta: Delta },        // Text(String) | Reasoning(String) | Arguments(String)
    Done { index: u32, item: ModelItem },
    Completed { usage: Usage, model: String, stop_reason: String, next: Next },  // Next::Finished | Next::Paused (pause_turn: call again with the same history) | Next::ServerToolPending (results arrive in the next call)
    Incomplete { reason: String, usage: Usage },
    Failed { code: Option<String>, message: String },
}
pub struct ModelItem { pub stored: Vec<Value>, pub view: ItemView }   // `stored` = what goes into model_items, verbatim
pub enum ItemView {
    Reasoning { summary: String },
    Text { text: String, citations: Vec<WebCitation> },
    ToolCall { call_id: String, name: String, arguments: String },
    WebSearch { id: String, query: String },                           // the call
    WebSearchResult { id: String, results: Vec<WebResult>, error: Option<String> },
    WebFetch { id: String, url: String },
    WebFetchResult { id: String, url: String, title: Option<String>, error: Option<String> },
    Compaction,
    Other,
}
```

The model a call uses is `config.models.get(opts.route)`; there is no per-request model override any more (the `ModelRequest.model: Option<String>` of the first version goes; a test that wants another model builds another `ProviderConfig`). `Completed.model`/`stop_reason` and `TextOutput.model`/`stop_reason` exist for the trace row (D14): the trace records what the provider answered with, not what was asked for.

`AiError::Http` gains `code: Option<String>` (OpenAI `error.code`; Anthropic `error.type`, or `error.details.error_code` when present). `AiError::is_retryable`: 429 (except the two not-retryable codes in D7), 5xx incl. 529, 504, network. `retry_after` from `retry-after` (seconds, uncapped by the loop) or `retry-after-ms`. `Usage` (shared) gains `cache_write_input_tokens`, `web_search_requests`, `web_fetch_requests` (all `#[serde(default)]`).

`ResponsesClient: Provider` adapts today's `StreamEvent`s: `output_item.added/done` by item type (`reasoning` → Reasoning, `message` → Text with `url_citation` annotations as `WebCitation { url, title, cited_text: None, start, end }`, `function_call` → ToolCall, `web_search_call` → WebSearch at `added` and, at `done`, a `WebSearchResult` with `action.query` and `action.sources` as results, `compaction` → Compaction, else Other); `Completed` → `Next::Finished` with `response.model` and `status`. Request: `model` = `models.get(route)`; `tools` = function tools + `{"type":"web_search","search_context_size":"medium"}` when `web.search`; `include: ["reasoning.encrypted_content", "web_search_call.action.sources"]`; `tool_choice` as today (`allowed_tools` for `Allowed`); `context_management` from `compaction`; `prompt_cache_key` = `opts.cache_key` (R1-ai already sends it on `stream_parts`; `cache_prefix_len` is ignored: OpenAI caches by automatic prefix). Under `Credential::Bearer` (ChatGPT) the client never sends `max_output_tokens`, `temperature`, `top_p`, `user`, `previous_response_id` (unsupported on that route) and treats a `response.failed` whose code is `subscription_sharing_usage_limit_exceeded` / `subscription_sharing_usage_unavailable` as D7 says.

#### D2. The Claude client (`crates/ai/src/messages.rs`)

`POST {base_url}/v1/messages` (default `https://api.anthropic.com`), headers `x-api-key`, `anthropic-version: 2023-06-01`, `anthropic-beta: compact-2026-01-12,thinking-binding-controls-2026-08-01` (the second only while `block_binding` is sent, D4), `accept: text/event-stream`. Body for an agent step:

- `model: models.get(Chat)`, `max_tokens: CLAUDE_MAX_OUTPUT_TOKENS` (32 000; `opts.max_output_tokens` when given), `stream: true`.
- `system: [{"type":"text","text": req.system, "cache_control": {"type":"ephemeral"}}]` (one block: the agent's system prompt is constant for the chat's life already).
- `messages`: rendered by the agent's `replay::claude` (D4); the client only adds `cache_control: ephemeral` to the last block of the last message (moving a marker is not an edit).
- `tools`: each `ToolDef` as `{name, description, input_schema, strict: true}` (the same strict schemas; `additionalProperties:false` + `required` are already there), the last one with `cache_control`; then, when `web.search`, `{"type":"web_search_20260318","name":"web_search","max_uses": WEB_SEARCH_MAX_USES (8)}` and, when `web.fetch`, `{"type":"web_fetch_20260318","name":"web_fetch","max_uses": 8, "citations": {"enabled": true}, "max_content_tokens": 50000}`. Dynamic filtering is the default on these versions (`allowed_callers` defaults to code execution); on a 400 whose message says to set `allowed_callers` the client retries once with `"allowed_callers": ["direct"]` and remembers that per model for the process (models before 4.6).
- `tool_choice`: `{"type":"auto"}` or `{"type":"none"}` (forced `any`/`tool` are 400 on 5.5 models and never sent). `Allowed(names)`: D5.
- `thinking: {"type":"adaptive","display":"summarized","block_binding":{"prefix_mismatch_behavior":"drop_block"}}`; `output_config: {"effort": effort.as_str()}` (the five values are Claude's five). Never `budget_tokens`, never `disabled`, no sampling parameters.
- `context_management: {"edits":[{"type":"compact_20260112","trigger":{"type":"input_tokens","value": threshold}}]}` when `compaction` is set.

Three breakpoints on an agent step (system, last tool, last message), under the limit of four.

Stream → `ModelEvent`: `message_start` (usage: `input_tokens`, `cache_read_input_tokens` → cached, `cache_creation_input_tokens` → cache_write; `message.model` kept for `Completed.model`); `content_block_start` by block type: `thinking`/`redacted_thinking` → Reasoning, `text` → Text (consecutive `text` blocks of one response are **one** neutral Text item: the adapter maps their block indexes to one logical index and concatenates; `stored` keeps each block verbatim), `tool_use` → ToolCall, `server_tool_use` → WebSearch/WebFetch by `name`, `web_search_tool_result`/`web_fetch_tool_result` (complete in their `content_block_start`) → `Done` with the matching result view (`content` a list → results `{url, title, page_age}`; an object → `error_code` as the error), `compaction` → Compaction; `content_block_delta`: `text_delta` → Delta::Text, `thinking_delta` → Delta::Reasoning, `input_json_delta` → Delta::Arguments, `signature_delta` kept into the stored block, `citations_delta` → collected into the Text view's citations (`web_search_result_location` → `WebCitation { url, title, cited_text, start/end = offsets into the merged text }`; `char_location` from web fetch likewise); `message_delta`: cumulative usage (`output_tokens`, `server_tool_use.web_search_requests/web_fetch_requests`, `output_tokens_details.thinking_tokens` → reasoning_tokens, `iterations` summed into input/output when present) and `stop_reason` (kept verbatim for the trace): `end_turn`/`stop_sequence` → Finished, `tool_use` with every `server_tool_use` answered → Finished (the loop runs the client tools), `tool_use` with a `server_tool_use` lacking its result → `ServerToolPending`, `pause_turn` → Paused, `max_tokens`/`model_context_window_exceeded` → `Incomplete`, `refusal` → Text as it came plus `Incomplete { reason: "refusal" }`; `message_stop` ends; `ping` ignored; `event: error` → `Failed { code: error.type }`. Non-2xx before the stream → `AiError::Http` with `code` = `error.type` (`rate_limit_error`, `overloaded_error`, `authentication_error`, …) and `details.error_code` when present (`enforced_spend_limit_reached`).

`TextModel` for Claude (D12 has the caching detail): `model: models.get(opts.route)`; `system` = the instructions as **two** text blocks when `opts.cache_prefix_len` is `Some(n)` — `instructions[..n]` with `cache_control: ephemeral`, then `instructions[n..]` without — else one block with the marker; one user message with `text` and `image` (`{"type":"image","source":{"type":"base64","media_type","data"}}`) blocks in order, `max_tokens: 64 000` unless given, thinking adaptive (omitted = adaptive), no effort override. `list_models()`: `GET /v1/models` → `data[].{id, display_name, max_input_tokens, max_tokens}` → `ModelInfo` (D13).

#### D3. The loop on neutral events (`crates/agent/src/turn.rs`)

`consume` keys on `ItemView` instead of item types; everything else in the contract's §4.2–4.6 stays, including R1's retrieval tool family (`search`, `read_passage`, `document_outline`, `view_picture`), which is just more `TypedTool`s to the loop. Additions:

- `WebSearch`/`WebFetch` at `Started`: `add_item(InProgress, ToolCall { call_id: id, name: "web_search"|"web_fetch", arguments: "", label: "Searching the web"/"Fetching a page", device: None })`; `Delta::Arguments` streams into it; at `Done` the label becomes `Searching the web for “<query>”` / `Fetching <host>`. At the matching result view: `ToolResult { ok: error.is_none(), summary: "N results"|"Fetched “title”"|<error_code in words> }` with `full_output` = the results as text (url, title, page age — never `encrypted_content`), the `ToolCall` → Done, then `Card::WebResults(WebResultsCard { query, results })` for a search. These calls are not counted against the tool output budget and are not in `unanswered_calls` (they are the provider's, not Margin's). A result that arrives in the next step (`ServerToolPending`) finishes the still-open call by id; a Stop or a turn end with one still open marks it `Cancelled` with `ToolResult { summary: "Cancelled" }` and no model item (D4 drops the block on replay).
- `Text` with citations: `AssistantText { text, citations }` (R1 already added the shared field for source citations; the web variant is one more `WebCitation` shape in it).
- `Completed { next: Paused }`: call again at once with the same request (the stored blocks are already in the history; the step counts toward `MAX_STEPS`); `ServerToolPending` + client calls: run them, send their outputs, the next call returns the server result first.
- `Incomplete { reason: "refusal" }`: Error "The model declined to answer (<category>)", `retryable: false`, code `bad_request`.
- Compaction per provider: `compaction = Some(threshold)` on both, the threshold from D13; a rejection (OpenAI: unknown `context_management`; Claude: 400 naming the beta header) flips `server_compaction` as today and is remembered per `(provider, base_url)` in `Turns` (phase 5's A8 lands here). The local condense keeps today's algorithm; on Claude the condensed tail is rendered with its `thinking`/`redacted_thinking` blocks removed (the summary replaces the prefix those blocks were bound to; `drop_block` covers the rest) — see D4.
- `TurnDeps.small: &dyn TextModel` (replaces the first version's `summary_model`; the rag doc's "`TurnDeps.summary_model` becomes `model_for(Small)`"): the default provider's text model, called with `CallOptions { route: Small, cache_key: "chat:{id}", .. }` for the condense summary. The service builds it from `model_for(Small, None)` (D10).
- `tool_choice` on Claude: D5.
- `TurnDeps.on_call: &dyn Fn(ModelCallRecord)` (R1-trace's hook in `turns.rs` stays where it is: the loop reports `Completed`/`Incomplete` usage, `model`, `stop_reason` and `step` through the existing sink path, and the service writes the trace row; D14).

#### D4. Stored history stays provider-specific; replay per provider (`crates/agent/src/replay/{openai,claude}.rs`)

`Chat.provider: Provider` (shared, `#[serde(default)] = OpenAi`, set at creation from `NewChat.provider` else Settings' default, never changed). `StoredChatItem.model_items` hold that provider's wire items verbatim.

**OpenAI**: unchanged (`replay::openai::replay_input` = today's `replay_input`, plus `web_search_call` items replayed verbatim like any output item — the spike confirms the API accepts them as input under `store: false`).

**Claude**: a stored entry is `{"role":"assistant"|"user","block":{…one content block verbatim…}}`. What stores which:

| Item | model_items |
|---|---|
| `UserMessage` | `user` blocks: `text` (the context part, as D6 words it), `text` (the owner's words), `image`s if any |
| `Reasoning` | `assistant` `thinking` or `redacted_thinking` block, verbatim incl. `signature` |
| `AssistantText` | one `assistant` `text` block per block the model sent (with `citations`, incl. `encrypted_index`) |
| `ToolCall` (Margin tool) | `assistant` `tool_use` |
| `ToolResult` | `user` `tool_result { tool_use_id, content: output, is_error: !ok }` (R1's `ToolOutput.images` become `image` blocks inside the `tool_result` content list) |
| `ToolCall`/`ToolResult` for web search/fetch | `assistant` `server_tool_use`; `assistant` `web_search_tool_result`/`web_fetch_tool_result` verbatim (`encrypted_content` inside) |
| `Action` | `user` `text` block `"Margin: <text>"` (D6) — or the `assistant` `compaction` block |
| `Card`, `Error`, cancelled partial output | none |

`replay::claude::messages(items) -> Vec<Value>`: items in `seq` order from the latest `compaction` block (or the latest condensed-history note); consecutive blocks of one role become one message (adjacent same-role messages are merged — the API would otherwise reject or merge them); the safety net for an unanswered `tool_use` is a `tool_result { is_error: true, content: {"error":"cancelled by the user"} }` inserted as the next user message; an `Action` note stored between a step's `tool_use`s and their `tool_result`s is rendered after the results (the user message that holds the results then holds the note too — one message, results first; the API requires that a tool-result message holds only results when a *server* tool is pending, so when the preceding assistant message has an unresolved `server_tool_use` the note is deferred to the next user message instead); a trailing `server_tool_use` with no result block is left out; a trailing `thinking` block with no later `text`/`tool_use` in its message is left out; a trailing `text`-only assistant message is fine. The rendering is deterministic: the same stored items always render the same prefix, which is what preserved thinking checks (the prefix check compares requests as sent, and Margin never edits what it sent — deferral and the safety net are applied identically every time). `block_binding.prefix_mismatch_behavior: "drop_block"` is still always sent: a tool list change at a release, a model switch, or a local condense would otherwise 400; the dropped blocks are logged from `message_start.input_transformations`.

`unanswered_calls` is per provider too (`tool_use` ids without `tool_result`). `cancelled_call_output(provider, call_id)` renders the right shape.

#### D5. Tool gating without `allowed_tools` (Claude)

Claude has no `allowed_tools`, and changing `tools` between steps is a history edit. So on Claude `tools` is constant for the chat's life (the whole registry + the web tools the provider offers) and `ToolChoice::Allowed` is rendered as `auto`. The gating that `allowed_tools` did on OpenAI already exists as tool behaviour: folder tools answer "No folder is attached to this chat…" (`device::workspace`), app-control answers the offline text, and the message context says which folders and devices are online. `ToolChoice::None` (last step, budget used) is sent as `{"type":"none"}`. The registry order is fixed; a release that changes it changes the cache prefix once (same as OpenAI).

#### D6. The prompt on both providers (`prompt.rs`)

- The "There is no web search" paragraph becomes: "You can search the web (`web_search`) and, where offered, read pages (`web_fetch`) when the answer needs current or outside information; say when you did, and prefer the library and attached material for what it covers." The owner's words still rule ("don't search" means don't).
- Actions on Claude are user-role notes starting with `Margin: ` (OpenAI keeps developer messages). The prompt adds one sentence: "A user-role note that begins with “Margin:” is the app's own record of something that happened (a stop, a course created, a restart), not the owner's words." The context part already states that quoted names are data.
- The system prompt is the `system` field on Claude (cached) and the leading developer item on OpenAI, as today. The prompt's constant part is byte-identical on both providers; `prompt_version` of the agent prompt (`"agent-v2"` at this release) goes into the trace like the lesson's (D14).

#### D7. Errors, retries, rate limits (`error_code` gains four codes)

| Situation | OpenAI / ChatGPT | Claude | Code | Retry |
|---|---|---|---|---|
| Rate limited | 429 (+`retry-after`/`retry-after-ms`), stream `error` `rate_limit_exceeded` | 429 `rate_limit_error` with `retry-after` seconds (uncapped, as the owner decided) | `rate_limited` | yes |
| Overloaded / server | 5xx | 529 `overloaded_error` (HTTP or stream `error`), 500 `api_error`, 504 `timeout_error` | `server_error` | yes |
| Credentials | 401, 403 | 401 `authentication_error`, 403 `permission_error`, 402 `billing_error` | **`auth_failed`** (resolves phase 3's open question: not `no_api_key`) | no; the app offers Open Settings → the provider |
| Spend cap | — | 429 with `details.error_code: enforced_spend_limit_reached` (no `retry-after`), or 400 "You have reached your specified API usage limits" | **`spend_limit_reached`** | no |
| ChatGPT plan share used up | 429 `subscription_sharing_usage_limit_exceeded`, or mid-stream `response.failed` with that code or `subscription_sharing_usage_unavailable` | — | **`plan_limit_reached`** (the app: "Margin's share of your ChatGPT plan is used up. Raise it in ChatGPT → Settings → Usage, or start a new chat on another provider.") | no (the docs say not to infer a reset time) |
| ChatGPT plan not eligible | 403 `subscription_sharing_user_not_eligible` | — | **`plan_ineligible`** | no |
| ChatGPT token invalid | 401 `subscription_sharing_invalid_user` | — | the service refreshes once and retries the call; a terminal refresh error signs the provider out (`auth_failed`) | once |
| Context too long | as today | `model_context_window_exceeded` stop, or 400 naming the context window | `context_too_long` | condense once |
| Cut off | `incomplete` | `max_tokens` | `incomplete` | Continue |
| Refusal | refusal content | `stop_reason: refusal` | `bad_request` | no |

Waiting banner texts come from the code as today (`wait_reason`). `RetryPolicy` unchanged. Every failed call is still traced (`ok: false`, `error_code`, D14).

#### D8. What a web search looks like in the chat

- Rows: a `ToolCall` named `web_search`/`web_fetch` (icon `globe`, label with the query/host, "on OpenAI"/"on Claude" is **not** shown — the chat header shows the provider), its `ToolResult` summary ("6 results", "Fetched “Title”", "Search limit reached (max_uses_exceeded)"), on-demand output = the results list.
- `Card::WebResults { query, results: [{ url, title, page_age: Option<String> }] }` after the result: a list of links (host + title, page age), opening in the system browser (`open_external`), rendered like `PassagesView`.
- Citations: `AssistantText.citations` holds `WebCitation { url, title, cited_text: Option<String>, start: Option<u32>, end: Option<u32> }` beside R1's source citations — the answer renders a numbered "Sources" list under the text (title → url, cited text as a tooltip), and, where `start`/`end` are present, superscript markers at those offsets (the existing `[^n]` marker styling). Claude's terms require the citations to be shown; OpenAI's `url_citation` annotations give the same. The usage footer adds "· N web searches" when `web_search_requests > 0` (Claude: $10 per 1 000; the tooltip says so).
- `tool_icon("web_search" | "web_fetch") = "globe"`.

#### D9. Settings and credentials (service `providers.rs`, `settings.rs`, `secrets.rs`; app `ui/settings.rs`)

Keychain (service Mac, never on devices): `openai-api-key` (exists), `anthropic-api-key`, `chatgpt-tokens` (JSON: `client_id`, `ext_agent_host_id`, `access_token`, `refresh_token`, `expires_at`, `scopes`, `sub`, `email`).

**Three model slots per provider** (owner). Shared `Settings` gains (additive): `default_provider: Provider`; `openai_models: RouteModels`, `chatgpt_models: RouteModels`, `claude_models: RouteModels` (`RouteModels { chat, write, small }`, model ids); `has_claude_api_key`; `chatgpt: Option<ChatGptAccount { email, signed_in_at, expires_at }>`. Defaults: `claude_models = { chat: "claude-opus-5-5", write: "claude-opus-5-5", small: "claude-haiku-5-5" }` (owner: Opus is the default model, Haiku the small one; §8 asks whether writing should default to Sonnet 5.5); `openai_models = { chat: agent_model ?? openai_model, write: openai_model, small: "gpt-6-luna" }` (the rag doc's Small default; **the id is unverified** — the orchestrator checks it against the live model list when the first commit lands, and §8 lists it); `chatgpt_models = { chat/write/small: "gpt-6.1-sol" }` (the SIWC docs' example model; the owner picks from the account's list). The existing `openai_model`/`agent_model`/`has_api_key` are the OpenAI provider's; `agent_model` is read once at migration into `openai_models.chat` and then kept only as a deprecated `#[serde(default)]` field (no field is removed from the wire). `SettingsPatch` gains the same plus `anthropic_api_key: Option<String>` (`""` deletes). `default_provider = OpenAi`.

`pipeline.routes: RouteTable` (R1, shared) is **not** the product's slot store: it stays what R1 made it, the eval-variant override (`evals/variants/*.toml` may pin a model per route), `None` in the shipped Settings. Resolution: D10.

Routes (`routes.rs`): `GET /api/providers/{provider}/models` → `Vec<ModelInfo { id, display_name, context_window: Option<u32>, max_output_tokens: Option<u32> }>` (OpenAI key: `GET {base}/models` `data[].id`, windows from D13's table; ChatGPT: `GET https://api.openai.com/v1/models` with the bearer, `models[]` where `visibility == "list"`, `slug`/`display_name`; Claude: `GET /v1/models` with `max_input_tokens`/`max_tokens`); 409 `not_configured` when the provider has no credential. `POST /api/providers/chatgpt/start` → `{ authorize_url, state }`; `GET /api/providers/chatgpt` → `{ status: SignedOut | Waiting { authorize_url } | SignedIn { account } | Failed { message } }`; `DELETE /api/providers/chatgpt` signs out (deletes the tokens; no remote revocation endpoint is documented).

The SIWC flow (service-side, the owner completes it in a browser **on the service Mac** because the OAuth callback is a loopback URL `http://127.0.0.1:<port>/auth/callback` on the machine that runs the flow; the app on another device shows the link to open there): `ext_agent_host_id = "urn:uuid:<v4>"` generated once and kept with the tokens; first sign-in `client_id=dynamic_agent_client` + `agent_name_hint=Margin`, later sign-ins the issued `client_id` (+ `id_token_hint`, `login_hint`); `response_type=code`, `scope=openid profile email offline_access resource.invoke chatgpt.tokens.use.direct`, `resource=https://api.openai.com/v1`, fresh `state`/`nonce`, PKCE `S256`; authorization at `https://auth.openai.com/api/accounts/authorize`; the service listens on `127.0.0.1` (port from the request, reused: the path and host must not change) for the callback with `code`, `state` and, on first registration, `client_id`; token exchange `POST https://auth.openai.com/api/accounts/oauth/token` form-encoded `authorization_code` grant with `client_id`, `code`, `code_verifier`, `redirect_uri`, `resource` → `access_token` (1 h), `refresh_token` (30 d, rotates on every refresh), `id_token` (validated: signature against OpenAI's JWKS, issuer, audience = client id, expiry, nonce; `sub` kept), granted `scopes` must include `chatgpt.tokens.use.direct` else `Failed`. Refresh: on demand before a call when < 5 min remain, under a mutex, `grant_type=refresh_token`; `invalid_grant`/`token_expired`/`refresh_token_*` → tokens cleared, `SettingsChanged`, chats on ChatGPT fail with `auth_failed`. The listener closes after one callback or 10 min. `SettingsChanged` carries the account.

App Settings → AI pane (`AiSection` becomes three provider sections plus a header; R1-app's three slots, built against `pipeline.routes` before this phase, move into each provider block):
- **Default provider**: a segmented control (OpenAI · ChatGPT · Claude); a provider without a credential is selectable but shows "Set it up below"; saving it is `SettingsPatch.default_provider`.
- **OpenAI**: base URL, API key (as today), then the three slots "Model for chats", "for writing", "for small tasks" (each a combobox: the list from `/models` when a key is set, else free text; the window from `ModelInfo` shown as "1M context" beside the id when known).
- **ChatGPT**: "Continue with ChatGPT" → `POST start`; on the Mac app `open_external(authorize_url)`; elsewhere the link is shown with "Open this link on the Mac that runs Margin's service" and a Copy button; the pane polls `GET /api/providers/chatgpt` every 2 s while `Waiting`; `SignedIn` shows the account email, "Signed in <date>", "Sign out…" (confirm), the three slots from the account's list; `Failed` shows the message with "Try again". Help text: "Margin uses your ChatGPT plan for what you ask it here. You can cap Margin's share in ChatGPT → Settings → Usage."
- **Claude**: API key (same key UI as OpenAI's: stored, replace, remove), the three slots from `/v1/models` (defaults listed first: Opus 5.5, Sonnet 5.5, Haiku 5.5).
- **Chats** section keeps Reasoning (shared across providers; the Claude client maps it to `effort`).
- **Pipeline** section (R1-app) is unchanged; its route override, when a variant sets one, is shown read-only under the slots as "Overridden by the pipeline variant: <id>".
- "Open Settings" from an `auth_failed`/`plan_*`/`spend_limit_reached` error lands on this pane.

New chat (`ui/chat.rs`, Ask screen): a provider segmented control above the composer, preset to the default; a provider without a credential is disabled with a tooltip "Set up in Settings → AI". `NewChat.provider` is sent; the chat header (`TopBar`) shows a small provider badge; the sidebar row doesn't. Settings' default change never moves existing chats.

#### D10. Service wiring (`engines.rs`, `state.rs`, `turns.rs`, `chats.rs`, `ask.rs`, `video.rs` call sites)

```rust
pub struct ModelChoice { pub config: ProviderConfig, pub model: String, pub window: ModelWindow }  // window: D13
impl AppState {
    /// The provider's config and the model its slot names for `route`; `pipeline.routes`
    /// overrides the slot when the id belongs to that provider's list (else logged and ignored).
    /// `provider: None` = Settings' default provider.
    pub async fn model_for(&self, route: Route, provider: Option<Provider>) -> AppResult<ModelChoice>;
    pub async fn provider_config(&self, provider: Provider) -> AppResult<ProviderConfig>;
}
pub trait Generator {
    fn write_lesson<'a>(&'a self, m: &'a ModelChoice, ctx: &'a LessonContext, on_delta: …) -> BoxFuture<'a, Result<TextOutput, AiError>>;
    fn write_scene<'a>(&'a self, m: &'a ModelChoice, req: &'a SceneRequest) -> BoxFuture<'a, Result<TextOutput, AiError>>;
    fn plan_course<'a>(&'a self, m: &'a ModelChoice, input: &'a PlanInput) -> BoxFuture<'a, Result<TextOutput, AiError>>;  // R1's planner, now on the seam
    fn agent_model(&self, config: &ProviderConfig) -> Arc<dyn Provider>;
    fn text_model(&self, config: &ProviderConfig) -> Arc<dyn TextModel>;
}
```

Errors from `provider_config`: `no_api_key` naming the provider, `no_model` (an empty slot), and for ChatGPT `auth_failed` when signed out. `ai_config()`/`AiConfig` are deleted. Call sites: `ask.rs` lessons and the planner use `model_for(Write, None)` with `CallOptions { route: Write, cache_key: "lesson:{prompt_version}" | "plan:{prompt_version}", cache_prefix_len: Some(prompt.constant.len()), prompt_version }`; `video.rs` scenes the same with `scene:{prompt_version}`; `turns.rs` runs the chat on `model_for(Chat, Some(chat.provider))`, builds `TurnDeps.small` from `model_for(Small, None)`, and the title call on `model_for(Small, None)` with `max_output_tokens` only off ChatGPT (unsupported there; the instructions keep it short). The lesson `Budget` resolves against `ModelChoice.window` (D13). `Chat.usage` keeps adding (the web counters included). Recovery and Stop are unchanged.

#### D11. The SIWC spike — before area P-ai-openai's ChatGPT part is built

A throwaway binary in a worktree that is never merged (`examples/siwc_spike.rs` in `crates/ai`, removed afterwards), run by the owner with their own plan, writing nothing to Margin's data folder and touching no running service. It verifies, in order: (1) registration with `dynamic_agent_client` + loopback on this Mac and the token exchange; (2) a Responses call with the bearer and `store: false`, `stream: true`, function tools with `strict: true`, `include: ["reasoning.encrypted_content"]`, a replayed `reasoning` item, `tool_choice: allowed_tools`, `context_management` compaction — each accepted or the exact 400 recorded; (3) `web_search` allowed under the plan, and a `web_search_call` item accepted when replayed as input; (4) a refresh with rotation; (5) the per-app cap (set low in ChatGPT → Settings → Usage) producing `subscription_sharing_usage_limit_exceeded`. Outcomes: (1) or (2) fail → **the ChatGPT provider is dropped**: no `Provider::ChatGpt` variant work in P-service/P-app (the enum keeps the variant, unselectable, so stored data never needs it), and the brief's other parts go ahead unchanged; (3) fails → ChatGPT stays without web search (`WebTools { search: false }` for that credential; the prompt line is per request); a feature in (2) fails → that feature is turned off for `Credential::Bearer` the way `max_output_tokens` already is. The spike's log (no tokens) goes into `docs/agent-siwc-spike.md`.

#### D12. Prompt caching on R1's constant-first prompts

R1-ai lays every job prompt out as `Prompt { version, constant, variable, cache_key }` and enforces `instructions = constant ++ variable` with a test. The providers phase makes both clients use that boundary:

- **OpenAI**: automatic prefix caching (≥ 1 024 identical leading tokens) plus `prompt_cache_key = opts.cache_key` — already what R1-ai sends; nothing more. `cache_prefix_len` is ignored.
- **Claude**: `cache_control: ephemeral` goes on the end of the constant block (`system[0]`, D2), never after the variable part (passages, level, minutes, the quiz spec change per call and would only write cache entries nobody reads). The minimum cacheable prefix on the 5.5 models is 512 tokens, 1 024–4 096 on older ones: the lesson constant (~1.3k tokens with `LESSON_SOURCES`) and the scene cheat sheet (~4k) clear it on every current model; the planner's does; the title and condense instructions are below it and simply don't cache (no error, `cache_creation_input_tokens: 0`), which is fine. An agent step has three breakpoints (system, last tool, last message) and a `TextModel` call one; the API allows four.
- **Invariant, enforced by tests on both fakes**: for one `prompt_version`, the bytes before the breakpoint are identical across calls (the fake Responses server already records bodies for R1's prefix-identity gate; the fake Messages server records them too, and the gate in `crates/evals/tests/gates.rs` asserts `system[0].text` is byte-identical and carries the marker, and that `system[1]` holds the variable part). `prompt_version` bumps when `constant` changes.
- Cache reads and writes are visible per call in the trace (`usage.cached`, `usage.cache_write`), so "cache-hit share by route and provider" is one rollup query (D14); the §2.9 target of ≥ 60 % cached input from step 2 on applies to both providers.

#### D13. Model windows and the budget (no fixed context budget)

The owner's decision: budgets scale with the model's window. The provider is the source of the window:

- `ModelInfo { id, display_name, context_window: Option<u32>, max_output_tokens: Option<u32> }` (shared). Claude fills both from `GET /v1/models` (`max_input_tokens`, `max_tokens`). OpenAI's `/models` carries no window, so `crates/ai/src/windows.rs` holds a small table by id prefix (`gpt-6` family → the values checked when the first commit lands; **unverified until then**), with `None` for an unknown id; ChatGPT uses the same table.
- `ModelWindow { input: u32, output: u32 }` on `ModelChoice` (D10), resolved by `AppState` from a per-process cache of each provider's `list_models` (filled lazily at first use and refreshed on `SettingsChanged`; a model missing from the list, or a provider that answers no window, gets `MODEL_WINDOW_FALLBACK` = 200 000 input / 32 000 output and a log line). The window is never guessed silently: the Settings pane shows "window unknown, using 200k" beside such a model.
- R1's packer gets `PackBudget.tokens = floor(window.input × Budget.share_of_window) − reserve_output` with `reserve_output = min(window.output, CLAUDE_MAX_OUTPUT_TOKENS or the OpenAI cap)`, pictures counted at the provider's cost (≈ 1 100 tokens at 1 536 px on OpenAI, `(w×h)/750` on Claude — the packer takes the provider from `ModelChoice`).
- The agent's server-compaction threshold becomes `min(COMPACT_THRESHOLD_TOKENS, 0.6 × window.input)` so a small-window model is compacted before it overflows; on a 1M-window model today's constant stands.
- Nothing else in the pipeline reads a literal token cap (R1's review lens), and the trace records `packed_tokens` against the window so the eval can plot cost per budget setting as §2.9 asks.

#### D14. Traces per provider and `evals/prices.toml`

R1-trace writes a `trace` row per model call (`kind: model_call`) with `route`, `provider`, `model`, `usage: TraceUsage`, `cost_usd`, `stop_reason`, `step`, `prompt_version`, `pipeline_hash`. This phase fills them per provider; no new table, no prompt text:

| Field | OpenAI / ChatGPT | Claude |
|---|---|---|
| `provider` | `"openai"` / `"chatgpt"` (`Provider::as_str`, snake_case) | `"claude"` |
| `model` | `response.model` | `message_start.message.model` |
| `usage.input/cached/output/reasoning` | `usage.input_tokens`, `input_tokens_details.cached_tokens`, `output_tokens`, `output_tokens_details.reasoning_tokens` | `input_tokens` (+ `iterations`), `cache_read_input_tokens`, `output_tokens`, `output_tokens_details.thinking_tokens` |
| `usage.cache_write` | 0 (no explicit writes) | `cache_creation_input_tokens` |
| `usage.web_searches` / `usage.web_fetches` (**new**, `#[serde(default)]`) | `web_search_call` items in the response / 0 | `server_tool_use.web_search_requests` / `web_fetch_requests` |
| `stop_reason` | `status` (`completed`, `incomplete`) or the `incomplete_details.reason` | `stop_reason` verbatim |
| `cost_usd` | priced from `prices.toml` by model id; **ChatGPT: `None`** (plan-billed, no dollar price; the rollup still counts its tokens under `provider = "chatgpt"`) | priced: `input × input + cached × cached_input + cache_write × cache_write + output × output` per million, plus `web_searches × web_search / 1 000` |

`evals/prices.toml` (first commit): the OpenAI rows for the models the slots default to (the orchestrator checks the prices on the day; a model missing from the table records `cost_usd: None`, never a guess — the existing rule); the Claude rows gain `web_search = 10.0` (USD per 1 000 searches) and Haiku 5.5 its long-context tier as `[models.claude-haiku-5-5.long_context] over = 100000, input = 0.50, output = 2.50` (`ModelPrice` in `crates/evals` and `crates/service/src/trace.rs` gain `#[serde(default)] web_search: f64` and `long_context: Option<LongContextPrice>`; `cost_usd` applies the tier when `usage.input > over`). The `provider` enum in `crates/evals` gains `Chatgpt` so a report can group ChatGPT models apart; `prices.checked` is bumped. `TraceUsage` gains `web_fetches` (additive). `trace_hourly`/`trace_daily` already group by `provider` and `model`, so "cost today by provider", "cache-hit share per provider" and "web searches this week" are one `SELECT` each; `margin-eval report` adds a per-provider column to its cost table.

### 3. First commit (orchestrator lands on `claude/agent-build` after R1 and phase 4 are merged, before branching)

`crates/shared`:
1. `Provider { OpenAi, ChatGpt, Claude }` (`rename_all = "snake_case"`, `Default = OpenAi`) with `label()` and `as_str()`; `Chat.provider` (`#[serde(default)]`); `NewChat.provider: Option<Provider>`; `RouteModels`.
2. `ChatItemKind::AssistantText.citations` gains the `WebCitation` variant beside R1's source citation; `Card::WebResults(WebResultsCard { query, results: Vec<WebResult { url, title, page_age }> })`; `error_code::{AUTH_FAILED, SPEND_LIMIT_REACHED, PLAN_LIMIT_REACHED, PLAN_INELIGIBLE}`; `Usage` three new fields; `context::TraceUsage.web_fetches`; `ModelInfo` with the two window fields.
3. `Settings`/`SettingsPatch` additions (D9: `default_provider`, the three `RouteModels`, `has_claude_api_key`, `chatgpt`), `ChatGptAccount`, `ChatGptStatus`; `routes::{providers_models(provider), PROVIDERS_CHATGPT, PROVIDERS_CHATGPT_START}` + table rows; `Event::SettingsChanged` unchanged (carries the new fields).
4. Tests: round trips; an `AssistantText` without `citations` parses; a `Chat` without `provider` is OpenAI; a `Settings` without the slots gets the defaults, with `agent_model` migrated into `openai_models.chat`.

`crates/ai`: `src/model.rs` with everything in D1 (`CallOptions` moves here from R1-ai's `lib.rs` and gains `cache_prefix_len`/`prompt_version`; `RouteModels`, `ProviderConfig`, both traits, `WebTools`, `ToolDef`, `ToolChoice`, `ModelEvent`, `TextOutput`…), `AiError::Http.code`, `pub use`; `src/windows.rs` (D13's table); `impl Provider for ResponsesClient` and the new `TextModel::stream_parts` signature on `ResponsesClient` as `unimplemented!("providers, P-ai-openai")` bodies — R1-ai's `stream_parts(instructions, input, &CallOptions, on_delta)` is renamed to the trait method and kept compiling (`TextOutput` wraps its `String`); `src/messages.rs` with `pub struct ClaudeClient` + `new(base_url, api_key, models)` and both impls `unimplemented!("providers, P-ai-claude")`; `Cargo.toml`: `base64` (already in the workspace lock; needed for Claude image blocks and the SIWC code verifier), `sha2` (PKCE), nothing else. R1-ai's prompt modules (`prompts/*.rs`, `prompts/evals.rs`) are untouched.

`crates/agent`: `ModelClient` = re-export of `margin_ai::Provider` (the old trait removed); `TurnDeps.small`; `TurnInput.provider`; `replay` becomes a module directory with `openai.rs` (today's code) and `claude.rs` whose functions are `unimplemented!("providers, P-agent")`; `ScriptedModel` keeps compiling by scripting `ModelEvent`s (a mechanical move: the existing `Scripted::Events(Vec<StreamEvent>)` becomes `Vec<ModelEvent>`; the test helpers that built Responses items stay and gain `ModelEvent` constructors). Phase 4's 4-agent tools and R1's picture budget are untouched. The crate's tests may be red after this commit; P-agent makes them green.

`crates/service`: `ProviderConfig`/`ModelChoice` replace `AiConfig` in `engines.rs` (the `Generator` signatures of D10, incl. `plan_course` on the seam); `state::model_for`/`provider_config` stubs returning today's OpenAI config for `Provider::OpenAi` and `unimplemented!("providers, P-service")` otherwise, `ai_config()` deleted and its call sites in `ask.rs`, `video.rs`, `turns.rs` moved to `model_for` (mechanical, no behaviour change); `providers.rs` with the three route handlers as `unimplemented!`; `server.rs` routes wired; `secrets.rs` constants `ANTHROPIC_API_KEY`, `CHATGPT_TOKENS`; `trace.rs`: `ModelPrice` fields of D14 and `cost_usd`'s ChatGPT rule (`None`). R1's `pipeline.rs`, `retrieval.rs`, `ingest.rs`, `context_api.rs` and phase 4's `folders.rs` are untouched.

`crates/evals` + `evals/`: `prices.toml` rows and fields of D14; `Provider::Chatgpt`; the `gates_and_prices_parse` test extended to the new fields. `cargo test --workspace` must be green after this commit except `crates/agent` (above).

`crates/app`: nothing (P-app owns its files; `fake.rs`'s `Settings` sample gains the new fields with defaults — P-app).

Contract (`docs/agent-contract.md`): §1 ownership (the ai crate has two owners by file), §2 a "Providers" row, §3 data, §4.2 the request per provider, §4.5 the error table (D7), §4.11 replay per provider (D4), §5 web tools, §6 unchanged, §7 `CLAUDE_MAX_OUTPUT_TOKENS` 32 000, `WEB_SEARCH_MAX_USES` 8, `WEB_FETCH_MAX_USES` 8, `WEB_FETCH_MAX_CONTENT_TOKENS` 50 000, `MODEL_WINDOW_FALLBACK` 200 000 / 32 000, §8 tests (section 5 below). Architecture §3: a row for the provider decision. `docs/agent-rag-architecture.md` §2.6: a pointer here (the routing table's defaults are now D9's).

### 4. Areas (worktree each; files exclusive; all branch from the first commit)

Ownership is re-cut against what R1 and phase 4 leave behind. Every file below is free by then: R1's areas (`r1-*`) and phase 4's (`4-*`) have merged, so a file that was R1-ai's or 4-S's is this phase's to own.

| Area | Worktree | Builds | Owns (exclusively) |
|---|---|---|---|
| **P-ai-openai** | `agent/prov-openai` | `ResponsesClient: Provider + TextModel` (D1): neutral request → body (web_search tool, includes, bearer rules, model by route), `StreamEvent` → `ModelEvent` (web_search_call, url_citation, `response.model`/status), `AiError.code`, SIWC failure codes, `list_models` for key and bearer with windows from `windows.rs`; the spike example (D11). Keeps R1-ai's prefix-identity test green. | `crates/ai/src/{lib,responses,windows}.rs`, `crates/ai/tests/fake_responses.rs`, `crates/ai/examples/siwc_spike.rs` |
| **P-ai-claude** | `agent/prov-claude` | `ClaudeClient` (D2, D12): request rendering, headers/betas, the split `system` with the breakpoint at `cache_prefix_len`, SSE → `ModelEvent` with text-block merging and citations, server tools, pause/pending, usage incl. cache writes and web counters, `model`/`stop_reason`, errors and `retry-after`, `allowed_callers` fallback, `TextModel` with image blocks, `list_models` with windows; a fake Messages server in tests that records bodies (for the gate) | `crates/ai/src/messages.rs`, `crates/ai/tests/fake_messages.rs` |
| **P-agent** | `agent/prov-agent` | The loop on `ModelEvent` (D3), `replay::claude` (D4) and the per-provider `unanswered_calls`/`cancelled_call_output`, D5 gating, D6 prompt (+ `agent-v2` version), web rows/cards/citations, pause/pending continuation, per-provider compaction memory hook, the window-scaled compaction threshold (D13), `ScriptedModel` neutral + all tests green (incl. phase 4's folder-tool tests and R1's picture budget) | `crates/agent/src/{turn,replay/*,prompt,scripted,sink}.rs`, `crates/agent/tests/turn.rs` |
| **P-service** | `agent/prov-service` | D9's secrets/routes/SIWC flow (`providers.rs`), `provider_config`/`model_for` with the window cache (D10, D13), `Generator` impls per provider, chat provider at creation (`chats.rs`), `turns.rs` (provider per chat, `TurnDeps.small`, title on `Small`, compaction memory per provider/base URL, the trace row's provider fields of D14 on R1-trace's spans), `ask.rs`/`video.rs` call sites on `model_for(Write)` with `cache_prefix_len` and the window-resolved `PackBudget`, `trace::cost_usd` per D14, settings patch fields, tests with a fake auth server and the fake Messages server (the fake Responses server exists) | `crates/service/src/{engines,state,settings,secrets,providers,turns,chats,server,video,trace}.rs`, `ask.rs` (the model call sites and budget resolution only: the planner/retrieval code R1-retrieve wrote stays as is), `tests/{chats,agent_e2e,api,media}.rs`, `tests/common/mod.rs` |
| **P-app** | `agent/prov-app` | D9's Settings pane (the slots per provider, R1-app's trio re-homed, the override line), D8's rows/card/citations/footer, the New chat provider picker and header badge, `Api` calls for the new routes, error-code → Settings mapping, `fake.rs` (providers routes with windows, ChatGPT status state machine, models lists, a sample chat on Claude with a web search and citations), render tests | `crates/app/src/ui/{settings,chat}.rs`, `api.rs`, `store.rs`, `fake.rs`, `citations.rs` (only `WebCitation` helpers), `ui/test_render.rs`, `examples/render_screens.rs`, `assets/` |
| **P-evals** (small) | `agent/prov-evals` | The per-provider cost column in `report`, the Claude prefix gate (D12) reading the fake Messages server's recorded bodies, `compare` refusing to compare runs across providers unless `--allow-provider-change` | `crates/evals/**` (except `src/lib.rs`'s price types, frozen in the first commit), `evals/README.md` |

No two areas share a file (`crates/ai/src/model.rs` is frozen after the first commit; a needed change goes through the orchestrator). Untouched by every area: `crates/service/src/{pipeline,retrieval,ingest,context_api,search,agent_tools,folders,sources,db}.rs` (R1's and phase 4's), `crates/embed`, `crates/shared` (frozen after the first commit), `server.rs` beyond the first commit's route lines.

### 5. Tests (fakes only; never the real APIs, never the running service or `~/Library/Application Support/Margin`)

- **P-ai-openai** (`fake_responses.rs`): bodies for key vs bearer (no `max_output_tokens`/`temperature`/… on bearer; `web_search` tool and both `include`s present when asked); `model` follows `opts.route`; `prompt_cache_key = opts.cache_key`; `web_search_call` + `url_citation` → `WebSearchResult`/`Text.citations`; `response.failed` with `subscription_sharing_usage_limit_exceeded` → `Failed { code }`; `AiError.code` from a 429 body; `Completed.model`/`stop_reason` from the response; `list_models` for both shapes with windows from the table and `None` for an unknown id; R1-ai's prefix-identity test still green.
- **P-ai-claude** (`fake_messages.rs`, an axum fake streaming the documented SSE shapes and recording request bodies): text; thinking + `signature_delta` stored verbatim; `tool_use` with `input_json_delta`; `server_tool_use` + `web_search_tool_result` (with `encrypted_content`, a `max_uses_exceeded` error variant); `citations_delta` with offsets across two merged text blocks; `compaction` block; `stop_reason: pause_turn` → `Paused`; `tool_use` with a pending `server_tool_use` → `ServerToolPending`; `event: error` overloaded; 429 with `retry-after: 7` → `retry_after == 7 s`; 429 with `enforced_spend_limit_reached` → not retryable; 401; a 400 asking for `allowed_callers` → retried with `["direct"]`; `message_delta` usage incl. `server_tool_use`, `cache_creation_input_tokens` and `iterations`; request headers/betas/`block_binding`/`cache_control` placement (three breakpoints on a step, one on a text call, the split `system` at `cache_prefix_len`, byte-identical `system[0]` across two calls of one version); `model` by route; `TextModel` with an image block; `list_models` → `context_window`/`max_output_tokens` from `max_input_tokens`/`max_tokens`; the request body's `tools` for a `ToolChoice::None` step.
- **P-agent** (`ScriptedModel`): every existing test on neutral events (phase 4's and R1's included); Claude replay rendering (grouping, same-role merge, deferred note, cancelled-call safety net, trailing `server_tool_use` and orphan `thinking` dropped, start at the latest compaction, `tool_result` with images); byte-identical re-rendering across steps (the preserved-thinking property); a web search step on each provider → rows, result, card, citations on the answer; `Paused` → a second call with the same history; `ServerToolPending` + a client call → the result closes the open call in the next step; refusal → Error; condense on Claude strips thinking from the tail; the compaction threshold scales with a small window; the strict-schema test extended to the Claude `input_schema` rendering.
- **P-service**: settings patch and `GET /api/settings` with the slots per provider and the `agent_model` migration; `provider_config`/`model_for` errors per provider and route; the `pipeline.routes` override applied only when the id is the provider's; models routes against fake servers (409 when unconfigured; windows present); the window cache refreshed on `SettingsChanged` and the fallback logged; the SIWC flow end to end against a fake auth server (authorize → the service's loopback callback → token exchange → `SignedIn`; refresh rotation; a failed refresh signs out and emits `SettingsChanged`); a chat created on Claude runs through the real `ClaudeClient` against the fake Messages server (text, a Margin tool, R1's `search` tool, a web search with card and citations, Stop with a pending server tool → Cancelled row, Continue after a simulated restart renders the same prefix); a chat on OpenAI unchanged; the title and condense calls use the default provider's `small` slot (assert the fake request's `model`); a lesson on Claude uses the `write` slot, sends `cache_prefix_len`'s split `system`, and its `PackBudget.tokens` equals `floor(window × share) − reserve` for the fake's advertised window; `no_api_key`/`auth_failed` codes per provider; **traces**: a Claude step and a lesson produce `model_call` rows with `provider: "claude"`, `model` from the fake, `usage.cache_write`, `usage.web_searches`, `cost_usd` per D14 (incl. the web-search line and Haiku's long-context tier), an OpenAI row with a price, a ChatGPT row with `cost_usd: None`, a failed call with `ok: false` and the code; `GET /api/traces/rollup` groups by provider.
- **P-app**: `fake.rs` routes; render tests of the AI pane in every ChatGPT state and with the three slots per provider (lists, free text, the window label, the override line), the New chat picker (default, disabled providers), the chat header badge, web rows/card, an answer with citations, the footer with searches, the error notices with Open Settings; `render_screens` gains `RENDER_PROVIDER=claude` and `RENDER_CHATGPT=waiting|signed_in|failed`.
- **P-evals** (`cargo test -p margin-evals`): the Claude prefix gate on recorded fake-Messages bodies; `report`'s cost table has a provider column; `compare` across providers refused without the flag.
- **Integrator e2e** (after merge): `agent_e2e.rs` gets a Claude twin of the phase-1 test (tools, replay, Stop, restart) and of R1's course-on-the-corpus test (the lesson's packed passages respect the window-derived budget; citations resolve; the trace rows carry `provider`); a web-search test per provider; the full R1 and phase 4 e2e lists stay green on OpenAI.

### 6. Review lenses

- **One seam**: every model call goes through `CallOptions` (route, cache key, prefix length); no call site names a model id or a token cap; `model_for` is the only place a route becomes a model.
- **Verbatim storage**: thinking/`redacted_thinking` with signatures, `encrypted_content`, `encrypted_index`, `encrypted_content` of reasoning: stored byte-for-byte, never reconstructed; nothing else of the provider's output is stored twice.
- **Append-only rendering**: `replay::claude` is a pure function of the stored items; no request-time edit of earlier messages; `drop_block` always sent; `input_transformations` logged.
- **Cache discipline**: the breakpoint never sits after variable text; `system[0]` is byte-identical per `prompt_version` on both fakes; at most four breakpoints.
- **Windows and budgets are data**: no literal window or cap in a packer, prompt builder or the loop; the fallback is logged and shown; `packed_tokens` traced.
- **Traces complete per provider**: every call on every provider writes a row with `provider`, `model`, `usage` (cache writes and web counters), `stop_reason`, `cost_usd` or `None` by rule; no prompt text, key or token in a row; prices only from `prices.toml`.
- **Secrets**: three Keychain entries; `ProviderConfig`'s `Debug` prints no credential; no token in events, logs, traces, the app, or the spike's notes.
- **Citations shown** wherever a cited answer is shown (Claude's terms); `encrypted_content` never leaves the service.
- **Error codes**: every `Error` item carries one of the (now fourteen) codes; the four new ones map to Open Settings → the right provider.
- **Provider isolation**: a chat on one provider never sends another's items; `Chat.provider` is immutable; Settings' default change touches no chat; a route override from `pipeline.routes` never crosses providers.
- **Every UI element works**: default-provider control, three provider blocks (key set/replace/remove, sign-in link/copy/open/poll/sign-out, three slot comboboxes with list and free text and the window label), New chat picker, header badge, web card links, citation links, footer tooltip, the Open Settings buttons.
- **Both providers in the fakes**: no test calls api.openai.com, auth.openai.com or api.anthropic.com; the spike is the only thing that does, by hand.

### 7. Sequencing

Per `docs/agent-rag-architecture.md` §4: **R1 and phase 4 first** (R1's service areas in parallel with phase 4's app areas; then 4-S and 4-agent on `Retriever`/`ingest`; then R1-app), **then providers** (this brief), then Q (quizzes), R2, phase 5. **The SIWC spike runs now, in parallel with R1 and phase 4.** Reasons unchanged from the first version, plus one: this phase's first commit rewrites `Generator`, `AppState::ai_config` and the agent's model seam, which R1-ai (`stream_parts` + `CallOptions`), R1-trace (`turns.rs` spans), R1-retrieve (`ask.rs` planner and budgets) and 4-S (`ask.rs`, `agent_tools.rs`) all build on; landing after them means one cut of those files instead of three. Phase 5 then hardens both providers at once (compaction memory per provider is already placed here; prompt-injection tests run against both `ScriptedModel` shapes and R1's retrieval outputs; the trace retention job under load). The spike's result decides whether P-ai-openai's bearer work and P-service's SIWC flow are built at all.

### 8. Questions for the owner (with recommendations)

Answered by decisions since the first version, now built as decided:

- **Q3 (first version) — the cheap side calls.** Answered by the three model slots: titles, condense, query rewrite and rerank run on the default provider's `small` slot (`claude-haiku-5-5` on Claude; OpenAI/ChatGPT per the slot). No longer open.
- **Context budget**: no fixed cap; D13 scales it with the window. **Evals and traces**: outside the app, every call traced with cost; D14. **Claude defaults**: Opus 5.5, small tasks Haiku 5.5; D9.

Still open (the brief builds the recommendation unless told otherwise):

- **Q1 — Web fetch on Claude: on or off?** It lets Claude read a page it found (or a URL the owner pasted), costs only tokens, and can only fetch URLs already in the conversation (user messages, tool results, search results), never ones the model invents; the residual risk is a URL inside an attached file or a retrieved passage. Recommend **on** with `max_uses` 8 and `max_content_tokens` 50 000. OpenAI's `web_search` opens pages itself, so there is nothing to switch there.
- **Q2 — A per-chat web switch?** Today the prompt steers searching (current information, or when asked) and the owner can say "don't search". A composer chip ("Web: on/off", stored on the chat) would make it explicit and keep searches out of chats about the owner's own material. Recommend **no switch**: the prompt steers well enough, searches are cheap ($10 per 1 000 on Claude, token-only on OpenAI), the footer shows the count and the trace rollup shows the total; add it if the count surprises you.
- **Q4 — How much of the ChatGPT plan Margin may use.** The per-app cap is set by you in ChatGPT → Settings → Usage; Margin can't read or set it, only hit it (`plan_limit_reached`). Recommend a **low cap for the spike** (so step 5 of D11 triggers), then whatever share you want for real use; lessons and videos draw on it when ChatGPT is the default.
- **Q5 — The Claude writing slot: Opus 5.5 or Sonnet 5.5?** The decision names Opus as the default model and Haiku for small tasks; the rag doc's table had Sonnet for writing (lessons, scenes, the planner) at half the price. Recommend **Opus 5.5 as the shipped default** (one default model, as decided) and a Sonnet variant in `evals/variants/` so the eval shows what it costs in quality before you switch the slot.
- **Q6 — OpenAI and ChatGPT slot defaults.** `gpt-6-luna` (small) and `gpt-6.1-sol` (ChatGPT) come from the rag doc and the SIWC docs' example; neither id is verified here, and OpenAI's windows and prices are not in `prices.toml` yet. Recommend the **orchestrator checks the ids, windows and prices against the live model list and pricing page when the first commit lands**, and you pick from the lists in Settings afterwards; no code depends on the exact ids.

Notes, not questions: ChatGPT sign-in must be finished in a browser on the service Mac (loopback callback); Claude's per-request web search cap is 8 (`max_uses`); the phase-3 question "401/403 → `no_api_key`" is answered by `auth_failed`; a model whose window the provider doesn't report runs on the 200k fallback and says so in Settings; ChatGPT calls have no dollar cost in traces by design (plan-billed), so a cost report over a ChatGPT week shows tokens, not dollars.
