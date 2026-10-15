# Agent build contract

The binding interface for building the agent described in `docs/agent-architecture.md`
(read its section 3, "Owner decisions", first). This file says who builds what, in which
phase, and exactly how a turn behaves. The types and signatures it names are in the code
on branch `claude/agent-build`; where this file and a doc comment disagree, fix the
disagreement before coding past it.

Code against the contract, not against another implementer's unfinished work. Bodies
marked `unimplemented!("… (phase N, X owner)")` are where each owner fills in.

## 1. Ownership

Four implementers work in parallel, one crate each. Nobody edits another owner's crate;
a change to a shared signature (anything in `crates/shared`, a `pub` item another crate
uses) is a contract change: report it rather than making it, unless it only adds.

| Owner | Crate | Owns |
|---|---|---|
| **ai** | `crates/ai` | `ResponsesClient::stream_response` (`src/responses.rs`): SSE → `StreamEvent`s, `Retry-After`, terminal events. Fake-server tests in `tests/`. |
| **agent** | `crates/agent` | `run_turn`, replay, the system prompt and message context, `strict_schema`, `Typed`, `truncate_output`, device tools and their text output. Tests against `ScriptedModel` and a fake `TurnSink`/`DeviceBridge`. |
| **service** | `crates/service` | Storage (`chats`, `workspaces`), the routes, `turns` (sink, stop/continue/confirm/pending, recovery), `devices::DeviceHub` and the events socket, `agent_tools` (library/course/source/job tools), settings, provenance for Q2. Integration tests in `tests/` with `FakeGenerator.agent` (a `ScriptedModel`). |
| **app** | `crates/app` | `api.rs` calls for every new route, the store (already applies the new events), the chat screen and cards, the Chats sidebar section, Settings (agent model, reasoning effort, Folders pane), the device-tool runner (app control, folders per platform), the fake service in `fake.rs`. |

**R1 (retrieval core, traces, context API, evals; `docs/agent-rag-architecture.md` §6).**
Frozen after R1's first commit and changed only through the orchestrator:
`crates/shared` (`pipeline.rs`, `context.rs`, the `routes.rs` context/trace rows),
`crates/service/src/pipeline.rs` (stage traits, `Query`, `Hit`, `Packed`, `PackBudget`,
`Prompt`, `Registry`, `Pipeline::build`) and `server.rs`. Areas own, exclusively:

| Area | Owns |
|---|---|
| **R1-embed** | `crates/embed/src/{chunk,extract,fake,lib}.rs` (its `pdf`, `image`, `vision` and `coreml` files were the macOS backends, since removed; the readers and `onnx/` came with LX), `crates/embed/tests/*` |
| **R1-store** | `crates/service/src/{ingest,sources,embedding,engines,state,db,library}.rs` (`db.rs` additive), `tests/sources.rs`, `tests/common/mod.rs` |
| **R1-retrieve** | `crates/service/src/{retrieval,search,agent_tools,ask,context_api}.rs`, `tests/{courses,api}.rs`, the search-tool tests in `tests/agent_e2e.rs` |
| **R1-ai** | `crates/ai/src/{lib,prompts,responses}.rs`, `crates/ai/src/prompts/evals.rs`, `crates/ai/tests/{fake_responses,fixtures_render}.rs` |
| **R1-trace** | `crates/service/src/{trace,turns,video,chats}.rs` (`chats.rs` only the `?traces=1` flag), `tests/{chats,media}.rs` |
| **R1-evals** | `crates/evals/**`, `evals/**` |

**Providers (switchable model providers; `docs/agent-providers-brief.md` §3–§4).** Frozen
after the providers phase's first commit and changed only through the orchestrator:
`crates/shared` (all of it), `crates/ai/src/model.rs` (the one model seam), the price
types in `crates/evals/src/lib.rs`, and `server.rs` beyond the provider route lines. The
ai crate has two owners by file: **P-ai-openai** owns `crates/ai/src/{lib,responses,windows}.rs`,
`tests/fake_responses.rs` and `examples/siwc_spike.rs`; **P-ai-claude** owns
`crates/ai/src/messages.rs` and `tests/fake_messages.rs`. **P-agent** owns
`crates/agent/src/{turn,replay/*,prompt,scripted,sink}.rs` and `tests/turn.rs`;
**P-service** `crates/service/src/{engines,state,settings,secrets,providers,turns,chats,server,video,trace}.rs`,
the model call sites of `ask.rs`, `tests/{chats,agent_e2e,api,media}.rs` and
`tests/common/mod.rs`; **P-app** `crates/app/src/ui/{settings,chat}.rs`, `api.rs`,
`store.rs`, `fake.rs`, `citations.rs` (the `WebCitation` helpers), `ui/test_render.rs`,
`examples/render_screens.rs`, `assets/`; **P-evals** `crates/evals/**` but the price
types, and `evals/README.md`.

