## Switchable model providers — design and the binding build brief

Owner decisions (2026-10-14, superseding `docs/agent-engine-review.md`'s ACP/Codex options): Margin keeps its own loop, tools, cards and confirmations; only the model provider changes. Three providers: **OpenAI** (Responses API, API key), **ChatGPT** (Responses API with a "Sign in with ChatGPT" plan token, SIWC token sharing for local apps), **Claude** (Messages API, API key; no claude.ai login). Claude is called directly from Rust (no official Rust SDK; the Agent SDK is rejected). Web search uses each provider's hosted tool; the old "no OpenAI hosted web search" decision is reopened. Provider chosen per chat at New chat, default in Settings; a chat stays on its provider. Lessons, videos, outlines, the condense and title calls follow the default provider. Latest Claude models: `claude-opus-5-5`, `claude-sonnet-5-5`, `claude-haiku-5-5`.

Read: `crates/ai/src/{lib,responses,sse,prompts}.rs`, `crates/agent/src/{model,turn,replay,scripted,sink,prompt,tool,registry}.rs`, `crates/service/src/{engines,state,settings,secrets,turns,chats}.rs`, `tests/common/mod.rs`, `crates/shared/src/{lib,chat}.rs`, `crates/app/src/ui/{settings,chat}.rs`. Facts about the APIs were checked today against the live docs: Claude web search (`web_search_20260318`, `pause_turn`, `encrypted_content`, pricing), web fetch, streaming, thinking/preserved thinking, threshold compaction, rate limits, errors, server tools; OpenAI SIWC (sign-in, models and inference, token reference, preview limitations, errors) and the Responses `web_search` tool. Where a fact is unverified it says so. No code edited.

### 1. What the code is today, and what the switch changes

| Today | Change |
|---|---|
| `margin_ai::ResponsesClient` is the only API shape: `stream_parts` (lessons, scenes), `stream_response` → `StreamEvent` (Responses item types verbatim). | Two clients behind two traits: `TextModel` (plain streamed text from instructions + text/image parts) and `Provider` (one agent step from a provider-neutral `ModelRequest`, answered as neutral `ModelEvent`s). `ResponsesClient` and the new `ClaudeClient` implement both. |
| `margin_agent::ModelClient::stream(ResponseRequest)`; `turn.rs` builds Responses JSON (`input`, `tools`, `tool_choice`, `reasoning`, `context_management`) and consumes `StreamEvent`s by Responses item type; `replay_input` is Responses-only; `ScriptedModel` scripts `StreamEvent`s. | `ModelClient` = `Provider`. The loop builds `ModelRequest`s and consumes `ModelEvent`s; replay is per provider (`replay::openai`, `replay::claude`) over provider-verbatim stored items; `ScriptedModel` scripts `ModelEvent`s. The step/confirmation/Stop/budget machinery is untouched. |
| Stored `model_items: Vec<Value>` are Responses items (reasoning with `encrypted_content`, `function_call`, `function_call_output`, `compaction`, messages). | Still provider-verbatim JSON, one provider per chat (`Chat.provider`). Claude entries are content blocks with their role; replay regroups them into messages. Nothing stored is rewritten. |
| `AiConfig { base_url, api_key, model }` from Settings + one Keychain key; `Generator::agent_model(ai)`. | `ProviderConfig { provider, base_url, credential: ApiKey \| Bearer, model }`; three Keychain entries; `Generator::agent_model(&ProviderConfig)` picks the client. |
| Settings: one OpenAI block + chats model + effort. | Settings: default provider; OpenAI, ChatGPT and Claude blocks (key or sign-in, model lists, chat model); effort shared. New chat: provider picker. |
| No web search; the prompt says so. | `web_search` (and on Claude `web_fetch`) as hosted tools; `ToolCall`/`ToolResult` rows, a `WebResults` card, citations on answers. |

### 2. Settled designs

#### D1. The provider boundary (`crates/ai/src/model.rs`, first commit)

```rust
pub enum ProviderKind { OpenAi, ChatGpt, Claude }           // = margin_shared::Provider
pub enum Credential { ApiKey(String), Bearer(String) }        // never Debug-printed
pub struct ProviderConfig { pub provider: ProviderKind, pub base_url: String, pub credential: Credential, pub model: String }

/// Plain streamed text: lessons, scenes, titles, condense summaries.
pub trait TextModel: Send + Sync {
    fn stream_parts<'a>(&'a self, instructions: &'a str, input: &'a [InputPart], max_output_tokens: Option<u32>,
                        on_delta: Box<dyn FnMut(&str) + Send + 'a>) -> BoxFuture<'a, Result<String, AiError>>;
}

/// One agent step.
pub trait Provider: Send + Sync + 'static {
    fn kind(&self) -> ProviderKind;
    fn stream(&self, req: ModelRequest) -> BoxFuture<'_, Result<ModelStream, AiError>>;
}

pub struct ModelRequest {
    pub model: Option<String>,
    pub system: String,                       // the system prompt; the provider places it (developer item / `system`)
    pub history: Vec<Value>,                  // the chat's stored items for this provider, in replay order (D4)
    pub tools: Vec<ToolDef>,                  // function tools: name, description, parameters (strict schema)
    pub tool_choice: ToolChoice,              // Auto | None | Allowed(Vec<String>)  (D5 says what Claude does with Allowed)
    pub web: WebTools { search: bool, fetch: bool },
    pub effort: ReasoningEffort,
    pub max_output_tokens: Option<u32>,
    pub cache_key: String,                    // chat id
    pub compaction: Option<u32>,              // threshold in input tokens; None = don't ask the server
    pub parallel_tool_calls: bool,
}

pub enum ModelEvent {
    Started { index: u32, kind: StartKind },  // Reasoning | Text | ToolCall{call_id,name} | WebSearch{id} | WebFetch{id} | Other
    Delta { index: u32, delta: Delta },        // Text(String) | Reasoning(String) | Arguments(String)
    Done { index: u32, item: ModelItem },
    Completed { usage: Usage, next: Next },    // Next::Finished | Next::Paused (pause_turn: call again with the same history) | Next::ServerToolPending (results arrive in the next call)
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

`AiError::Http` gains `code: Option<String>` (OpenAI `error.code`; Anthropic `error.type`, or `error.details.error_code` when present). `AiError::is_retryable`: 429 (except the two not-retryable codes in D7), 5xx incl. 529, 504, network. `retry_after` from `retry-after` (seconds, uncapped by the loop) or `retry-after-ms`. `Usage` (shared) gains `cache_write_input_tokens`, `web_search_requests`, `web_fetch_requests` (all `#[serde(default)]`).

`ResponsesClient: Provider` adapts today's `StreamEvent`s: `output_item.added/done` by item type (`reasoning` → Reasoning, `message` → Text with `url_citation` annotations as `WebCitation { url, title, cited_text: None, start, end }`, `function_call` → ToolCall, `web_search_call` → WebSearch at `added` and, at `done`, a `WebSearchResult` with `action.query` and `action.sources` as results, `compaction` → Compaction, else Other); `Completed` → `Next::Finished`. Request: `tools` = function tools + `{"type":"web_search","search_context_size":"medium"}` when `web.search`; `include: ["reasoning.encrypted_content", "web_search_call.action.sources"]`; `tool_choice` as today (`allowed_tools` for `Allowed`); `context_management` from `compaction`; `prompt_cache_key`. Under `Credential::Bearer` (ChatGPT) the client never sends `max_output_tokens`, `temperature`, `top_p`, `user`, `previous_response_id` (unsupported on that route) and treats a `response.failed` whose code is `subscription_sharing_usage_limit_exceeded` / `subscription_sharing_usage_unavailable` as D7 says.

#### D2. The Claude client (`crates/ai/src/messages.rs`)

`POST {base_url}/v1/messages` (default `https://api.anthropic.com`), headers `x-api-key`, `anthropic-version: 2023-06-01`, `anthropic-beta: compact-2026-01-12,thinking-binding-controls-2026-08-01` (the second only while `block_binding` is sent, D4), `accept: text/event-stream`. Body for an agent step:

- `model`, `max_tokens: CLAUDE_MAX_OUTPUT_TOKENS` (32 000; `max_output_tokens` when given), `stream: true`.
- `system: [{"type":"text","text": req.system, "cache_control": {"type":"ephemeral"}}]`.
- `messages`: rendered by the agent's `replay::claude` (D4); the client only adds `cache_control: ephemeral` to the last block of the last message (moving a marker is not an edit).
- `tools`: each `ToolDef` as `{name, description, input_schema, strict: true}` (the same strict schemas; `additionalProperties:false` + `required` are already there), the last one with `cache_control`; then, when `web.search`, `{"type":"web_search_20260318","name":"web_search","max_uses": WEB_SEARCH_MAX_USES (8)}` and, when `web.fetch`, `{"type":"web_fetch_20260318","name":"web_fetch","max_uses": 8, "citations": {"enabled": true}, "max_content_tokens": 50000}`. Dynamic filtering is the default on these versions (`allowed_callers` defaults to code execution); on a 400 whose message says to set `allowed_callers` the client retries once with `"allowed_callers": ["direct"]` and remembers that per model for the process (models before 4.6).
- `tool_choice`: `{"type":"auto"}` or `{"type":"none"}` (forced `any`/`tool` are 400 on 5.5 models and never sent). `Allowed(names)`: D5.
- `thinking: {"type":"adaptive","display":"summarized","block_binding":{"prefix_mismatch_behavior":"drop_block"}}`; `output_config: {"effort": effort.as_str()}` (the five values are Claude's five). Never `budget_tokens`, never `disabled`, no sampling parameters.
- `context_management: {"edits":[{"type":"compact_20260112","trigger":{"type":"input_tokens","value": threshold}}]}` when `compaction` is set.

Stream → `ModelEvent`: `message_start` (usage: `input_tokens`, `cache_read_input_tokens` → cached, `cache_creation_input_tokens` → cache_write); `content_block_start` by block type: `thinking`/`redacted_thinking` → Reasoning, `text` → Text (consecutive `text` blocks of one response are **one** neutral Text item: the adapter maps their block indexes to one logical index and concatenates; `stored` keeps each block verbatim), `tool_use` → ToolCall, `server_tool_use` → WebSearch/WebFetch by `name`, `web_search_tool_result`/`web_fetch_tool_result` (complete in their `content_block_start`) → `Done` with the matching result view (`content` a list → results `{url, title, page_age}`; an object → `error_code` as the error), `compaction` → Compaction; `content_block_delta`: `text_delta` → Delta::Text, `thinking_delta` → Delta::Reasoning, `input_json_delta` → Delta::Arguments, `signature_delta` kept into the stored block, `citations_delta` → collected into the Text view's citations (`web_search_result_location` → `WebCitation { url, title, cited_text, start/end = offsets into the merged text }`; `char_location` from web fetch likewise); `message_delta`: cumulative usage (`output_tokens`, `server_tool_use.web_search_requests/web_fetch_requests`, `output_tokens_details.thinking_tokens` → reasoning_tokens, `iterations` summed into input/output when present) and `stop_reason`: `end_turn`/`stop_sequence` → Finished, `tool_use` with every `server_tool_use` answered → Finished (the loop runs the client tools), `tool_use` with a `server_tool_use` lacking its result → `ServerToolPending`, `pause_turn` → Paused, `max_tokens`/`model_context_window_exceeded` → `Incomplete`, `refusal` → Text as it came plus `Incomplete { reason: "refusal" }`; `message_stop` ends; `ping` ignored; `event: error` → `Failed { code: error.type }`. Non-2xx before the stream → `AiError::Http` with `code` = `error.type` (`rate_limit_error`, `overloaded_error`, `authentication_error`, …) and `details.error_code` when present (`enforced_spend_limit_reached`).

`TextModel` for Claude: `system` = instructions, one user message with `text` and `image` (`{"type":"image","source":{"type":"base64","media_type","data"}}`) blocks in order, `max_tokens: 64 000` unless given, thinking adaptive (omitted = adaptive), no effort override. `list_models(config)`: `GET /v1/models` → `data[].{id, display_name}`.

#### D3. The loop on neutral events (`crates/agent/src/turn.rs`)

`consume` keys on `ItemView` instead of item types; everything else in the contract's §4.2–4.6 stays. Additions:

- `WebSearch`/`WebFetch` at `Started`: `add_item(InProgress, ToolCall { call_id: id, name: "web_search"|"web_fetch", arguments: "", label: "Searching the web"/"Fetching a page", device: None })`; `Delta::Arguments` streams into it; at `Done` the label becomes `Searching the web for “<query>”` / `Fetching <host>`. At the matching result view: `ToolResult { ok: error.is_none(), summary: "N results"|"Fetched “title”"|<error_code in words> }` with `full_output` = the results as text (url, title, page age — never `encrypted_content`), the `ToolCall` → Done, then `Card::WebResults(WebResultsCard { query, results })` for a search. These calls are not counted against the tool output budget and are not in `unanswered_calls` (they are the provider's, not Margin's). A result that arrives in the next step (`ServerToolPending`) finishes the still-open call by id; a Stop or a turn end with one still open marks it `Cancelled` with `ToolResult { summary: "Cancelled" }` and no model item (D4 drops the block on replay).
- `Text` with citations: `AssistantText { text, citations }` (shared field, `#[serde(default)]`).
- `Completed { next: Paused }`: call again at once with the same request (the stored blocks are already in the history; the step counts toward `MAX_STEPS`); `ServerToolPending` + client calls: run them, send their outputs, the next call returns the server result first.
- `Incomplete { reason: "refusal" }`: Error "The model declined to answer (<category>)", `retryable: false`, code `bad_request`.
- Compaction per provider: `compaction = Some(COMPACT_THRESHOLD_TOKENS)` on both; a rejection (OpenAI: unknown `context_management`; Claude: 400 naming the beta header) flips `server_compaction` as today and is remembered per `(provider, base_url)` in `Turns` (phase 5's A8 lands here). The local condense keeps today's algorithm; on Claude the condensed tail is rendered with its `thinking`/`redacted_thinking` blocks removed (the summary replaces the prefix those blocks were bound to; `drop_block` covers the rest) — see D4.
- `TurnDeps.summary_model: &dyn TextModel` (the default provider's, else the chat's) for the condense summary, per the owner's "condense follows the default provider".
- `tool_choice` on Claude: D5.

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
| `ToolResult` | `user` `tool_result { tool_use_id, content: output, is_error: !ok }` |
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
- The system prompt is the `system` field on Claude (cached) and the leading developer item on OpenAI, as today.

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

Waiting banner texts come from the code as today (`wait_reason`). `RetryPolicy` unchanged.

#### D8. What a web search looks like in the chat

- Rows: a `ToolCall` named `web_search`/`web_fetch` (icon `globe`, label with the query/host, "on OpenAI"/"on Claude" is **not** shown — the chat header shows the provider), its `ToolResult` summary ("6 results", "Fetched “Title”", "Search limit reached (max_uses_exceeded)"), on-demand output = the results list.
- `Card::WebResults { query, results: [{ url, title, page_age: Option<String> }] }` after the result: a list of links (host + title, page age), opening in the system browser (`open_external`), rendered like `PassagesView`.
- Citations: `AssistantText.citations: Vec<WebCitation { url, title, cited_text: Option<String>, start: Option<u32>, end: Option<u32> }>` — the answer renders a numbered "Sources" list under the text (title → url, cited text as a tooltip), and, where `start`/`end` are present, superscript markers at those offsets (the existing `[^n]` marker styling). Claude's terms require the citations to be shown; OpenAI's `url_citation` annotations give the same. The usage footer adds "· N web searches" when `web_search_requests > 0` (Claude: $10 per 1 000; the tooltip says so).
- `tool_icon("web_search" | "web_fetch") = "globe"`.

#### D9. Settings and credentials (service `providers.rs`, `settings.rs`, `secrets.rs`; app `ui/settings.rs`)

Keychain (service Mac, never on devices): `openai-api-key` (exists), `anthropic-api-key`, `chatgpt-tokens` (JSON: `client_id`, `ext_agent_host_id`, `access_token`, `refresh_token`, `expires_at`, `scopes`, `sub`, `email`). Shared `Settings` gains (additive): `default_provider: Provider`, `claude_model`, `claude_agent_model: Option`, `has_claude_api_key`, `chatgpt_model`, `chatgpt_agent_model: Option`, `chatgpt: Option<ChatGptAccount { email, signed_in_at, expires_at }>`; the existing `openai_*`/`has_api_key`/`agent_model` are the OpenAI provider's. `SettingsPatch` gains the same plus `anthropic_api_key: Option<String>` (`""` deletes). Defaults: `claude_model = "claude-opus-5-5"`, `chatgpt_model = "gpt-6.1-sol"` (the SIWC docs' example model; the owner picks from the list), `default_provider = OpenAi`.

Routes (`routes.rs`): `GET /api/providers/{provider}/models` → `Vec<ModelInfo { id, display_name }>` (OpenAI key: `GET {base}/models` `data[].id`; ChatGPT: `GET https://api.openai.com/v1/models` with the bearer, `models[]` where `visibility == "list"`, `slug`/`display_name`; Claude: `GET /v1/models`); 409 `not_configured` when the provider has no credential. `POST /api/providers/chatgpt/start` → `{ authorize_url, state }`; `GET /api/providers/chatgpt` → `{ status: SignedOut | Waiting { authorize_url } | SignedIn { account } | Failed { message } }`; `DELETE /api/providers/chatgpt` signs out (deletes the tokens; no remote revocation endpoint is documented).

The SIWC flow (service-side, the owner completes it in a browser **on the service Mac** because the OAuth callback is a loopback URL `http://127.0.0.1:<port>/auth/callback` on the machine that runs the flow; the app on another device shows the link to open there): `ext_agent_host_id = "urn:uuid:<v4>"` generated once and kept with the tokens; first sign-in `client_id=dynamic_agent_client` + `agent_name_hint=Margin`, later sign-ins the issued `client_id` (+ `id_token_hint`, `login_hint`); `response_type=code`, `scope=openid profile email offline_access resource.invoke chatgpt.tokens.use.direct`, `resource=https://api.openai.com/v1`, fresh `state`/`nonce`, PKCE `S256`; authorization at `https://auth.openai.com/api/accounts/authorize`; the service listens on `127.0.0.1` (port from the request, reused: the path and host must not change) for the callback with `code`, `state` and, on first registration, `client_id`; token exchange `POST https://auth.openai.com/api/accounts/oauth/token` form-encoded `authorization_code` grant with `client_id`, `code`, `code_verifier`, `redirect_uri`, `resource` → `access_token` (1 h), `refresh_token` (30 d, rotates on every refresh), `id_token` (validated: signature against OpenAI's JWKS, issuer, audience = client id, expiry, nonce; `sub` kept), granted `scopes` must include `chatgpt.tokens.use.direct` else `Failed`. Refresh: on demand before a call when < 5 min remain, under a mutex, `grant_type=refresh_token`; `invalid_grant`/`token_expired`/`refresh_token_*` → tokens cleared, `SettingsChanged`, chats on ChatGPT fail with `auth_failed`. The listener closes after one callback or 10 min. `SettingsChanged` carries the account.

App Settings → AI pane (`AiSection` becomes three provider sections plus a header):
- **Default provider**: a segmented control (OpenAI · ChatGPT · Claude); a provider without a credential is selectable but shows "Set it up below"; saving it is `SettingsPatch.default_provider`.
- **OpenAI**: base URL, API key (as today), model (combobox: the list from `/models` when a key is set, else free text), "Model for chats" (the existing agent-model field).
- **ChatGPT**: "Continue with ChatGPT" → `POST start`; on the Mac app `open_external(authorize_url)`; elsewhere the link is shown with "Open this link on the Mac that runs Margin's service" and a Copy button; the pane polls `GET /api/providers/chatgpt` every 2 s while `Waiting`; `SignedIn` shows the account email, "Signed in <date>", "Sign out…" (confirm), the model combobox from the account's list, "Model for chats"; `Failed` shows the message with "Try again". Help text: "Margin uses your ChatGPT plan for what you ask it here. You can cap Margin's share in ChatGPT → Settings → Usage."
- **Claude**: API key (same key UI as OpenAI's: stored, replace, remove), model combobox from `/v1/models` (defaults listed first: Opus 5.5, Sonnet 5.5, Haiku 5.5), "Model for chats".
- **Chats** section keeps Reasoning (shared across providers; the Claude client maps it to `effort`).
- "Open Settings" from an `auth_failed`/`plan_*`/`spend_limit_reached` error lands on this pane.

New chat (`ui/chat.rs`, Ask screen): a provider segmented control above the composer, preset to the default; a provider without a credential is disabled with a tooltip "Set up in Settings → AI". `NewChat.provider` is sent; the chat header (`TopBar`) shows a small provider badge; the sidebar row doesn't. Settings' default change never moves existing chats.

#### D10. Service wiring (`engines.rs`, `state.rs`, `turns.rs`, `chats.rs`, `ask.rs` untouched)

`AppState::provider_config(provider) -> AppResult<ProviderConfig>` (errors: `no_api_key` naming the provider, `no_model`, and for ChatGPT `auth_failed` when signed out); `ai_config()` = `provider_config(default_provider)`; `agent_config(provider)` picks that provider's chat model. `Generator::agent_model(&ProviderConfig) -> Arc<dyn Provider>`; `Generator::text_model(&ProviderConfig) -> Arc<dyn TextModel>` replaces the direct `ResponsesClient` use in `write_lesson`/`write_scene`; the title call uses `text_model(default)` with `max_output_tokens` only off ChatGPT (unsupported there; the instructions keep it short); the condense summary gets `TurnDeps.summary_model`. `turns::run` uses `chat.provider`. `Chat.usage` keeps adding (the web counters included). Recovery and Stop are unchanged.

#### D11. The SIWC spike — before area P-ai-openai's ChatGPT part is built

A throwaway binary in a worktree that is never merged (`examples/siwc_spike.rs` in `crates/ai`, removed afterwards), run by the owner with their own plan, writing nothing to Margin's data folder and touching no running service. It verifies, in order: (1) registration with `dynamic_agent_client` + loopback on this Mac and the token exchange; (2) a Responses call with the bearer and `store: false`, `stream: true`, function tools with `strict: true`, `include: ["reasoning.encrypted_content"]`, a replayed `reasoning` item, `tool_choice: allowed_tools`, `context_management` compaction — each accepted or the exact 400 recorded; (3) `web_search` allowed under the plan, and a `web_search_call` item accepted when replayed as input; (4) a refresh with rotation; (5) the per-app cap (set low in ChatGPT → Settings → Usage) producing `subscription_sharing_usage_limit_exceeded`. Outcomes: (1) or (2) fail → **the ChatGPT provider is dropped**: no `Provider::ChatGpt` variant work in P-service/P-app (the enum keeps the variant, unselectable, so stored data never needs it), and the brief's other parts go ahead unchanged; (3) fails → ChatGPT stays without web search (`WebTools { search: false }` for that credential; the prompt line is per request); a feature in (2) fails → that feature is turned off for `Credential::Bearer` the way `max_output_tokens` already is. The spike's log (no tokens) goes into `docs/agent-siwc-spike.md`.

### 3. First commit (orchestrator lands on `claude/agent-build` before branching)

`crates/shared`:
1. `Provider { OpenAi, ChatGpt, Claude }` (`rename_all = "snake_case"`, `Default = OpenAi`) with `label()`; `Chat.provider` (`#[serde(default)]`); `NewChat.provider: Option<Provider>`.
2. `ChatItemKind::AssistantText { text, #[serde(default)] citations: Vec<WebCitation> }`; `WebCitation`; `Card::WebResults(WebResultsCard { query, results: Vec<WebResult { url, title, page_age } > })`; `error_code::{AUTH_FAILED, SPEND_LIMIT_REACHED, PLAN_LIMIT_REACHED, PLAN_INELIGIBLE}`; `Usage` three new fields.
3. `Settings`/`SettingsPatch` additions (D9), `ChatGptAccount`, `ChatGptStatus`, `ModelInfo`; `routes::{providers_models(provider), PROVIDERS_CHATGPT, PROVIDERS_CHATGPT_START}` + table rows; `Event::SettingsChanged` unchanged (carries the new fields).
4. Tests: round trips; an `AssistantText` without `citations` parses; a `Chat` without `provider` is OpenAI.

`crates/ai`: `src/model.rs` with everything in D1 (types, both traits, `WebTools`, `ToolDef`, `ToolChoice`, `ModelEvent`…), `AiError::Http.code`, `pub use`; `impl Provider for ResponsesClient` and `impl TextModel for ResponsesClient` as `unimplemented!("providers, P-ai-openai")` bodies; `src/messages.rs` with `pub struct ClaudeClient` + `new(base_url, api_key, model)` and both impls `unimplemented!("providers, P-ai-claude")`; `Cargo.toml`: `base64` (already in the workspace lock; needed for Claude image blocks and the SIWC code verifier), `sha2` (PKCE), nothing else.

`crates/agent`: `ModelClient` = re-export of `margin_ai::Provider` (the old trait removed); `TurnDeps.summary_model`; `TurnInput.provider`; `replay` becomes a module directory with `openai.rs` (today's code) and `claude.rs` whose functions are `unimplemented!("providers, P-agent")`; `ScriptedModel` keeps compiling by scripting `ModelEvent`s (a mechanical move: the existing `Scripted::Events(Vec<StreamEvent>)` becomes `Vec<ModelEvent>`; the test helpers that built Responses items (`message_item`, …) stay and gain `ModelEvent` constructors). The crate's tests may be red after this commit; P-agent makes them green.

`crates/service`: `ProviderConfig` replaces `AiConfig` in `engines.rs` (the `Generator` signatures); `state::provider_config`/`agent_config` stubs returning today's OpenAI config for `Provider::OpenAi` and `unimplemented!("providers, P-service")` otherwise; `providers.rs` with the three route handlers as `unimplemented!`; `server.rs` routes wired; `secrets.rs` constants `ANTHROPIC_API_KEY`, `CHATGPT_TOKENS`.

`crates/app`: nothing (P-app owns its files; `fake.rs`'s `Settings` sample gains the new fields with defaults — P-app).

Contract (`docs/agent-contract.md`): §1 ownership (the ai crate has two owners by file), §2 a "Providers" row, §3 data, §4.2 the request per provider, §4.5 the error table (D7), §4.11 replay per provider (D4), §5 web tools, §6 unchanged, §7 `CLAUDE_MAX_OUTPUT_TOKENS` 32 000, `WEB_SEARCH_MAX_USES` 8, `WEB_FETCH_MAX_USES` 8, `WEB_FETCH_MAX_CONTENT_TOKENS` 50 000, §8 tests (section 5 below). Architecture §3: a row for the provider decision.

### 4. Areas (worktree each; files exclusive; all branch from the first commit)

| Area | Worktree | Builds | Owns (exclusively) |
|---|---|---|---|
| **P-ai-openai** | `agent/prov-openai` | `ResponsesClient: Provider + TextModel` (D1): neutral request → body (web_search tool, includes, bearer rules), `StreamEvent` → `ModelEvent` (web_search_call, url_citation), `AiError.code`, SIWC failure codes, `list_models` for key and bearer; the spike example (D11) | `crates/ai/src/{lib,responses}.rs`, `crates/ai/tests/fake_responses.rs`, `crates/ai/examples/siwc_spike.rs` |
| **P-ai-claude** | `agent/prov-claude` | `ClaudeClient` (D2): request rendering, headers/betas, SSE → `ModelEvent` with text-block merging and citations, server tools, pause/pending, usage, errors and `retry-after`, `allowed_callers` fallback, `TextModel` with image blocks, `list_models`; a fake Messages server in tests | `crates/ai/src/messages.rs`, `crates/ai/tests/fake_messages.rs` |
| **P-agent** | `agent/prov-agent` | The loop on `ModelEvent` (D3), `replay::claude` (D4) and the per-provider `unanswered_calls`/`cancelled_call_output`, D5 gating, D6 prompt, web rows/cards/citations, pause/pending continuation, per-provider compaction memory hook, `ScriptedModel` neutral + all tests green | `crates/agent/src/{turn,replay/*,prompt,scripted,sink}.rs`, `crates/agent/tests/turn.rs` |
| **P-service** | `agent/prov-service` | D9's secrets/routes/SIWC flow (`providers.rs`), `provider_config`/`agent_config`, `Generator` impls per provider, chat provider at creation (`chats.rs`), `turns.rs` (provider per chat, `summary_model`, title via default provider, compaction memory per provider/base URL), settings patch fields, tests with a fake auth server and a fake Messages server (the fake Responses server exists) | `crates/service/src/{engines,state,settings,secrets,providers,turns,chats,server}.rs`, `tests/{chats,agent_e2e,api}.rs`, `tests/common/mod.rs` |
| **P-app** | `agent/prov-app` | D9's Settings pane, D8's rows/card/citations/footer, the New chat provider picker and header badge, `Api` calls for the new routes, error-code → Settings mapping, `fake.rs` (providers routes, ChatGPT status state machine, models lists, a sample chat on Claude with a web search and citations), render tests | `crates/app/src/ui/{settings,chat}.rs`, `api.rs`, `store.rs`, `fake.rs`, `citations.rs` (only `WebCitation` helpers), `ui/test_render.rs`, `examples/render_screens.rs`, `assets/` |

No two areas share a file (`crates/ai/src/model.rs` is frozen after the first commit; a needed change goes through the orchestrator). The service's `ask.rs`/`agent_tools.rs` are untouched (phase 4's).

### 5. Tests (fakes only; never the real APIs, never the running service or `~/Library/Application Support/Margin`)

- **P-ai-openai** (`fake_responses.rs`): bodies for key vs bearer (no `max_output_tokens`/`temperature`/… on bearer; `web_search` tool and both `include`s present when asked); `web_search_call` + `url_citation` → `WebSearchResult`/`Text.citations`; `response.failed` with `subscription_sharing_usage_limit_exceeded` → `Failed { code }`; `AiError.code` from a 429 body; `list_models` for both shapes.
- **P-ai-claude** (`fake_messages.rs`, an axum fake streaming the documented SSE shapes): text; thinking + `signature_delta` stored verbatim; `tool_use` with `input_json_delta`; `server_tool_use` + `web_search_tool_result` (with `encrypted_content`, a `max_uses_exceeded` error variant); `citations_delta` with offsets across two merged text blocks; `compaction` block; `stop_reason: pause_turn` → `Paused`; `tool_use` with a pending `server_tool_use` → `ServerToolPending`; `event: error` overloaded; 429 with `retry-after: 7` → `retry_after == 7 s`; 429 with `enforced_spend_limit_reached` → not retryable; 401; a 400 asking for `allowed_callers` → retried with `["direct"]`; `message_delta` usage incl. `server_tool_use` and `iterations`; request headers/betas/`block_binding`/`cache_control` placement; `TextModel` with an image block; the request body's `tools` for a `ToolChoice::None` step.
- **P-agent** (`ScriptedModel`): every existing test on neutral events; Claude replay rendering (grouping, same-role merge, deferred note, cancelled-call safety net, trailing `server_tool_use` and orphan `thinking` dropped, start at the latest compaction); byte-identical re-rendering across steps (the preserved-thinking property); a web search step on each provider → rows, result, card, citations on the answer; `Paused` → a second call with the same history; `ServerToolPending` + a client call → the result closes the open call in the next step; refusal → Error; condense on Claude strips thinking from the tail; the strict-schema test extended to the Claude `input_schema` rendering.
- **P-service**: settings patch and `GET /api/settings` with the new fields; `provider_config` errors per provider; models routes against fake servers (409 when unconfigured); the SIWC flow end to end against a fake auth server (authorize → the service's loopback callback → token exchange → `SignedIn`; refresh rotation; a failed refresh signs out and emits `SettingsChanged`); a chat created on Claude runs through the real `ClaudeClient` against the fake Messages server (text, a Margin tool, a web search with card and citations, Stop with a pending server tool → Cancelled row, Continue after a simulated restart renders the same prefix); a chat on OpenAI unchanged; the title call on the default provider; `no_api_key`/`auth_failed` codes per provider.
- **P-app**: `fake.rs` routes; render tests of the AI pane in every ChatGPT state, the New chat picker (default, disabled providers), the chat header badge, web rows/card, an answer with citations, the footer with searches, the error notices with Open Settings; `render_screens` gains `RENDER_PROVIDER=claude` and `RENDER_CHATGPT=waiting|signed_in|failed`.
- **Integrator e2e** (after merge): `agent_e2e.rs` gets a Claude twin of the phase-1 test (tools, replay, Stop, restart) and a web-search test per provider.

### 6. Review lenses

- **Verbatim storage**: thinking/`redacted_thinking` with signatures, `encrypted_content`, `encrypted_index`, `encrypted_content` of reasoning: stored byte-for-byte, never reconstructed; nothing else of the provider's output is stored twice.
- **Append-only rendering**: `replay::claude` is a pure function of the stored items; no request-time edit of earlier messages; `drop_block` always sent; `input_transformations` logged.
- **Secrets**: three Keychain entries; `ProviderConfig`'s `Debug` prints no credential; no token in events, logs, the app, or the spike's notes.
- **Citations shown** wherever a cited answer is shown (Claude's terms); `encrypted_content` never leaves the service.
- **Error codes**: every `Error` item carries one of the (now fourteen) codes; the four new ones map to Open Settings → the right provider.
- **Provider isolation**: a chat on one provider never sends another's items; `Chat.provider` is immutable; Settings' default change touches no chat.
- **Every UI element works**: default-provider control, three provider blocks (key set/replace/remove, sign-in link/copy/open/poll/sign-out, model combobox with list and free text), New chat picker, header badge, web card links, citation links, footer tooltip, the Open Settings buttons.
- **Both providers in the fakes**: no test calls api.openai.com, auth.openai.com or api.anthropic.com; the spike is the only thing that does, by hand.

### 7. Sequencing against the phase-4 brief

**Phase 4 first, then providers, then phase 5; the SIWC spike now, in parallel with phase 4.** Reasons: phase 4 is briefed, small and file-exclusive, and its only model-side dependency (`lesson_context` passages → `write_lesson`) is unchanged by providers; the providers' first commit rewrites the agent's model seam (`ModelClient`, `ScriptedModel`, replay) and would invalidate phase 4's e2e scaffolding mid-flight if interleaved, and both touch `ui/chat.rs`, `ui/settings.rs`, `fake.rs`, service `state.rs` and the service tests, which only one phase can own at a time. Phase 5 then hardens both providers at once (compaction memory per provider is already placed here; prompt-injection tests run against both `ScriptedModel` shapes; tracing spans carry the provider). The spike needs no branch code and its result decides whether P-ai-openai's bearer work and P-service's SIWC flow are built at all, so it runs during phase 4.

### 8. Questions for the owner (with recommendations)

- **Q1 — Web fetch on Claude: on or off?** It lets Claude read a page it found (or a URL the owner pasted), costs only tokens, and can only fetch URLs already in the conversation (user messages, tool results, search results), never ones the model invents; the residual risk is a URL inside an attached file. Recommend **on** with `max_uses` 8 and `max_content_tokens` 50 000. OpenAI's `web_search` opens pages itself, so there is nothing to switch there.
- **Q2 — A per-chat web switch?** Today the prompt steers searching (current information, or when asked) and the owner can say "don't search". A composer chip ("Web: on/off", stored on the chat) would make it explicit and keep searches out of chats about the owner's own material. Recommend **no switch**: the prompt steers well enough, searches are cheap ($10 per 1 000 on Claude, token-only on OpenAI), and the footer shows the count; add it if the count surprises you.
- **Q3 — The cheap side calls.** Titles and condense summaries "follow the default provider": with the default's main model (Opus 5.5 / the chat model) or that provider's cheapest (Haiku 5.5; on OpenAI/ChatGPT the chat model, there is no cheaper safe choice in Settings)? Recommend **cheapest on Claude (`claude-haiku-5-5`), the chat model elsewhere**; the brief builds that unless you say otherwise.

Notes, not questions: ChatGPT sign-in must be finished in a browser on the service Mac (loopback callback); Margin's share of the plan is capped by you in ChatGPT → Settings → Usage, and lessons/videos draw on it when ChatGPT is the default; Claude's per-request web search cap is 8 (`max_uses`); the Claude default for everything is `claude-opus-5-5` until you pick another in Settings; the phase-3 question "401/403 → `no_api_key`" is answered by `auth_failed`.
