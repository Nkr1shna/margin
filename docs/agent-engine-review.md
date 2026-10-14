## Review: Claude Code / Codex over ACP as Margin's agent engine

Everything below was verified on the live pages today (2026-10-14) by three lookups; quotes are verbatim, URLs inline, and anything the docs don't state is marked *unverified*. No files were edited.

### Headline findings that change the question

1. **The ACP world moved.** `zed-industries/claude-code-acp` and `zed-industries/codex-acp` are deprecated/archived; both now live in the `agentclientprotocol` GitHub org ("jointly governed by Zed and JetBrains", https://agentclientprotocol.com/community/governance) as `@agentclientprotocol/claude-agent-acp` (v0.90.0, 2026-10-13; **38 releases in 3 months**, 86 open issues) and `@agentclientprotocol/codex-acp` (v2.2.2, 2026-10-10; **33 releases in 3 months**, 60 open issues, rewritten in TypeScript). ACP v1 is stable, but a **v2 draft** is already breaking it: `session/load` → `session/resume`, all `fs/*` and `terminal/*` removed, `session/prompt` no longer ends the turn, `tool_call` merged into `tool_call_update` (https://agentclientprotocol.com/protocol/v2/migration.md: "gate v2 support behind explicit version negotiation and feature flags until it stabilizes"). The Rust crate had 3 major versions in 3 months (2.0 → 3.3). Neither Anthropic nor OpenAI ships ACP natively (no hits in either vendor's docs or repos).

2. **Claude on a subscription is not a permitted backend for Margin.** https://code.claude.com/docs/en/agent-sdk/overview: "Unless previously approved, Anthropic does not allow third party developers to offer claude.ai login or rate limits for their products, including agents built on the Claude Agent SDK. Use the API key authentication methods…" and https://code.claude.com/docs/en/legal-and-compliance: "OAuth authentication is intended exclusively for purchasers of … subscription plans and is designed to support ordinary use of Claude Code and other native Anthropic applications… Developers building products or services that interact with Claude's capabilities, including those using the Agent SDK, should use API key authentication… Anthropic reserves the right to take measures to enforce these restrictions and may do so without prior notice." The AUP (https://www.anthropic.com/legal/aup) prohibits "services that route requests through consumer subscriptions or misrepresent the product or client being used". The one carve-out is "an end user … signing in to the unmodified Claude Code binary with their own Claude subscription" — Margin's UI driving `claude -p`/the SDK is a third-party client, not that. The `claude-agent-acp` adapter itself ships `--hide-claude-auth`: "This integration does not support using claude.ai subscriptions." So the Claude route only works with an API key — which removes the "no API key" motivation, and with an API key you don't need Claude Code at all: the Claude API has a hosted `web_search` server tool.

3. **OpenAI has a documented, sanctioned path for exactly this use case, and it doesn't need ACP.** "Sign in with ChatGPT" token sharing for open-source and locally hosted apps (https://developers.openai.com/siwc/token-sharing-open-source): "These docs explain ChatGPT plan usage for open-source and locally hosted apps." The SIWC Terms (https://openai.com/policies/sign-in-with-chatgpt-terms/, 2026-09-29): "Requests must originate from the user's local runtime or a remote runtime only that user controls"; "Use the user's plan only for the application they connected"; "Any persistent storage of Authentication Tokens must be local and under the user's control"; no multi-account/usage-splitting. Its sub-page shows the launch line for `codex app-server` with the plan token (`-c 'model_provider="openai_chatgpt_plan"' … env_key="ACCESS_TOKEN" … wire_api="responses"`), and users cap each app's share in ChatGPT Settings → Usage ("Set its weekly limit as a percentage of your overall weekly plan usage"). That caps concern 2 by design.

---

## (1) Verified facts per concern and UX question

### Concern 1 — locking the agents down

| | Claude Code / Agent SDK | Codex CLI / app-server |
|---|---|---|
| Remove built-in tools from the model's tool list (not just block) | Yes. `--tools "WebSearch"` ("Restrict which built-in tools Claude can use… `""` to disable all"); SDK `tools: ["WebSearch"]`; `disallowedTools`: "A bare name such as `"Bash"` removes the tool from Claude's context"; `--restricted` (v2.1.248+) removes command/code tools and WebFetch. https://code.claude.com/docs/en/cli-reference, https://code.claude.com/docs/en/permissions | Partly. `features.shell_tool = false` ("Enable command execution tools (stable; on by default)", https://learn.chatgpt.com/docs/config-file/config-basic) removes shell/exec. No documented switch for `apply_patch`; file edits are blocked by `sandbox_mode = "read-only"` and `approval_policy = "never"` auto-declines escalation (*removal from the tool list unverified*). `codex mcp-server` and `--full-auto`/`untrusted` are gone (https://learn.chatgpt.com/docs/non-interactive-mode). |
| Keep web search while network is off | WebSearch is a Claude API server tool; works on "the Claude API, Claude Platform on AWS, and Microsoft Foundry", not Bedrock (https://code.claude.com/docs/en/tools-reference); region limits *unverified*. | Yes: "Web search is a hosted tool, separate from sandboxed local command networking… it can remain available when command network access is disabled" (https://learn.chatgpt.com/docs/web-search). Modes `disabled/cached/indexed/live`; `cached` "uses an OpenAI-maintained index without external web access". |
| Auto-deny the rest | `permissionMode: "default"` + `canUseTool` → deny, or `permissionPrompts: 'none'`; evaluation order "Hooks → Deny rules → Ask rules → Permission mode → Allow rules → canUseTool" (https://code.claude.com/docs/en/agent-sdk/permissions). Note: omitting `permissionMode` can now start in **`auto`** mode (v2.1.285+/SDK ≥0.3.286) — must be set explicitly. | `approval_policy = "never"` ("The agent doesn't stop for approval prompts") + read-only Seatbelt sandbox (`sandbox-exec` profile, https://learn.chatgpt.com/docs/agent-approvals-security). Over ACP, `session/request_permission` deny = select a `reject_*` option. |
| Keep the owner's own config out | `settingSources: []`, `--strict-mcp-config`, custom `systemPrompt` (string replaces everything; https://code.claude.com/docs/en/agent-sdk/modifying-system-prompts). | `CODEX_HOME` pointed at a Margin-owned dir, `-c` overrides, `--ignore-user-config` (exec); MCP servers restricted to Margin's (config `mcp_servers`, `enabled_tools`/`disabled_tools`, `tool_timeout_sec`). |

Net: both can be reduced to "web search + Margin's MCP tools"; Codex's `apply_patch` is sandbox-blocked rather than removed. Prompt injection then has two sinks: web-search queries (exfiltration of library text as a query — unavoidable with any web search) and Margin's own tools (already confirmation-gated for deletes/overwrites; the MCP handler keeps that gate).

### Concern 2 — rate limits
- Claude: "all activity in both tools counts against the same limits" (https://support.claude.com/en/articles/11145838-use-claude-code-with-your-pro-or-max-plan); 5-hour + weekly windows; SDK emits `rate_limit_event` with `resetsAt`; when exhausted "Claude Code blocks further requests until the reset time" (https://code.claude.com/docs/en/errors). Moot given finding 2.
- OpenAI: limits are "local messages per five-hour period" per model: Plus GPT-6 Astra 5–45, GPT-6.1 Sol 15–160, GPT-6 Luna 350–3,000; "Pro plans currently have no five-hour limit"; "Local messages and cloud chats share your plan's usage allowance. Weekly limits may also apply." (https://learn.chatgpt.com/docs/pricing). Shared with the owner's own Codex use — but SIWC lets the owner set Margin's weekly cap as a percentage.

### Concern 3 — terms
- Anthropic: quoted in finding 2. Verdict: not permitted for a subscription; permitted with an API key under the Commercial Terms.
- OpenAI: SIWC Terms permit it for a locally hosted app the user controls (finding 3). General Terms of Use add "You may not share your account credentials" and "circumvent any rate limits" (https://openai.com/policies/terms-of-use/). Reusing Codex CLI's own `~/.codex/auth.json` from Margin is *not addressed anywhere* — use the SIWC flow, not the CLI's cached login.

### Concern 4 — persona / system prompt
- Claude: fully replaceable (`--system-prompt`, SDK `systemPrompt` string: "The SDK sends only what you provide"; safety text is then yours to add).
- Codex: `thread/start` takes `developer_instructions` and `base_instructions` (app-server `ThreadStartParams`); `model_instructions_file` replaces the built-in prompt but OpenAI says "Users are STRONGLY DISCOURAGED from using this field… will likely degrade model performance". So: keep Codex's base prompt, put Margin's prompt in `developer_instructions`. Per-turn developer text is not available (*only the experimental collaborationMode*).

### Concern 5 — Margin's other AI
Lessons (`ask::write_lesson`), scene code (`video.rs`) and the condense summary all go through `margin_ai::ResponsesClient` with the API key. If the goal is "no API key", those must move too. Through an engine (ACP/app-server) that means re-implementing streaming lesson generation as agent turns — real rework. Through the SIWC **direct** path (option C below) it means a credential swap only. This is the deciding factor.

### Concern 6 — churn
Numbers in the headline: ACP crate 7 releases (3 majors) / 3 months; claude-agent-acp 38; codex-acp 33; Codex CLI 25 stable (214 incl. alphas), app-server "experimental and aren't supported for production workloads" but with a stable surface when `experimentalApi` is off (https://learn.chatgpt.com/docs/app-server). An ACP route stacks three moving parts (ACP SDK + adapter + app-server); talking to app-server directly stacks one.

### Concern 7 — rework of `crates/agent`
With any external engine, `replay.rs`, `turn.rs`'s step loop, retries, budgets, `tool_choice` gating and compaction are the engine's business, not Margin's. What survives: `Tool`/`TypedTool`/`ToolRegistry`, `ToolContext`, confirmations, `DeviceBridge` and device tools, `TurnSink`, all storage/events/routes, the whole app. See (3).

### UX questions
- **Cold start**: no documented figure for either CLI. Design around one warm `codex app-server` process for the service's lifetime (it serves many threads; `thread/start` per chat) — then there is no per-turn cold start. Claude: `--bare` "starts faster" but "doesn't use your subscription login".
- **Resume after restart**: Codex: "call `thread/resume` with the `thread.id` you recorded earlier"; rollouts are JSONL under `$CODEX_HOME/sessions`. Claude: `--resume <id>` from JSONL in `~/.claude/projects`. Both adapters advertise `loadSession: true`.
- **Multi-device sync**: unchanged — the service still owns `ChatItem`s and broadcasts them; engine events are mapped into items.
- **Reasoning stream**: Codex app-server `item/reasoning/summaryTextDelta` (`model_reasoning_summary = auto|concise|detailed|none`); Claude SDK thinking with `display: "summarized"`. Both map to `ChatItemKind::Reasoning` deltas.
- **Web search results**: Codex `WebSearchItem { query, action, results }` (`codex exec --json` / app-server items); claude-agent-acp renders `WebSearchResultBlock`. Map to a `ToolCall` named `web_search` with the query as label and results as on-demand `full_output`.
- **Model choice**: Codex `model/list`; subscription models today `gpt-6-astra`, `gpt-6.1-sol`, `gpt-6-sol`, `gpt-6-luna` (GPT-5.5 retired from Codex today, https://learn.chatgpt.com/docs/models); effort `low…max/ultra` per `turn/start`. Settings' `agent_model`/`reasoning_effort` carry over.
- **Two concurrent turns / memory**: one app-server process handles concurrent threads (*per-process memory unverified*); Claude via SDK = one Node process per session.

---

## (2) Recommendation

**Not ACP, not Claude-on-subscription, and not now as a rewrite.** Do this instead, in order:

**C (verify first, ~1 day spike): keep Margin's engine, change the credential.** The SIWC token-sharing page describes plan usage for local apps with scopes `offline_access resource.invoke chatgpt.tokens.use.direct`, and the app-server launch line it gives configures a *Responses-wire* provider fed by that token. If the plan token can be used by `margin_ai::ResponsesClient` directly (base URL + bearer token), Margin needs **no engine change at all**: lessons, videos, outlines, the condense summary and the agent all keep working, "no API key" is satisfied, and web search is the Responses API's hosted `web_search` tool — which the registry was explicitly kept ready for (owner decision Q1 said "no OpenAI hosted web_search", so that decision is reopened by this proposal). *This is unverified*: the spike must confirm (a) the endpoint and token work with `store: false`, function tools, `include: reasoning.encrypted_content` and `context_management`; (b) `web_search` is allowed under the plan ("Web search remains subject to model and account/workspace policy"); (c) token refresh (`offline_access`) and the per-app cap behave. Preview limits say "Unsupported tools: Image generation, file search, Code Interpreter, native computer use, hosted MCP/connectors" — none of which Margin uses.

**B (if C fails): add `codex app-server` as a second engine, directly over stdio JSON-RPC, no ACP.** OpenAI's own protocol, one moving part, the SIWC launch line is documented for it, resume and reasoning deltas exist, and the lock-down is `shell_tool=false` + `sandbox_mode="read-only"` + `approval_policy="never"` + `web_search="live"` (or `indexed`) + only Margin's MCP server + `CODEX_HOME` owned by Margin. Keep the Responses engine for API-key users, other base URLs, and for lessons/videos. Put an `Engine` trait between the service and both so ACP can be added later if it stabilises.

**Which CLI**: Codex only. Claude Code is excluded by Anthropic's terms for subscription use; with an API key the Claude *API* (with its hosted web search) would be a cleaner third engine than Claude Code, and that is a separate, later decision.

**Phase 3**: let it finish. It is engine-agnostic except for 3-C's Responses retry/Retry-After work and the title side-call (both stay valid for the Responses engine; see below).

---

## (3) Design delta if B is adopted (C needs none beyond auth + a `web_search` tool entry)

**`crates/agent`** — split into engine-neutral and Responses-specific:
- Keep as the neutral core: `Tool`, `TypedTool`, `ToolRegistry`, `ToolContext`, `ConfirmationPrompt`, `ToolOutput`, `TurnSink` (+ `retrying`), `DeviceBridge`/`DeviceCall`/device tools, `truncate_output`, `strict_schema`.
- New trait `Engine { fn run_turn(&self, deps, input) -> TurnOutcome; fn stop(...); fn supports_continue(...) }`; today's `run_turn` becomes `ResponsesEngine` (turn.rs + replay.rs unchanged inside it).
- New `CodexEngine` (service crate or a `crates/engine-codex`): owns the `codex app-server` child (one per service, restarted on exit), JSON-RPC over stdio JSONL with `experimentalApi: false`; per chat a `thread_id` (`thread/start` with `developer_instructions = system_prompt()`, model, `sandbox/approval/web_search` config; `thread/resume` after a restart); per turn `turn/start` with the user text (+ attached-file context as text), maps notifications → sink: `item/agentMessage/delta` → `AssistantText`, `item/reasoning/summaryTextDelta` → `Reasoning`, `item/started|completed` for MCP tool calls → `ToolCall`/`ToolResult`, web search items → `ToolCall{name:"web_search"}` with results as `full_output`, `turn/completed.usage` → `add_usage`; approval requests (shouldn't occur with `never`) → decline; Stop → `turn/interrupt`.
- Margin's tools reach Codex through a **service-hosted MCP server** (`rmcp`, streamable HTTP on loopback, one per-turn bearer in the `mcp_servers` config): each registry tool is listed once; the handler resolves the chat/turn from the bearer, runs `tool.confirmation`/`tool.run` exactly as the loop does today (confirmation parks the MCP call on `wait_for_decision`; set `tool_timeout_sec` generously and have the handler answer "declined" on Stop), writes `ToolResult` + cards through the sink itself (MCP results can't carry cards), and returns the text output. Device tools go through the same handler → `DeviceBridge` unchanged.

**Service**: `Chat.engine` fixed at creation (a thread can't be re-homed); `turns::run` picks the engine; `Continue` after a restart = `thread/resume` + a `turn/start` whose input is the Action text ("The service stopped while answering; continue.") — the history-replay path doesn't apply; recovery still marks `Interrupted`. Title generation (3-B) must go through `Engine::quick_text` (a throwaway thread turn on Codex). Settings: per-engine credentials — SIWC login flow (device/browser auth stored locally in the Keychain like the API key), default engine, model list from `model/list`. Lessons/videos stay on Responses unless C works (then nothing changes).

**Shared contract**: `Chat.engine: Engine { Responses, Codex }` (additive, default Responses); `Settings.default_engine`, `Settings.chatgpt_signed_in: bool`, auth routes for the SIWC flow; `ToolCall` for engine-native tools: reuse `ChatItemKind::ToolCall` with `name: "web_search"` and a `device: None` — no new item kind; `Error.code` already planned.

**App**: engine picker on New chat + Settings default; "Sign in with ChatGPT" pane; a `web_search` tool row renderer (query label, results list on expand); an engine badge on the chat header. Everything else (cards, confirmations, device-tool runner, folders, presence, usage footer, waiting banner) is reused as is.

**Phases**: 3 unchanged (its deliverables are all reused; 3-C's retry/`Retry-After`/A5 work is Responses-only but still needed for that engine; wasted only under a *replace*, which I don't recommend). 4 unchanged (folder tools are device tools; under B they are reached via the MCP handler, same `DeviceBridge`). 5: compaction/condense are Responses-only (already built); Codex compacts itself. New phase 3.5 or 6: engine trait + Codex engine + MCP server + SIWC auth (~ the size of phase 1's service work). Under C, the only new work is SIWC auth in Settings and enabling the hosted `web_search` tool in the registry.

---

## (4) Decisions that are the owner's

1. **Reopen Q1**: this proposal implies hosted web search is now wanted. OpenAI-hosted `web_search` (via Responses or Codex) or still "an alternative provider"? Everything above assumes OpenAI's.
2. **Accept the SIWC terms as the basis** (local runtime, Margin only, weekly cap you set in ChatGPT) and run the spike for option C before any engine work.
3. **Claude**: drop the subscription idea (terms); decide separately whether a Claude *API-key* engine is wanted later.
4. **Mixed engines**: one engine per chat, chosen at New chat, with a default in Settings — or a single global engine? (Per chat recommended; it's additive.)
5. **Usage budget**: what share of the ChatGPT plan Margin may use (the SIWC cap), knowing lessons/videos would draw on it too under C.
6. **Priority**: do this after phase 5 (hardening) or between 3 and 4? Recommend: spike C now (one day, no code in the main branch), decide B after the result, build it after phase 4 so folders don't straddle two tool transports.