**L1 (accounts and principals; `docs/l1-brief.md` §3).** Frozen after L1's first commit
and changed only through the orchestrator: `crates/shared` (all of it), `server.rs` (a
route an area needs that isn't there is reported), and the seams below. Merge order:
L1-auth → L1-groups → L1-events → L1-mcp → L1-app.

| Area | Owns (exclusively) |
|---|---|
| **L1-auth** | `crates/service/src/{auth,principal,people,invitations}.rs`, `tests/{auth,api}.rs`, `tests/common/mod.rs` |
| **L1-groups** | `crates/service/src/{groups,agents,access,context_api,trace}.rs`, `tests/{groups,agents}.rs` |
| **L1-events** | `crates/service/src/{events,state,chats,library,turns,ask,search,sources,uploads,folders,workspaces,agent_tools,ingest,settings,video,embedding}.rs`, `tests/{events,chats,courses,sources,folders,media,r1_e2e,agent_e2e,claude_e2e,providers,routes}.rs` |
| **L1-mcp** | `crates/mcp/**` |
| **L1-app** | `crates/app/**` |

Seams: `principal::require_principal` calls `agents::principal_for_token(state, token) ->
AppResult<Option<Arc<Principal>>>` (stub `Ok(None)`) and `groups::expansion(state,
person_id) -> AppResult<Arc<Expansion>>` (stub: `[company, person/<id>]` with the stored
role); L1-auth and L1-groups emit only through `App::{emit, emit_to, emit_row}` and add
no field to `App` (`expansions`, `access_sources` are there); `Harness::sign_in_as(code,
device)` and `BOB_CODE` (`bob-dev`) are in `tests/common/mod.rs`. Handlers of the new
routes are `unimplemented!("L1, <area>")` until their area lands.

`crates/shared` is the contract and is changed only through the orchestrator. It already
contains everything below; `crates/video` and `crates/embed` are untouched.

## 2. Phases

All five are built before the owner's hands-on run. Each owner builds its part of a
phase; a phase is done when the four parts are merged and its tests pass.

| Phase | ai | agent | service | app |
|---|---|---|---|---|
| **1 Chat foundation** | `stream_response` with every `StreamEvent`, HTTP errors with `retry_after` | `run_turn` (steps, streaming into items, parallel Read calls, retries, step cap, budgets), `replay_input`, `unanswered_calls`, `user_message_input`, `system_prompt`, `strict_schema`, `Typed`, `truncate_output` | `chats` routes and storage, `turns` (start, run, stop, continue, pending, recovery, `ServiceSink`), Read tools, settings fields, `Generator::agent_model` | Ask screen becomes the chat (composer, transcript, streaming text, tool rows with on-demand output, Stop, Continue/Retry, pending message with cancel), Chats in the sidebar (New chat button; rename/delete by right-click/⋯ and long press), Settings: agent model + reasoning effort |
| **2 Courses through the agent** | — | Confirmation parking (`wait_for_decision`), Write/Destructive ordering | Write and Destructive tools, `propose_course` + `accept_proposal`, `decide`, `AcceptProposal.chips`, `Proposal.chat_id` + Action on accept, Q2 provenance; remove `POST /api/ask` and the proposal job | Outline card (the existing outline editor with chips, Create lessons), result cards, confirmation cards (Approve/Deny from any device); remove the old Ask composer and `Api::ask` |
| **3 Jobs, attachments, app control** | — | App-control device tools, `device_output_text`; uncapped `Retry-After` with `sink.retrying`/`clear_waiting` (4.5); `Error.code` on every Error; context in the user role (4.11) | `write_lesson`, `regenerate_lesson`, `make_video`, `attach_sources`; files attached to chats (cleanup skips them, deleted with the chat); `DeviceHub::call`/`deliver`, events socket registration, `DeviceChanged`, `Device.online`, `DeviceDeleted` + workspace cleanup on revoke; Stop awaits started writes (4.6); `Chat.waiting`; model-written titles (§3); `Superseded` proposals (§5) | Usage footer, attach files in the chat composer, device presence (online dots in Settings → Devices), the socket's `ServerFrame` parsing and `DeviceFrame` sending, app-control runner (navigate, open citation, show passage, toggle sidebar); waiting banner; titles by `title_by`; superseded outline card; "Open Settings" for `no_api_key`/`no_model`; reasoning effort list from `ReasoningEffort::ALL`; Create lessons opens the first lesson |
| **4 Folders** | — | 4-agent: `FOLDER_TOOLS` = `list_files`, `find_files` (service `Read` tools over the folder's rows and sources) and `sync_folder` (the one device tool); a folder's text is searched with R1's `search` scoped to the workspace | 4-S: folder files are sources (`Source.workspace_id`, upload rules, no size cap), the manifest route and `Workspace.sync`, scoping and deletion, folders in courses; ingest and retrieval are R1's (`docs/agent-rag-architecture.md` §2.7, §6.6) | 4-core: handle store, sync engine over `Dir`, Attach folder, the strip, the Folders pane, folder citations in the viewer; 4a macOS + Linux, 4b iOS/iPadOS, 4c Android: `pick_folder`, `folders::open`/`forget` |
| **5 Hardening** (`docs/phase5-brief.md`) | 5-retry: `Backoff` the one retry policy (`Retry-After` uncapped + 0–1 s jitter), `Retrying<M: TextModel>` for every job call with waits reported through `on_wait` | 5-agent: the normaliser on every tool output in `Turn::record`, injection tests (outputs keep their frame, no URL-taking tool, destructive tools confirm), `TurnSink::compaction` markers, compaction/condense replays across restart and Continue, the condense summary cap | 5-guard: `text::for_model` at every edge (tool outputs, packed passages, device answers, context-API ingests, web titles, stored names and titles) and the injection suite; 5-retry: every `text_model` wrapped, waits shown on lessons/videos/proposals; 5-security: per-principal rate limits, no CORS, callback CSP, media `Cache-Control`, the §7 limits pinned; 5-data: `check-data`, `backup`, the schema-version guard, restore drill; 5-ops: alerts from traces (`GET /api/alerts`, journald, `/api/version` counts, webhook), `p95_ms`, `provider_ms`, `margin-eval alerts`/`perf` | 5-ios: `BGContinuedProcessingTask` (iOS 26) while a held folder syncs or a chat reading one runs (`state::active = foreground ‖ continued`, `background::Coordinator`) |
| **Providers** | One seam (`model.rs`: `CallOptions`, `TextModel`, `Provider`, `ModelRequest`, `ModelEvent`); `ResponsesClient` on it (OpenAI, ChatGPT bearer), `ClaudeClient` (Messages API, D2/D12), `list_models` with windows | The loop on neutral `ModelEvent`s, `replay::claude`, Claude tool gating (D5), web rows/cards/citations, pause/pending continuation, window-scaled compaction | `model_for`/`provider_config` per provider with the window cache, three secrets in the service's store, the SIWC flow, chats on their provider, traces per provider (D14) | Settings: default provider and three slots per provider, ChatGPT sign-in, Claude key; New chat provider picker; web card and citations |
| **R1 Retrieval core** | `CallOptions { route, cache_key, max_output_tokens }`, constant-first prompts with `prompt_version`, `plan_course` and rerank prompts, eval prompts | — (the `search` tool family lives in the service) | `passage`/`document`/`project`/`trace` storage with HNSW + FULLTEXT indexes, `ingest` (hash diff, batched writes, one-pass migration), hybrid retrieval with RRF and MMR packing, the `search`/`read_passage`/`document_outline`/`view_picture` tools, the outline planner, traces and rollups, the context API | R1-app after phase 4's app areas: three model slots, Pipeline settings, trace retention, Passages card locators, line highlights |
| **R2 Rerank, rewriting, prompt versions** (`docs/r2-brief.md`) | R2-ai: the `rewrite-v1` prompt and `rewrite_queries`, `prompts::catalog()` (versions and constant hashes), the rerank label with the page | — | R2-retrieve: `"listwise-small"` and `"small-expand-v1"` on the `Small` route, the `"onnx-crossencoder"` stage, `RerankRequest`/`RewriteRequest` with purpose and provider, `written_with` on lessons, proposals and videos, `GET /api/pipeline/prompts`; R2-xenc: the ONNX cross-encoder and its download routes; R2-evals: `evals/prompts.lock` in CI, recorded reranks and rewrites, `compare` on prompt versions | R2-app: the Rewriter row, Settings → Sources' reranker model row, "Written with …" in a lesson's details |

Between the phase 1 and phase 2 merges courses can't be created (the old composer is
unrouted until phase 2 reuses its outline editor); no release happens in between.

Web search is the provider's hosted tool (providers phase, superseding Q1): `web_search`
on OpenAI and ChatGPT, `web_search` and `web_fetch` on Claude (§5). Nothing in the
registry assumes a fixed set of tools. The service registers the app-control
device tools from phase 3 and the folder tools from phase 4 (the `tools` array changes,
and the prompt cache prefix with it, once at each of those releases).

## 3. Data

All in `margin_shared` (`chat.rs`, `device.rs`, `lib.rs`):

- `Chat { id, title, title_by: TitleSource, state: ChatState, workspace_ids, source_ids,
  pending, usage, waiting: Option<Waiting { until, reason }>, provider: Provider, … }`
  (`provider` set at creation from `NewChat.provider` else Settings' `default_provider`,
  never changed; chats stored before it are `openai`).
- Providers (`margin_shared::providers`): `Provider { OpenAi, ChatGpt, Claude }` (wire
  `openai`/`chatgpt`/`claude`), `RouteModels { chat, write, small }`, `ModelInfo { id,
  display_name, context_window, max_output_tokens }`, `ChatGptAccount`, `ChatGptStatus`,
  `WebCitation`, `WebResult`; `Settings` gains `default_provider`, `openai_models`,
  `chatgpt_models`, `claude_models`, `has_claude_api_key`, `chatgpt` (a `Settings` without
  slots migrates `agent_model` into `openai_models.chat`); `SettingsPatch` the same plus
  `anthropic_api_key`; `AssistantText.citations: Vec<WebCitation>`;
  `Card::WebResults(WebResultsCard { query, results })`; `Usage` gains
  `cache_write_input_tokens`, `web_search_requests`, `web_fetch_requests`;
  `context::TraceUsage.web_fetches`; `error_code::{AUTH_FAILED, SPEND_LIMIT_REACHED,
  PLAN_LIMIT_REACHED, PLAN_INELIGIBLE}`; routes `providers_models(provider)`,
  `PROVIDERS_CHATGPT`, `PROVIDERS_CHATGPT_START`.
  `ChatState`: `Idle | Running { turn_id, device_id, started_at } | WaitingForUser {
  turn_id, device_id, item_id } | Interrupted { turn_id } | Failed { turn_id, error }`.
- `ChatItem { id, chat_id, turn_id, seq, status: InProgress|Done|Cancelled, … , kind }`,
  kinds `UserMessage`, `AssistantText`, `Reasoning`, `ToolCall`, `ToolResult`, `Card`,
  `Action`, `Error { message, retryable, code: Option<String> }` (flattened, tag `kind`).
  `code` is one of `margin_shared::error_code`: `no_api_key`, `no_model`,
  `rate_limited`, `server_error`, `network`, `context_too_long`, `incomplete`,
  `bad_request`, `storage`, `interrupted` (`None` on items from older builds).
- `Card`: `Outline { proposal_id }`, `Result { target, title }`, `Passages { query,
  passages }`, `Confirmation { tool, title, detail, approve_label, decision }`.
- Phase 4: `Source.workspace_id` and `Source.modified` (a folder file and its stamp);
  `Citation.file: Option<WorkspaceFile { workspace_id, path }>`; `Workspace.sync:
  WorkspaceSync { files, bytes, uploading, reading, failed, last_synced_at }`;
  `SyncManifest { files: Vec<ManifestFile { path, kind, bytes, modified, sha256 }> }`
  (`FileStamp` flattened), `SyncPlan { upload, kept, removed }`; the allow-list
  `FolderFileType`, `FOLDER_FILE_EXTENSIONS`, `FOLDER_FILE_NAMES`, `folder_file_type`,
  `FOLDER_FILE_TYPES_TEXT`; `is_skipped_name` keeps only hidden and secret-looking
  names. Routes `workspace_sync(id)`, `workspace_files(id)`; `SOURCES`/`SOURCE_UPLOADS`
  take `workspace_id` and `modified`.
- `Workspace`, `NewWorkspace`, `Platform`; `Device.online`; `Settings.reasoning_effort`
  and `Settings.agent_model` (+ patch fields); `Proposal.chat_id`; `AcceptProposal.chips`;
  `ProposalStatus::Superseded { by }` (§5).
- `ReasoningEffort::ALL` = Low, Medium, High, XHigh, Max (sent as `low`, `medium`,
  `high`, `xhigh`, `max`), default Medium. A hidden `Minimal` exists only so settings
  stored by an older build still parse; it is sent as `low` and Settings never offers it.
- Events: `ChatChanged`, `ChatDeleted`, `ChatItemChanged`, `ChatItemDelta { offset }`,
  `WorkspaceChanged`, `WorkspaceDeleted`, `DeviceChanged`, `DeviceDeleted`.
- Socket frames: `ServerFrame = Event | DirectedFrame { ToolRequest | ToolCancel }`
  service → device; `DeviceFrame::ToolResult` device → service.
- Routes: `routes::CHATS`, `chat`, `chat_messages`, `chat_pending`, `chat_stop`,
  `chat_continue`, `chat_confirm`, `chat_item_output`, `WORKSPACES`, `workspace`
  (the table at the top of `routes.rs` gives bodies and answers).

Service storage (`db.rs`): table `chat` holds `Chat`; `chat_item` holds
`chats::StoredChatItem { item, model_items, full_output }` keyed by item id (query by
`chat_id`); `workspace` holds `Workspace`. No migration of existing data.

R1 storage (`db.rs`, `docs/agent-rag-architecture.md` §2.2, §2.11): `project` and
`document` are `{ doc }` tables like the others (`context::Project`,
`context::Document`; `document` indexed on `doc.project_id` and `doc.access.*`).
`passage` is SCHEMAFULL with top-level fields (`db::PassageRow`: `project_id`,
`document_id`, `origin`, `scope_ids`, `access`, `unit_id`, `modality`, `ord`, `page`,
`start_line`/`end_line`, `start_ms`/`end_ms`, `section_path`, `text`, `file`,
`chunk_hash`, `embedder`, `vector`), indexed by `document_id`, `scope_ids.*`, `access.*`,
unique `(document_id, chunk_hash)`, FULLTEXT on `text` (analyzer `prose`, BM25,
highlights) and HNSW on `vector` (768, cosine, F32, EFC 200, M 16). `None` fields are
left out when written (`option<…>` takes `NONE`, not JSON `null`). `trace` is SCHEMAFULL
with ids `trace:[kind, at, ulid]` (`context::TraceRow` minus `id`), read and deleted by
record range (`Store::range`, `Store::delete_range`); `trace_hourly` and `trace_daily` are
table views. The analyzer, the two search indexes and the views are applied with
`OVERWRITE` once per change of their text (fingerprint in `schema_meta:indexes`).
`Store::insert_many` writes at most `db::INSERT_MANY_MAX` (64) rows per statement.
`SettingsRow` gains `pipeline`, `trace_retention_days` and `migrations`.

R1 shared types: `Settings.pipeline: pipeline::PipelineConfig` and
`Settings.trace_retention_days` (+ patch fields; an unknown stage name is 400
`bad_pipeline`); `Citation.document_id`, `start_line`, `end_line`;
`Event::DocumentChanged { document }`, `Event::Trace { row }` (only to `?traces=1`
sockets); `context.rs`: `Project`, `Document` (with `generation`, `actor`, `access`),
`Origin`, `Modality`, `Scope`, `ContextQuery` (with the caller's `access`), `Hit`,
`DocumentOutline`, `ContextIngest`, `ContextChanges`, `TraceRow`, `TraceRollup`.

L1 (`principal.rs`, re-exported at the crate root): `PrincipalId` (`person/<id>`,
`agent/<id>` or a group path), `person_id`, `agent_id`, `is_group_path` (1–8 segments of
`[a-z0-9-]{1,40}`, the first `company`), `ancestors`, `is_handle`, `handle_for_login`,
`ROOT_GROUP`, `AGENT_TOKEN_PREFIX` (`mga_`), `MENTION_RECIPIENTS_MAX` (50, L3),
`INVITATION_DAYS` (30); `OrgRole { Owner, Admin, Member }`, `PersonState { Invited,
Active, Disabled }`, `Person { id, github_login, github_id, handle, name, avatar_url,
org_role, state, groups, settings, created_at }`, `PersonSettings { default_provider,
reasoning_effort }`, `PersonPatch`, `Group { path, name, source, members, created_at }`,
`NewGroup`, `GroupPatch`, `AgentKind { Margin, External, Voice }`, `Agent { id, owner_id,
name, kind, ceiling, disabled, token: Option<AgentTokenInfo>, created_at }`, `NewAgent`,
`AgentPatch`, `AgentToken { token, agent }`, `Invitation`, `NewInvitation`; the API codes
`access_code::{ADMIN_REQUIRED, DEVICE_REQUIRED, AGENT_NOT_ALLOWED, NOT_OWNER, OWNER_FIXED,
OUTSIDE_OWNER, BAD_ACCESS, UNKNOWN_GROUP, HAS_CHILDREN, HAS_MEMBERS, ALREADY_MEMBER,
ALREADY_INVITED, MANAGED_BY_SOURCE}`. `Topic`, `Lesson`, `Note`, `Chat`, `Source`,
`Workspace`, `Proposal` gain `owner: Option<Id>`, `access: Vec<String>`, `deny:
Vec<String>` (all `#[serde(default)]`; read empty, `access` means private to `owner`);
`Document.owner`; `TraceRow.principal`; `Device.person_id`; `Me { github_login, device:
Option<Device>, person: Option<Person>, agent: Option<Agent> }` (`device` is `Some` for a
device token, so the wire is unchanged for today's apps); `Event::{PersonChanged,
GroupChanged, GroupDeleted, MembershipChanged, AgentChanged, AgentDeleted,
InvitationChanged, InvitationDeleted, AccessChanged}`; routes `PEOPLE`, `person`,
`person_groups`, `ME_SETTINGS`, `INVITATIONS`, `invitation`, `GROUPS`, `group`, `AGENTS`,
`agent`, `agent_token`. `margin_agent::TurnInput.actor` (`person/<id>` or `agent/<id>`).

R2 shared types (`docs/r2-brief.md`, all additive): `WrittenWith { prompt_version,
pipeline_hash, provider, model, at }` on `Lesson.written_with` and
`Proposal.written_with` (`None` before R2 and while pending; regenerating overwrites
it); `PipelineConfig.rewriter: StageCfg` (default `"none"`, left out of the JSON and the
hash while default, so pre-R2 hashes keep their meaning; `STAGE_KINDS` is 8 with
`"rewriter"` after `"embedder"`); `pipeline::RetrievalPurpose { Lesson, Plan, Agent,
Context }`; `EmbeddingModel.reranker: Option<RerankerModelState { name, repo, state }>`;
`context::PromptInfo { job, version, constant_hash, constant_chars }`; `TraceRow` gains
`queries`, `reranked`, `reranker` and `RetrievalLegs.rewrite_ms`. Service seams
(`pipeline.rs`): `Reranker::rerank(&RerankRequest { query, purpose, provider, trace_id,
job_id }, hits)`, `Rewriter::rewrite(&RewriteRequest { purpose, provider, trace_id,
job_id, scope_names }, plan)`, `SmallModels`/`SmallModel`, `StageDeps { store, models,
tracer }` (`StageDeps::new(store)` has neither), `retrieval::Request { purpose, provider
}`, `Generator::{rerank, rewrite_queries}` (defaulted).

Title (owner decision): set from the first message, its first line cut to
`CHAT_TITLE_MAX` (60) characters (`title_by: FirstLine`; the app shows it in placeholder
style). After the chat's first `Finished` turn, while `title_by` is still `FirstLine`,
`turns` spawns a side task: one `ModelClient::stream` call, no tools, reasoning effort
low, at most `TITLE_MAX_OUTPUT_TOKENS` (1024) output tokens (reasoning tokens count
toward the cap, so it leaves the model room to reason; the title is kept short by the
instructions and the cut below), given the first user message and the final answer text
("at most 6 words, no quotes"); the reply, trimmed and cut to `CHAT_TITLE_MAX`, is
stored with `title_by: Model` → `ChatChanged`. It is not a tool and is not stored as an
item. If the call fails (or a `ScriptedModel` has no reply left), it logs and the
first-line title stays; it is tried at most once per chat per service start. The call
goes to `Turns.title_model` when set (tests point it at a scripted model of its own, so
the title doesn't take a reply queued for a later turn), else the agent's model; its
usage counts in the chat's usage footer. `PATCH /api/chats/{id}` with a title sets `title_by: Owner`; the
model never replaces an `Owner` title. The sidebar's new-chat row stays "New chat".

## 4. Lifecycle of a turn

`→` is a broadcast event. "Store" always happens before the event.

### 4.1 Starting

`POST /api/chats {message}` or `POST /api/chats/{id}/messages` on a chat that is not busy
(Idle, Interrupted, Failed). Message text: trimmed, not empty, at most
`chats::MAX_MESSAGE_BYTES`; source ids must exist (else 400). Under `write_lock`:

1. Store the `UserMessage` item (`Done`, new `turn_id`, model items =
   `margin_agent::user_message_input(text, ctx)`) → `ChatItemChanged`.
2. Chat: `state = Running { turn_id, device_id: sender, started_at }`, title if first,
   `source_ids` += the message's → `ChatChanged`.
3. Spawn `turns::run`; answer `MessageSent { chat, item: Some(user item) }`.

On a busy chat (Running, WaitingForUser — owner decision: a message sent while a
confirmation is pending is queued too) the message becomes `chat.pending` →
`ChatChanged`; answer `MessageSent { item: None }`; 409 `already_pending` if one exists.

`turns::run` waits for one of `MAX_CONCURRENT_TURNS` (2) slots (the chat already shows
Running; a turn waiting on a confirmation lets its slot go and takes one again once
answered), loads the AI config (a missing key or model ends the turn like a failed model
call: Error item with the `no_api_key`/`no_model` message and that `code`,
`retryable: true`), builds
`TurnInput` (agent model = `Settings.agent_model` else `openai_model`; reasoning effort;
`history = replay_input(items)`; the chat's workspaces) and calls `run_turn` with
`TurnDeps { model: generator.agent_model(ai), tools, sink: ServiceSink, devices:
state.devices, retry: RetryPolicy::default() }`.

### 4.2 One step (model call)

**Per provider (providers phase).** The loop builds one neutral `ModelRequest` (`opts:
CallOptions { route: Chat, cache_key: chat_id, .. }`, `system`, `history`, `tools:
Vec<ToolDef>`, `tool_choice: Auto | None | Allowed(names)`, `web`, `effort`,
`compaction`, `parallel_tool_calls`) and consumes neutral `ModelEvent`s (`Started`,
`Delta`, `Done { item: ModelItem { stored, view } }`, `Completed { usage, model,
stop_reason, next }`, `Incomplete`, `Failed`); `stored` is what goes into `model_items`,
verbatim. The model is the provider config's slot for the route: nothing in the request
overrides it. The Responses rendering is below (`margin_ai::responses_request`); the
Messages rendering (Claude) is `docs/agent-providers-brief.md` D2: `system` cached, every
tool constant for the chat's life with `strict: true`, `Allowed` sent as `auto` (D5),
`thinking: adaptive` with `display: summarized` and `block_binding.prefix_mismatch_behavior:
drop_block`, `output_config.effort`, betas `compact-2026-01-12,thinking-binding-controls-2026-08-01`,
web tools `web_search_20260318` / `web_fetch_20260318`. On ChatGPT the agent sends
`parallel_tool_calls: false` (or caps the calls it runs per step): the SIWC spike saw one
`allowed_tools`/required response return 128 calls of one tool
(`docs/agent-siwc-spike.md`).

Request (Responses): `input = [developer system_prompt()] + history`; `tools =
registry.definitions()` (same every call); `parallel_tool_calls: true`; `reasoning =
{effort, summary: auto}`; `include_encrypted_reasoning`; `prompt_cache_key = chat_id`;
`tool_choice`:
- `"none"` on step `MAX_STEPS` (40) or once the turn's tool output reached
  `TURN_OUTPUT_MAX_BYTES` (512 KB);
- else `allowed_tools` (mode auto) listing every tool except the folder tools when the
  chat has no workspace (from phase 4), and except app-control tools when the origin
  device is offline (from phase 3);
- else `"auto"`.

Streaming, per output item, in stream order:

| Stream event | Loop does | Events |
|---|---|---|
| `OutputItemAdded` reasoning | `add_item(InProgress, Reasoning { summary: "" })` | `ChatItemChanged` |
| `ReasoningSummaryTextDelta` | `delta` | `ChatItemDelta` |
| `OutputItemAdded` message | `add_item(InProgress, AssistantText { text: "" })` | `ChatItemChanged` |
| `OutputTextDelta` | `delta` | `ChatItemDelta` |
| `OutputItemAdded` function_call | `add_item(InProgress, ToolCall { call_id, name, arguments: "", label: "", device: None })` | `ChatItemChanged` |
| `FunctionCallArgumentsDelta` | `delta` | `ChatItemDelta` |
| `OutputItemDone` reasoning/message | `update_item(Done, final text/summary, model_items: [item])` | `ChatItemChanged` |
| `OutputItemDone` function_call | `update_item(still InProgress, arguments, label = tool.label(args), device` (Device tools: the workspace's device, or the origin device for app control)`, model_items: [item])` | `ChatItemChanged` |
| `OutputItemDone` of another type (e.g. `compaction`) | `add_item(Done, Action { text })` with `model_items: [item]`; text "Earlier messages were condensed." for compaction, "" otherwise (the app hides empty actions) | `ChatItemChanged` |
| `Completed { usage }` | `add_usage` | `ChatChanged` |

A message whose content is a refusal is shown as its `AssistantText` (the refusal text).

No function calls in the response: the turn is finished (`TurnOutcome::Finished`).

### 4.3 Running the calls

Unknown tool name or arguments that don't parse: `ToolResult { ok: false }` with the
error output, no run. Otherwise:

- Read and Device calls of the step run concurrently; then Write and Destructive calls
  one at a time in the model's order.
- Before a Write/Destructive call: `confirmation()`. `Some(prompt)` (always for
  Destructive):
  1. `add_item(Done, Card { call_id, card: Confirmation { …, decision: Pending } })`
     → `ChatItemChanged`;
  2. `wait_for_decision(card)`: the service sets `WaitingForUser { item_id }` →
     `ChatChanged`; on `POST confirm` it stores the decision on the card →
     `ChatItemChanged`, sets `Running` → `ChatChanged`, resolves;
  3. Approved: run the call. Denied: `ToolResult { ok: false, summary: "Declined" }`
     with output `{"error":"The owner declined: <title>"}`; the loop carries on (the
     model acknowledges).
- After each call, in this order: `add_item(Done, ToolResult { call_id, ok, summary,
  output_bytes })` with `model_items: [function_call_output(call_id, output)]` and
  `full_output` → `ChatItemChanged`; `update_item(ToolCall, Done)` →
  `ChatItemChanged`; one `add_item(Done, Card { call_id, card })` per card →
  `ChatItemChanged` each.
- Output size: `truncate_output(output, TOOL_OUTPUT_MAX_BYTES)` (64 KB). The turn
  counts output bytes, reserving 64 KB for each call from when it starts until its result
  is recorded (so calls running at once can't pass the budget together); past 512 KB
  further calls get `{"error":"This answer has used up its tool output budget; answer
  with what you have."}` without running.
- Results are recorded, and outputs appended to the history, in the model's call order
  (a Read call's result waits for the Write calls before it), so the stored items replay
  exactly as sent; then the next step.

Device tools call `DeviceBridge::call(device_id, DeviceCall { timeout })` with
`FOLDER_TOOL_TIMEOUT` (60 s) or `APP_CONTROL_TIMEOUT` (20 s). `Offline`/`TimedOut` give an error output naming the device, e.g. "iPhone isn't
connected to Margin right now (the app must be open to read the folder)."; `Failed(msg)`
gives `msg`.

### 4.4 Ending

- Finished → service: `Idle` → `ChatChanged`; then the pending message, if any, starts
  the next turn exactly as in 4.1 (`pending = None` in the same `ChatChanged` as
  `Running`).
- Failed → the loop already stored `Error { message, retryable, code }` → `ChatItemChanged`;
  service: `Failed { turn_id, error }` → `ChatChanged`; then the pending message, if
  any, starts the next turn (the Error item stays; Retry is offered only while the chat is
  `Failed`).

### 4.5 Model errors and retries

- `stream()` returning `Err(e)` with `e.is_retryable()` (429, 5xx, network), or a
  stream ending in a retryable `Err` **before any `OutputItemAdded` of this attempt**:
  wait `retry_after` if given, **uncapped** (owner decision: honoured as the server asks;
  plus 0–1 s of jitter), else `base_delay · 2^n` capped at `max_delay`
  (`RetryPolicy::delay`); retry up to `retries` (3) times. Then fail.
- Before every such wait the loop calls `sink.retrying(until, reason)`: the service sets
  `Chat.waiting = Some(Waiting { until, reason })` → `ChatChanged`, and the chat shows
  it's waiting (until when, why, with Stop). When the wait ends the loop calls
  `sink.clear_waiting()` (`waiting = None` → `ChatChanged`); the service also clears it
  when the turn ends in any way (finished, failed, stopped, restart).
- Anything after output started, `Failed`/`Error` events, non-retryable errors: no retry.
  Items of the attempt still `InProgress` → `update_item(Cancelled)` (no model items);
  each completed `function_call` of the attempt gets `ToolResult { ok: false, summary:
  "Not run" }` with output `{"error":"Not run: the response failed before it
  finished."}`; then the Error item (`retryable` = whether Continue may help: true for
  retryable errors that ran out of retries, `Incomplete`, network; false for 4xx other
  than 429; `code` from `error_code`: `rate_limited`, `server_error`, `network`,
  `context_too_long`, `incomplete`, `bad_request`, … — every Error the loop or the
  service stores carries one).
- Providers (`docs/agent-providers-brief.md` D7): a refused credential (OpenAI 401/403;
  Claude 401 `authentication_error`, 403 `permission_error`, 402 `billing_error`) is
  `auth_failed` (owner decision, not `no_api_key`); Claude 429 with
  `details.error_code: enforced_spend_limit_reached` is `spend_limit_reached`; ChatGPT's
  `subscription_sharing_usage_limit_exceeded` / `_usage_unavailable` (HTTP 429 or
  `response.failed`) is `plan_limit_reached` (untested against the live API: the spike's
  step 5 couldn't run), `subscription_sharing_user_not_eligible` is `plan_ineligible`.
  None of the four is retried; each offers Open Settings → that provider. Claude 529
  `overloaded_error`, 500 and 504 are `server_error` and retried; `AiError::Http.code`
  carries the provider's code (`error.details.error_code`, else `error.code`, else
  `error.type`).
- **Job calls** (phase 5, `docs/phase5-brief.md` D4): every `TextModel` call the service
  makes (lessons, scenes, the planner, titles, condense) goes through
  `margin_ai::Retrying` with the same `Backoff` as the loop: a call that fails before
  any delta with a retryable error is retried up to 3 times, `Retry-After` honoured
  uncapped plus 0–1 s, else `2 s · 2^n` capped at 60 s; the codes above are never
  retried. Each wait is reported (`on_wait(Some(Waiting))`, then `on_wait(None)`): a
  lesson shows it in its progress text, a video in its log line, the planner on the
  proposal; the title and condense calls wait silently. Each attempt is its own
  `model_call` trace row (`ok: false`, `error_code`). Stop and deletes abort a job's
  wait as they abort the job.
- `Incomplete { reason }` (`max_output_tokens`, `max_messages`, `content_filter`,
  `steered`): items done so far are kept; Error "The answer was cut off (…)"
  `retryable: true`, code `incomplete`.
- A `SinkError` ends the turn as `Failed { error }` (the Error item is attempted; if
  storing it fails too, the service still sets `Failed`). After a `SinkError` or a panic
  the service first finishes what the turn left: `InProgress` items → `Cancelled`, each
  unanswered call → `ToolResult { ok: false, summary: "Not run" }` with output
  `turns::CALL_FAILED`, a pending confirmation → `Expired`; no `Action`.

### 4.6 Stop

`POST /api/chats/{id}/stop` (any device) on a busy chat; 409 `not_running` otherwise.
`turns::stop` runs step 1 under `write_lock`, waits for step 2's writes without it (the
library changes take `write_lock` themselves), then does steps 3–6 under it again. A
second Stop during the wait answers with the chat as it is (`Turns.stopping`); the chat
stays `Running` until the wait ends:

1. Abort the task (`run_turn` is cancel-safe; pending `DeviceBridge` calls send
   `ToolCancel`). Jobs started by tools (lessons, videos) carry on, and so do library
   changes a Write tool already started: `agent_tools::to_the_end` registers each one
   with `Turns::register_write(chat_id, call_id, handle)` before it runs.
2. `Turns::take_writes(chat_id)` and await those handles, bounded (15 s in all). Each
   that finished is recorded with its **real** result: `ToolResult { ok, summary }` with
   its output → `ChatItemChanged`, the `ToolCall` → `Done`, its cards → `ChatItemChanged`
   (so the model doesn't make the change again). One that doesn't finish in time:
   `ToolResult { ok: false, summary: "Stopped" }` with output `{"error":"Stopped before
   this change was confirmed; check whether it took effect."}`. (A write parked on a
   confirmation never started; it is handled in step 4.) Whatever way a turn ends, the
   service takes the chat's registered writes, so none outlive the turn.
3. Items of the turn still `InProgress` → `Cancelled` → `ChatItemChanged` each.
4. For every remaining `unanswered_calls` entry: `ToolResult { ok: false, summary:
   "Cancelled" }` with `cancelled_call_output` → `ChatItemChanged`. A pending
   confirmation card → `Denied { device_id, by_stop: true }` → `ChatItemChanged`.
5. `Action { text: "Stopped by the owner." }` with `model_items: [action_input(text)]`
   → `ChatItemChanged`.
6. `Idle` (and `waiting: None`) → `ChatChanged`; then the pending message, if any,
   starts the next turn.

### 4.7 Continue

`POST /api/chats/{id}/continue` when `Interrupted` or `Failed`; 409 `cannot_continue`
otherwise. Same `turn_id`; `Running { device_id: the turn's original device }` →
`ChatChanged`; `run_turn` with the replayed history and a fresh step budget. The
history already ends validly (user message or function call outputs), so the next model
call simply goes on.

### 4.8 Pending message

At most one (`Chat.pending`). Sent when the busy turn ends while the service runs
(finished, failed, stopped), as a new turn whose origin device is the pending message's
device. `DELETE /api/chats/{id}/pending` drops it → `ChatChanged`. Kept across a restart:
an `Interrupted` chat keeps its pending message; Continue resumes the turn and the pending
message goes after it. A new message on an `Interrupted`/`Failed` chat that has a pending
message: 409 `already_pending`.

### 4.9 Recovery (service start, before serving)

`turns::recover` from `server::recover`: every `Running`/`WaitingForUser` chat →
`Interrupted { turn_id }`; its `InProgress` items → `Cancelled`; each unanswered call →
`ToolResult { ok: false, summary: "Interrupted" }`, output `{"error":"The service stopped
before this call finished; check whether it took effect before trying again."}`; a
`Pending` confirmation → `Expired` plus `ToolResult` output `{"error":"The service
restarted before the owner answered."}`; then `Action "The service stopped while
answering."`. No events (nobody is connected yet).

### 4.10 Delete

`DELETE /api/chats/{id}`: stop a busy turn (steps 1–4 of Stop, without the Action or the
pending message), delete items, delete sources whose only attachment is this chat
(`topic_id == None` and no other chat lists them), → `ChatDeleted`.

### 4.11 Replay

Replay is per provider (`replay::openai`, `replay::claude`; `docs/agent-providers-brief.md`
D4). A chat stays on the provider it was created on (`Chat.provider`) and its
`model_items` hold that provider's wire items verbatim; nothing stored is rewritten. On
Claude a stored entry is `{"role", "block"}` (one content block) and `replay::claude::messages`
regroups blocks into messages deterministically (same items → same prefix, what preserved
thinking checks); an `Action` is a user-role `Margin: …` note. What follows is the
Responses replay (OpenAI, ChatGPT).

`replay_input` = each stored item's `model_items` in `seq` order, plus the safety net for
unanswered calls. Anything stored between a step's `function_call`s and their outputs (an
`Action` recorded while the turn waited on a confirmation) is replayed right after those
outputs, so nothing sits between a call and its output. What stores which model items:

| Item | model_items |
|---|---|
| `UserMessage` | `user_message_input(text, ctx)`: one user-role message whose first `input_text` part is the context (date, device, attached folders with online state, attached files with ids and statuses; names only as JSON strings marked as data, no warning or error text) and whose second part is what the owner typed. The context is in the user role, not a developer message, because developer text carries instruction authority and the names are untrusted. Built once, so replays are byte-identical and cache-friendly (messages stored by older builds keep their developer context message as stored). |
| `Reasoning`, `AssistantText` | `[output_item.done item]` (reasoning carries `encrypted_content`) |
| `ToolCall` | `[output_item.done function_call]` |
| `ToolResult` | `[function_call_output]` |
| `Action` | `[action_input(text)]`, or `[compaction item]` |
| `Card`, `Error`, cancelled partial output | none |

### 4.12 What a device sees

A device that opens a chat calls `GET /api/chats/{id}` (the service overlays the live
text of in-progress items from the sink's memory) and applies item events for loaded
chats only. `ChatItemDelta.offset` lets it skip deltas it already has and detect a gap
(`ChatItemKind::apply_delta`); a gap fills in at the item's next `ChatItemChanged`.
Tool rows fetch `GET /api/chats/{id}/items/{item}/output` when expanded (owner decision
Q3: excerpts load on demand; nothing but the summary is pushed).

The composer: busy chat → "Will send when the answer finishes" with the pending text and
a cancel button; Stop visible while busy; Continue shown for `Interrupted`, Retry on the
last Error item while `Failed`.

## 5. Tools

Arguments are `strict` JSON schemas from `schemars` (`Option` fields are required and
nullable). Outputs are compact JSON text unless noted. `summary` is the tool row's line.

### Providers, hosted web tools

`web_search` (all providers) and `web_fetch` (Claude: `max_uses` 8, `citations` on,
`max_content_tokens` 50 000; owner decision: on, no per-chat switch) are the provider's
hosted tools, not registry tools: a `ToolCall`/`ToolResult` row pair named
`web_search`/`web_fetch` (icon `globe`), a `Card::WebResults` after a search, and
`AssistantText.citations` (`WebCitation { url, title, cited_text, start, end }`) shown
as a numbered Sources list. Not counted against the tool output budget; never in
`unanswered_calls`. `encrypted_content` never leaves the service.

### Phase 1, Read (service)

| Tool | Arguments | Output / card |
|---|---|---|
| `search_library` | `query` | `search::run` hits (kind, id, title, context, snippet) |
| `list_topics` | — | The tree: id, parent_id, title, lesson and note counts |
| `get_topic` | `topic_id` | Topic, its lessons (id, title, summary, status, video status), notes (id, title), sources (id, name, kind, status) |
| `read_lesson` | `lesson_id` | Title, topic path, status, `body_md`, citations (n, source, page/time) |
| `read_note` | `note_id` | Title, topic, lesson, quote, `body_md` |
| `list_notes` | `topic_id: Option` | id, title, topic, lesson, updated_at |
| `search_sources` | `query`, `topic_id: Option`, `source_ids: Option<Vec>` | `sources::passages` (scope: the ids, else the topic's sources, else the chat's files plus every source); card `Passages` |
| `list_sources` | `topic_id: Option` | id, name, kind, status, topic |

### Phase 2, Write and Destructive (service)

| Tool | Kind | Arguments | Notes |
|---|---|---|---|
| `create_topic` | Write | `title`, `parent_id: Option` | Result card |
| `rename_topic` | Write | `topic_id`, `title` | |
| `move_topic` | Write | `topic_id`, `parent_id: Option` (null = root), `position: Option` | `position` is the index among the new siblings (0 first, null last); siblings are renumbered 0, 1, 2… |
| `edit_lesson_title` | Write | `lesson_id`, `title` | |
| `create_note` | Write | `topic_id`, `lesson_id: Option`, `title`, `body_md` | Agent-written; Result card |
| `append_to_note` | Write | `note_id`, `text` | Never asks (Q2) |
| `replace_note_body` | Write | `note_id`, `body_md` | Asks when the note is the owner's writing (Q2); an empty body has nothing to overwrite and never asks |
| `propose_course` | Write | `prompt` (the learner's request), `topic_title`, `subject_title`, `lessons: [{title, summary, minutes}]` (1–12), `level`, `with_videos`, `with_quiz`, `parent_topic_id: Option`, `source_ids` | The model writes the outline; the service validates it like `ask::validate_outline`, stores a `Ready` `Proposal` with `chat_id`, card `Outline`. Options the owner didn't state are inferred; the chips on the card change them. |
| `accept_proposal` | Write | `proposal_id` | Same as Create lessons with no edits; Result card for the topic. Only a proposal of the same chat; its result is the chat's record (no extra `Action`). A superseded proposal is refused (the model is told which newer one replaced it) |
| `delete_topic` / `delete_lesson` / `delete_note` | Destructive | the id | Prompt names what goes with it (lessons, notes, videos, sources) |

Q2 provenance (service-only fields `StoredNote.author`, `StoredLesson.author`:
`state::Author::{Owner, Agent}`): a note created by an agent tool is agent-written; a
note created through `POST /api/notes`, or whose body was changed through
`PATCH /api/notes/{id}`, is the owner's (a retitle alone keeps the body's author). A
lesson whose body was changed through `PATCH /api/lessons/{id}` is owner-edited; a lesson
job that writes the body makes it the agent's. `replace_note_body` on the owner's note
and `regenerate_lesson` on an owner-edited lesson ask first; once replaced (the owner
approved), the note is the agent's writing (owner decision). Appending keeps the author. Rows without the field read as the
owner's (safe default).

Superseding: `propose_course` marks every earlier `Ready` proposal of the same chat
`ProposalStatus::Superseded { by: <the new proposal id> }` (→ `ProposalChanged` each),
under `write_lock` with storing the new one. Accepting a superseded proposal (route or
tool) answers 409 `superseded` ("A newer outline replaced this one."); its outline card
renders inactive with that line, so only the newest outline in a chat can create a
course.

Create lessons on the outline card: `POST proposal_accept` with the edited outline and
`chips`; on success the app opens the course's first lesson (owner decision), while the
chat keeps the course card; when the proposal has a `chat_id`, the service adds
`Action "Created the course “<topic>” with N lessons."` (with `action_input`) to that chat
→ `ChatItemChanged`, so the next turn knows. Accepting while that chat's turn is busy is
fine: the action takes that turn's id and lands between items; replay moves it after the
outputs of calls still open when it was stored (4.11). Accepting through the
`accept_proposal` tool adds no `Action` (its result is the record).
A proposal whose `parent_topic_id` was deleted after `propose_course` is accepted under
`outline.subject_title` instead (an existing subject of that title, else a new one; the
stored request's `parent_topic_id` is cleared). The outline card then shows the Subject
field, prefilled from the outline, with a notice that the named subject was deleted;
an empty subject keeps Create lessons from going, like an empty topic title.

`POST /api/ask` (`routes::ASK`), the proposal job and `margin_ai::propose_outline` with its
prompt are gone; the model writes the outline in `propose_course`. `AskRequest` stays as
`Proposal.request` (what the course was proposed with; the chips start from it and
`AcceptProposal.chips` overrides it). Proposals left `Drafting` by an older service are
failed at startup.

### Phase 3 (service and device)

| Tool | Kind | Arguments | Notes |
|---|---|---|---|
| `write_lesson` | Write | `topic_id`, `title`, `summary`, `minutes`, `level`, `with_quiz` | Appends a lesson and starts its job; Result card (lesson) |
| `regenerate_lesson` | Write | `lesson_id` | Asks when owner-edited; Result card |
| `make_video` | Write | `lesson_id` | `video::enqueue`; Result card (video) |
| `attach_sources` | Write | `source_ids`, `topic_id` | Moves chat files to a topic |
| `navigate` | Device | `to: NavTarget` | To the origin device |
| `open_citation` | Device | `lesson_id`, `n` | |
| `show_passage` | Device | `source_id`, `page: Option`, `start_ms: Option` | |
| `toggle_sidebar` | Device | `visible: Option<bool>` | No appearance tool (owner decision) |

Result cards render live state from the library (a lesson generating, a video
rendering) through the existing events.

`write_lesson` stores `StoredLesson.prompt` (the request of the course the topic came
from, if any) and `with_quiz` (both service-only, serde default) so the lesson writer
has them without a proposal; the lesson is written without a video (`first_write`
false). `attach_sources` takes only files attached to this chat and not yet in a topic;
they stay in `Chat.source_ids` and, now in a topic, survive the chat's deletion.

### Phase 4, folders (`docs/agent-phase4-brief.md`, adjusted by the RAG doc §2.7, §6.6)

A folder's allow-listed files (prose documents, PDFs, images; `folder_file_type`) are
copied to the service as they change and ingested there like sources; nothing is read
from a device on demand. The model's folder tools (`FOLDER_TOOLS`) are `list_files`
and `find_files` (service `Read` tools over the folder's rows and sources) and
`sync_folder` (a device tool: `DeviceToolCall::SyncFolder`, answered `Done` with the
plan's counts); a folder's text is searched with R1's `search` scoped to the workspace.
The workspace must be one of the chat's. A file the device lists but can't read (evicted
to iCloud, locked) is sent in `SyncManifest.unreadable` with its reason; the service
keeps what it has of it and records the list on `Workspace.unreadable`, which the
Folders pane shows greyed out. No per-file size cap and no picture cap (owner); `UPLOAD_PART_BYTES` still
bounds one request. The device-side `ListFiles`/`FindFiles`/`SearchText`/`ReadFile`
calls and their outputs stay in the protocol for older services; the app answers them
with an error (`tools::READ_ON_SERVICE`: the folder's files are on the service).

### R1, the retrieval tool family (service, replaces `search_sources`)

`search {query, scope: {topic_id?, source_ids?, workspace_ids?, chat_files?, library?},
k ≤ TOOL_SEARCH_K_MAX, modalities}` → hits with `n`, locator, `section_path`, text cut to
`TOOL_OUTPUT_PASSAGE_CHARS`, score, and a Passages card; default scope this chat's files
and folders, then its topic (never every source). `read_passage {document_id, page? |
ord range}` → up to `TOOL_READ_PASSAGES_MAX` adjacent passages. `document_outline
{document_id}` → section paths with page/line ranges. `view_picture {document_id, page?}`
→ the stored picture, counted against the turn's picture budget. Each output ≤ 16 KB;
`search_library` keeps its name and runs on the FULLTEXT index.

### L1, what is and isn't enforced (`docs/l1-brief.md` D6)

- **List routes** return the principal's slice: `GET /api/library` with `WHERE doc.access
  CONTAINSANY $paths` on each table, `GET /api/chats`, `GET /api/workspaces`, `GET
  /api/workspaces/{id}/files`, `GET /api/search`. `deny` is written and read back; it is
  not evaluated until L2.
- **Item routes** answer 404 outside the principal's access; writing (patch/delete) also
  needs `owner == principal.person_id` (403 `not_owner`); admins are not exempt.
- **Chat controls:** stop and continue by anyone in the chat's audience; confirm only by
  the chat's owner.
- **Settings:** `PATCH /api/settings` admin-only; `GET` for anyone.
- **Traces:** `GET /api/traces`, `/api/traces/rollup` and `?traces=1` admin-only; every
  trace row gets `principal`.
- **Retrieval plumbing, not enforcement:** the agent's `search` and the context API pass
  the principal's paths; a caller-supplied `access` that isn't a subset is 400
  `bad_access`.

**Accepted L1 gaps (closed by L2):** agent tools that read a library row by id
(`get_topic`, `read_note`, `list_sources`, …) still take any id; `read_passage`/
`document_outline`/`passages` by `document_id` likewise; passages all say `company` so a
search by B finds A's private note's passages. `GET /api/context/changes` lists every
document's metadata (name, origin, owner, generation; not the text) to every principal,
whatever its access, agent tokens included. Documents from the context API are keyed by
agent name and document name across the whole organisation: when B's agent has the
same name as A's (both `claude-code`, say) and ingests a document with the same name as
one of A's, the ingest is refused with 403 `not_owner` instead of being kept as B's own
document (B renames the agent or the document). A second *trusted* person (a tester) is
fine on L1; a second *untrusted* one is not until L2 ships (plan §8.3 still stands for
untrusted accounts).

**Disabling a person** deletes their devices (and closes their sockets), so their next
request is 401 `unauthorized`; 401 `person_disabled` (with "The GitHub account @x is
disabled on this Margin. Ask an admin.") is answered only in the moment before the
devices go, and the app shows that sentence when it gets it. The sentence is promised
only at sign-in: a disabled person's GitHub callback is refused with it.

## 6. Device tool transport

- The events socket registers with `DeviceHub::register` on open (`DeviceChanged` with
  `online: true` when it is the device's first socket) and `unregister` on close
  (`DeviceChanged` offline when the last closes). The socket task also forwards
  `DirectedFrame`s from the receiver and hands incoming `DeviceFrame::ToolResult` text
  frames to `DeviceHub::deliver`. Anything else a device sends is ignored.
- `DeviceHub::call` sends `ToolRequest { request_id, chat_id, call_id, deadline, call }`
  to the newest socket and waits; on timeout or drop it sends `ToolCancel`.
- The app parses incoming frames as `ServerFrame`; `Directed` frames go to the device-tool
  runner, which answers with `DeviceFrame::ToolResult` before the deadline and abandons
  work on `ToolCancel`. Folder tools check that the workspace is this device's.
  One task owns both halves of the app's socket; the runner sends through
  `AppCtx.device_frames` (an unbounded channel per connection, `None` while
  disconnected). Each request is its own task keyed by `request_id`, waiting until the
  deadline but at least 5 s (the device's clock may run ahead). App-control calls are
  queued on `AppCtx.agent_commands` (`AgentCommand { call, chat_id, reply }`) and run by
  the shell on the main thread; an unknown id answers `Err`. `show_passage` outside a
  lesson uses `AppCtx.shown_passage`. On iOS and Android the app closes the socket when
  it goes to the background (`LiveStatus::Background`) and reconnects on return.
- Revoking a device: `DeviceHub::forget_device`, its workspaces deleted (detached from
  chats → `ChatChanged`, `WorkspaceDeleted`), `DeviceDeleted`.
- Phase 4 calls to the holding device: `SyncFolder { workspace_id }` (walk, `POST` the
  manifest, answer `Done`, upload in the background), `Manifest { workspace_id }` →
  `DeviceToolOutput::Manifest { files }` (every allow-listed visible file with its
  stamp; the hash is computed on the device), `FetchFile { workspace_id, path, stamp }`
  (upload that file now through `SOURCES`/`SOURCE_UPLOADS` with `workspace_id` and
  `modified`, answer `Done`; a stamp that no longer matches is `Err`; no size limit).
  `workspace_id()` names the folder for all three; the app answers them from its sync
  engine (`folders::sync`).

### R1, the context API (device token today; per-agent tokens later)

`GET /api/projects`; `POST /api/context/search` (`ContextQuery` → `Vec<Hit>`, 400
`bad_scope`); `GET /api/context/documents/{id}/outline` and `…/passages?from&to` (404,
409 `not_ready`); `POST /api/context/ingest` (`ContextIngest` → `Document`, 400
`bad_origin`; `actor = agent:<name>`); `GET /api/context/changes?since&limit` (one page,
by generation: `limit` 1-500, default 100, 400 outside; `more` while others follow,
and then `generation` is the page's last; margin-mcp reads every page and its
`recent_changes` lists the latest 50, counting the rest); `GET /api/version` (no
token: version, sha, environment); a search or passage read sent with
`x-margin-agent: <name>` (margin-mcp sends `claude-code`) leaves trace rows with
`actor: agent:<name>`, `via: mcp` and, for a search, the query, which
`margin-eval harvest` turns into eval candidates (`docs/plan-dogfood.md` §6);
`GET /api/traces?kind&from&to&trace_id` (JSON lines, oldest first, 400 `bad_range`);
`GET /api/traces/rollup?bucket&from&to`. The handlers call the same functions as the
agent's tools: one retrieval path for Margin's agent, external agents and the GUI.

### R2 routes

`GET /api/pipeline/prompts` → `Vec<context::PromptInfo>` (every prompt the service
sends, with its version and constant hash); `GET`/`DELETE /api/embedding/reranker` and
`POST /api/embedding/reranker/download` (answer the whole `EmbeddingModel`, progress as
`Event::EmbeddingModelChanged`, 409 `no_reranker_model` where the service offers none:
the first commit's stubs answer that until R2-xenc).

## 7. Limits (constants)

`margin_agent`: `MAX_STEPS` 40, `TOOL_OUTPUT_MAX_BYTES` 64 KB, `TURN_OUTPUT_MAX_BYTES`
512 KB, `MODEL_RETRIES` 3, `RETRY_BASE_DELAY` 2 s, `RETRY_MAX_DELAY` 60 s,
`MAX_CONCURRENT_TURNS` 2, `FOLDER_TOOL_TIMEOUT` 60 s, `APP_CONTROL_TIMEOUT` 20 s, `COMPACT_THRESHOLD_TOKENS` 200 000 (phase 5).
`margin_shared::device`: `READ_FILE_MAX_LINES` 400, `READ_FILE_MAX_BYTES` 64 KB,
`READ_FILE_MAX_BYTES_TOTAL` 2 MB, `SEARCH_MAX_BYTES` 50 MB, `SEARCH_MAX_SECONDS` 20,
`MAX_LISTED_ENTRIES` 500, `MAX_TEXT_MATCHES` 200, `MATCH_LINE_MAX_CHARS` 300,
`FOLDER_SYNC_INTERVAL_SECS` 900. Phase 4 adds no file, page or picture caps (owner).
`margin_shared::CHAT_TITLE_MAX` 60; `chats::MAX_MESSAGE_BYTES` 32 KB.
R1, `margin_shared::routes`: `RETRIEVAL_K_DENSE` 40, `RETRIEVAL_K_LEX` 40,
`RETRIEVAL_K_FUSED` 24, `RRF_K` 60, `MMR_LAMBDA` 0.7, `TOOL_SEARCH_K_MAX` 12,
`TOOL_OUTPUT_PASSAGE_CHARS` 1 100, `TOOL_READ_PASSAGES_MAX` 4;
`margin_shared::TRACE_RETENTION_DAYS` 90; `db::INSERT_MANY_MAX` 64;
`retrieval::RETRIEVAL_CACHE_ENTRIES` 256; `trace::TRACE_CHANNEL` 4 096. `Budget`
defaults: `share_of_window` 0.5, `min_score_pct` 25, `images_max` 8, `passages_min` 0
(no fixed token cap; owner Q3).
Providers, `margin_ai`: `CLAUDE_MAX_OUTPUT_TOKENS` 32 000 (a text call: 64 000),
`WEB_SEARCH_MAX_USES` 8, `WEB_FETCH_MAX_USES` 8, `WEB_FETCH_MAX_CONTENT_TOKENS` 50 000,
`MODEL_WINDOW_FALLBACK` 200 000 input / 32 000 output, `WEB_SEARCH_TOOL`
`web_search_20260318`, `WEB_FETCH_TOOL` `web_fetch_20260318`, `CLAUDE_BETAS`
`compact-2026-01-12,thinking-binding-controls-2026-08-01`; `windows::openai_window`
(GPT-6 family: 1 050 000 / 128 000). Model defaults (`margin_shared`): Claude
`claude-opus-5-5` for chats and writing, `claude-haiku-5-5` for small tasks; OpenAI small
`gpt-6-luna`; ChatGPT `gpt-6.1-sol`.
R2, `margin_shared::routes`: `RERANK_MAX_CANDIDATES` 24, `RERANK_QUERY_CHARS` 800,
`RERANK_MAX_OUTPUT_TOKENS` 400, `REWRITE_MAX_ADDED` 2, `REWRITE_MIN_WORDS` 5,
`REWRITE_MAX_OUTPUT_TOKENS` 120, `RERANK_PAIR_TOKENS` 512.

Phase 5 (`docs/phase5-brief.md` D7), body and frame limits, each a named constant with a
test at its boundary:

| What | Limit | Where |
|---|---|---|
| Source upload (one request) | 50 MB; audio and video 500 MB; folder files unlimited, in parts of 32 MB | `routes::{MAX_OTHER_SOURCE_BYTES, MAX_MEDIA_SOURCE_BYTES, UPLOAD_PART_BYTES}` |
| Folder manifest (`POST /api/workspaces/{id}/sync`) | 64 MB | `folders::manifest_limit` |
| `POST /api/context/ingest` | 16 MB | `routes::CONTEXT_INGEST_MAX_BYTES` |
| Every other JSON body | 2 MB | `routes::JSON_BODY_MAX_BYTES` (on the API router) |
| A device's frame on the events socket | 1 MB | `routes::DEVICE_FRAME_MAX_BYTES` |
| `GET /api/traces` | paged by the server (500 rows a read) | `trace::PAGE` |
| Condense summary | 8 000 characters | `routes::CONDENSE_SUMMARY_MAX_CHARS` |
| Alert job tick | 60 s | `routes::ALERT_TICK_SECS` |
| Per-principal rate limits (5-security; a person's devices share one window, an agent has its own) | messages 30/min, context search 120/min, context ingest 60/min, source and upload starts 60/min together (a folder's files, `?workspace_id=`, aren't counted: a sync sends every file of the folder); 429 `rate_limited` with `Retry-After` in seconds until the minute's window ends (`margin-mcp` waits it out); sign-in per IP: start 10/min, poll 90/min, same answer | `auth::{MESSAGES_LIMIT, CONTEXT_SEARCH_LIMIT, CONTEXT_INGEST_LIMIT, SOURCES_LIMIT, START_LIMIT, POLL_LIMIT}` through `auth::limit` |
| Signed media links | 6 h; `Cache-Control: private, max-age=<seconds left on the link>` | `state::MEDIA_URL_TTL_SECONDS`, `media::cache_control` |
| Sign-in callback page | `Content-Security-Policy: default-src 'none'` (its one stylesheet by hash), `Referrer-Policy: no-referrer`, `no-store`; a replayed callback is refused (410) | `auth::page_csp` |
| CORS | none: no route sets `Access-Control-*` (native apps and `margin-mcp` only) | — |

## 8. Tests each owner adds

- **ai**: the fake SSE server streams real event shapes (text, reasoning summary,
  parallel function calls with argument deltas, completed with usage, incomplete,
  failed, `error`, a connection that drops, chunks split mid-line and mid-UTF-8); 429
  with `Retry-After`; the request body (`response_body`) is what was sent.
- **agent**: with `ScriptedModel` and a recording fake sink: item and event order for a
  text answer, one call, parallel calls, Write ordering, confirmation approve/deny,
  retries (before output only), failure after output (cancelled items, "Not run"
  results), step cap (`tool_choice: none` on the last step), budgets, truncation on a
  UTF-8 boundary, unknown tool/bad arguments, replay validity (unanswered calls),
  byte-identical replay of the history, the strict-schema test (every registered tool's
  schema satisfies strict mode), device tools against a fake `DeviceBridge` (offline,
  timeout, error).
- **service**: through HTTP and the socket with `FakeGenerator.agent`: create chat with
  message → events and stored items; pending message flow; stop mid-stream
  (`Scripted::Hang`); continue after a simulated restart (`recover`); confirm from a
  second device; delete; item output on demand; each tool against the in-memory store;
  `DeviceHub` with a test socket answering and ignoring requests; settings fields.
- **app**: store reducer (done for the new events), `fake.rs` routes for chats,
  render tests and offscreen screenshots of the chat screen, cards, sidebar Chats, the
  Settings additions; device-tool runner unit tests; path-rule tests on each platform's
  folder access code.

Never call the real OpenAI API; never touch the running service or its data folder.

**Providers**: `docs/agent-providers-brief.md` §5 per area (fake Responses and fake
Messages servers that record bodies, a fake auth server for the SIWC flow, `ScriptedModel`
on neutral `ModelEvent`s). No test calls api.openai.com, auth.openai.com or
api.anthropic.com. The first commit has: shared round trips (a `Chat` without `provider`
is OpenAI, an `AssistantText` without `citations` parses, `Settings` without slots gets
the defaults with `agent_model` migrated into `openai_models.chat`), the Responses
adapter (`responses_request` + `to_model_events`), `AiError` codes and retries, price
parsing with the web-search price, Haiku 5.5's long-context tier and ChatGPT's `None`
cost.

**Phase 4**: the e2e tests of `docs/agent-phase4-brief.md` D13, rewritten by the
phase's integrator for the ingest path (`docs/agent-rag-architecture.md` §6.6); the
first commit has the allow-list, frame and wire-type round trips, `StdDir` against temp
dirs (listing, stat, streaming open, symlinks refused at every component), and the
folder handle and unbuilt-platform tests. Storage: chunk and media rows (and R1's
passages) carry the folder's `workspace_id` as their scope.

**R1** (first commit, already in): the SurrealQL the design relies on runs on `kv-mem`
(`db.rs` tests: HNSW nearest with scope and access pre-filters and `EXPLAIN` showing
`KnnScan` on `passage_vector`, the scope index on its own, FULLTEXT score and highlight
with stemming, `search::rrf` of two legs, trace range by kind and window, the hourly and
daily views following inserts and range deletes); `PipelineConfig` round trip, pinned
default hash, unknown params kept; `Pipeline::build` of the default and trivial configs,
an unknown stage named in the error, fakes registered under real names; settings switch
the pipeline by name; `margin-evals` metrics against hand-worked values and the golden
set's anchors against the corpus. Each R1 area adds the tests of §6.2/§6.4 of the
architecture doc; the eval gates (`cargo test -p margin-evals`) run on
`RecordedEmbedder` and `kv-mem` only.

**Phase 5**: the tests of `docs/phase5-brief.md` §5, fakes only. The first commit has:
`text::for_model` (controls, bidi, zero-width and tag characters, private use and
unassigned, NFC, blank-line runs, idempotence, ASCII/CJK/emoji untouched); round trips
of the new trace fields, `TraceKind::Alert`, `TraceRollup.p95_ms`, `Alert`/`AlertList`
with rows from before phase 5 parsing; `Backoff` (computed delays capped,
`Retry-After` uncapped with jitter bounds, non-retryable codes); `[alerts]` defaults and
overrides; `VersionInfo` without the alert fields parsing; the background
`Coordinator` over the D1 table (start in front, update, end, expiry restores phase 4,
no start in the background, a refusal not retried); the `margin-eval` sub-commands
parse; the injection rows load; `budgets.toml` parses; `margin-mcp` waits out a 429.
**R2** (`docs/r2-brief.md` §5). The first commit has: a config without `rewriter`, or
with `"none"`, hashes as before (the pinned default hash), `"small-expand-v1"`
differently; `WrittenWith`, `EmbeddingModel.reranker`, the trace fields and
`PromptInfo` round trips; the trivial `"none"` reranker and rewriter through the new
seams; `FakeReranker` scores by shared words; `prompts::catalog()` lists each job once;
the prompt lock (`crates/evals/tests/gates.rs::prompt_lock`, always on) and its checker
(same version with another hash fails). The areas add §5's e2e (`tests/r2_e2e.rs`) on
`FakeGenerator.{rerank_reply, rewrite_reply}` and `FakeReranker`; nothing calls a model
API or Hugging Face.

## 9. Open points for the owner

Listed in the build report; nothing above depends on them being answered differently.

L1 (`docs/l1-brief.md` §8; the owner accepted every recommendation): the bootstrap owner
is fixed (no ownership transfer in L1); no person switcher (Sign out → sign in); only the
owner grants or removes the Admin role and invites admins; everyone may list people and
groups (names and handles, never content); invitations expire after 30 days and can be
re-sent; traces are admin-only, `?traces=1` included; members see the org AI settings
read-only with the sentence; `margin-mcp login` (device token) stays beside `margin-mcp
token` (agent token, the one to use).
