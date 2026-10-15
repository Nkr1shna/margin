# Margin agent design — review and corrected direction

Scope: review of the Margin Agent Design page against `PLAN.md` and the code (service `ask.rs`, `sources.rs`, `events.rs`, `state.rs`, `server.rs`; ai `lib.rs`/`sse.rs`; app `ui/ask.rs`, `main.rs`, `events.rs`, `state.rs`, `picker/*`, `sources.rs`). Facts about external APIs were looked up this week (URLs inline). No files were changed.

Vocabulary used below (no "desktop"): **computer platforms** = macOS and Linux (Windows only if it is ever built; Dioxus 0.7 supports it but nothing in the repo targets it); **Apple mobile** = iOS and iPadOS (one code path, `picker/ios.rs`); **Android**. "The service" = `margin-service` on the owner's Mac. "A device" = any installed app instance (a Mac app counts as a device; the service Mac can also run the app).

## 1. Verdict on the current design

### What is right
- The product shape: Ask becomes a chat with inline cards, the outline card stays editable with Create lessons, a Chats section in the sidebar, chats stored on the service and synced, agent acts freely and asks before deletes, course options inferred/asked with chips on the card. All consistent with the owner's answers; keep.
- Keeping lesson writing, video rendering and source embedding as service jobs started by tools and observed through the existing events. Correct: they are long and already have queueing, recovery and progress.
- The tool inventory is roughly the right set (library read/write, sources/retrieval, folder read, app control, deletes confirmed). It lacks schemas, limits and routing, covered below.
- The three "points to confirm" (web search, folder writes, shell) are genuine owner questions; carried into section 3 with recommendations.

### What is wrong
1. **"Where the agent runs: on the device".** This is the central mistake and most of the vagueness flows from it.
   - A turn that runs on an iPhone/iPad stops when the app is backgrounded: UIKit gives `applicationDidEnterBackground` five seconds and then suspends the process; `beginBackgroundTask` buys seconds, not minutes (https://developer.apple.com/documentation/uikit/extending-your-app-s-background-execution-time). A multi-tool turn easily lasts longer than a lock-screen tap. Android is similar once the activity is not visible (https://developer.android.com/develop/background-work/background-tasks).
   - "A chat runs on the device that sent the message; others can't send until it finishes" means a chat is stuck "Answering on MacBook Pro" if that laptop goes to sleep mid-turn, with no recovery path because the partial turn only exists in that app's memory.
   - Every library tool would be an HTTP round-trip from the device to the service anyway; the tool logic (validation, locking, events) would be duplicated on the client side of an API that already exists.
   - Closing the app mid-turn loses the turn.
2. **The model relay.** `POST /api/model/responses` forwarding an arbitrary Responses request with the key attached is a key-spending oracle: any device token could call hosted `web_search`, `code_interpreter`, `image_generation`, `computer`, or an unrelated model, with no per-chat accounting. The owner's actual decision was "keys never on devices"; the relay was only needed because the loop was put on the device. With the loop on the service, the key never leaves the Keychain and no relay exists at all, which satisfies the decision more strongly.
3. **"Attached in place on desktop and read locally" / "On iOS/Android: not available".** Factually wrong and against PLAN.md. Folder access is supported and persistable on both: iOS 13+ `UIDocumentPickerViewController(forOpeningContentTypes: [.folder])` returns a security-scoped directory URL that can be saved as a bookmark (https://developer.apple.com/documentation/uikit/providing-access-to-directories); Android `ACTION_OPEN_DOCUMENT_TREE` + `takePersistableUriPermission` survives reboots (https://developer.android.com/reference/android/content/ContentResolver#takePersistableUriPermission(android.net.Uri,%20int)). The code already does both picks; it just copies files afterwards.
4. **"Contents read from a laptop folder are kept only as short summaries."** Contradicts how the Responses API continues a conversation: with `store: false` you must replay every output item plus every `function_call_output` (https://developers.openai.com/api/docs/guides/conversation-state); if the stored transcript holds only summaries, the next turn's model input is not the conversation that happened. Whether other devices *see* the outputs is a separate choice (Q3).
5. **No answer for the cross-device questions**: what a chat with a MacBook folder shows on a phone, where tool executions happen, what "Stop" does on another device, what happens when the service restarts mid-turn, what happens when two devices send at once.

### What is vague or hacky
- "⌘N starts a new chat" and "⋯ / right-click" describe keyboard/mouse only; touch needs a visible New chat button and a long-press menu.
- "sidebar and appearance (show/hide, theme)" as agent tools: appearance is a per-device local preference; which device would it change? Low value; defer.
- "attach an uploaded file to a chat": the service purges sources never attached to a topic after 24 h. A chat attachment needs a rule or it silently disappears.
- "Tool result trimmed to a summary" and "a size limit per request": no numbers, no truncation rule, no per-turn budget, no context-window strategy.
- No error model (429/5xx, device offline, tool timeout), no idempotency, no cancellation semantics, no token accounting.
- Build plan phase 1 builds the relay, i.e. the wrong thing first.

## 2. Corrected architecture

### 2.1 Where the agent loop runs: on the service; device-only tools run on the device that owns the resource

**Decision.** The loop (model calls, tool dispatch, chat persistence) is a service job, like lesson and video jobs today (`tokio::spawn`, abort handle, `write_lock`, recovery at startup). Tools fall into two classes:
- **Service tools**: everything over the library, sources, retrieval, proposals and jobs, executed in-process by calling the same functions the HTTP handlers call, so validation, locking and events are shared, not duplicated.
- **Device tools**: folder reads and app control. The service sends a `ToolRequest` to one specific device over its events socket and awaits a `ToolResult`. The socket becomes bidirectional.

**Why.**
- Chats sync for free: the service writes items and broadcasts `ChatItemChanged`/`ChatItemDelta` like `LessonDelta`; every device renders the same transcript live. No "answering on MacBook Pro"; any device can Stop.
- App closed or backgrounded on Apple mobile/Android: the turn keeps running on the Mac. Only device tools need the device.
- Resumability: a turn interrupted by a service restart is marked `Interrupted` by the existing recovery pass and can be continued because every item is in the database.
- The key stays in the Keychain; no endpoint lets a device spend it on arbitrary requests.
- Folders: the service never reads a path by itself. A workspace is readable only through the app that holds the handle. The service is internet-reachable through the tunnel; giving it a "read this absolute path" capability would turn any leaked device token into filesystem access on the Mac. Consequence, stated plainly in the UI: *to read a folder attached from the Mac, the Margin app on that Mac must be running* (it can be in the background). This is what "runs on the machine" means here: the model and loop are on the owner's Mac, the folder is read where it lives.

**Device presence and routing.** A `DeviceHub { device_id → sender }` populated when an events socket opens; `Device` gains `online`. Directed frames go only to the addressed device; broadcast events stay as they are.

**Device tool lifecycle.** Pending request `{request_id, chat_id, call_id, device_id, deadline}`. Deadlines: 60 s for `list_files`/`find_files`/`read_file`, 120 s for `search_text`, 20 s for app-control. On timeout or when the device isn't connected, the tool returns an error output such as "iPhone isn't connected to Margin right now (the app must be open to read the folder)" and the model carries on. Stop cancels pending requests. App-control tools go to the **originating device** of the turn.

**Concurrency.** One running turn per chat. A message posted while a turn runs is stored as a pending message (one at a time; the composer shows "Will send when the answer finishes" with cancel). Two concurrent turns across chats.

**Cancellation.** `POST /api/chats/{id}/stop` aborts the task. Saved items stay; an unanswered `function_call` gets `{"error":"cancelled by the user"}` on the next turn so the replayed history is valid. Jobs already started by tools continue.

**Recovery.** Startup marks running turns `Interrupted`; the chat shows a Continue button.

### 2.2 Model access and Responses API usage (service side; no relay)

Extend `margin_ai::ResponsesClient` (keep its tested SSE parser) with a general streaming call over `input`, `tools`, `tool_choice`, `reasoning`, `max_output_tokens`, `prompt_cache_key`, `context_management`, `parallel_tool_calls`.

- **State: `store: false`, the service owns the transcript, replay all output items** (https://developers.openai.com/api/docs/guides/conversation-state); `include: ["reasoning.encrypted_content"]` (https://developers.openai.com/api/docs/guides/reasoning). Works with the configurable base URL and needs no provider-side state. Each item's verbatim model JSON is stored on the service.
- **System prompt as a `developer` message**, stable for prompt caching; `prompt_cache_key = chat id`.
- **Function tools with `strict: true`**, parallel calls on; restrict per step with `tool_choice: {type: "allowed_tools"}` rather than changing the tools array (https://developers.openai.com/api/docs/guides/function-calling).
- **Streaming**: handle `output_item.added/.done`, `function_call_arguments.delta/.done`, `output_text.delta`, `reasoning_summary_text.delta`, `completed/failed/incomplete`, `error` (https://developers.openai.com/api/docs/api-reference/responses-streaming).
- **Context window**: server-side compaction (`context_management` with a threshold, https://developers.openai.com/api/docs/guides/compaction); for base URLs that reject it, a service-side trim of old tool outputs plus a summary item.
- **Tool output size**: Margin caps each tool output at **64 KB** and each turn at **512 KB**, then compacts.
- **Errors**: 429/5xx retried 3 times with backoff and `Retry-After`; then an Error item with Retry.
- **Model config**: Settings gains reasoning effort and an optional agent model. Note on the current OpenAI lineup: `gpt-5`, `gpt-5-mini`, `gpt-5-nano` shut down 11 Dec 2026; current options are `gpt-6.1-sol` (balanced), `gpt-6-astra` (flagship), `gpt-6-luna` (cheap) (https://developers.openai.com/api/docs/models, https://developers.openai.com/api/docs/deprecations).

### 2.3 Folders ("workspaces"): what "attach" means on each platform

A **workspace** is a folder the owner granted one device access to. The *handle* lives only on that device; the service stores metadata.

| Platform | Pick | Handle kept on the device | Persistence / revocation | Reading |
|---|---|---|---|---|
| macOS (direct distribution, not sandboxed) | NSOpenPanel via `rfd` (already used) | Path plus a plain bookmark, so a moved folder still resolves | Persists; macOS may prompt for Desktop/Documents/Downloads; treat permission errors as revoked. If ever sandboxed (App Store), switch to security-scoped bookmarks (Q6) | `std::fs` |
| Linux | xdg-desktop-portal folder pick via `rfd` (already) | Path (under Flatpak a portal path that persists) | Persists | `std::fs` |
| iOS / iPadOS | `UIDocumentPickerViewController` for folders (already) | Bookmark created while access is open | Survives relaunch and reboot; revocable in Settings; stale bookmarks ask to re-attach. Works for On My iPhone/iPad and iCloud Drive; most third-party providers grey folders out; external drives break after unmount | One access scope per tool call; coordinated reads so iCloud files download; reads only while the app is open |
| Android | `ACTION_OPEN_DOCUMENT_TREE` (already) | Tree URI with `takePersistableUriPermission` (the current Kotlin doesn't call it yet) | Survives reboot until revoked; can't pick storage root, Download or Android/data | `DocumentsContract` child queries and file descriptors; reads while the app is visible |

Common rules: relative paths only, no `..`/absolute/NUL; canonical prefix checks; skip hidden files, `.git`, build folders, and anything that looks like a secret (`.env*`, `*.pem`, `*.key`, `id_rsa*`, `credentials*`); binary files listed, not read; read cap 2 MB per file, search 50 MB / 20 s per call. Other devices see "Attached: margin on MacBook Pro · online/offline"; opening the chat on a phone works, and the model reads the folder through the Mac app if it's connected, otherwise says so. A per-device **Folders** pane in Settings lists and removes workspaces.

### 2.4 Tool system

A new crate `crates/agent` with a `Tool` trait (name, description, strict JSON schema from `schemars`, kind Read / Write / Destructive / Device, async run) and a per-turn registry.

- **Library (service):** `search_library`, `list_topics`, `get_topic`, `read_lesson`, `read_note`, `list_notes`, `create_topic`, `rename_topic`, `move_topic`, `edit_lesson_title`, `create_note`, `append_to_note`, `replace_note_body` (Q2); deletes are Destructive.
- **Courses (service):** `propose_course` (validates and stores the outline, shows the editable card), `accept_proposal` (same as Create lessons, for "yes, go ahead"), `write_lesson`, `regenerate_lesson`, `make_video` (enqueue existing jobs; progress via existing events; result cards).
- **Sources (service):** `search_sources` (retrieval with citations), `list_sources`, `attach_sources`.
- **Folders (device, read-only):** `list_files`, `find_files`, `search_text`, `read_file` (400 lines / 64 KB per call).
- **App control (device that sent the message):** `navigate`, `open_citation`, `show_passage`, `toggle_sidebar`. Appearance deferred.
- **Confirmation:** a Destructive tool parks the turn with a Confirmation card; approve/deny from any device; Stop counts as deny.
- **Idempotency and errors:** every call recorded by `call_id` before it runs; errors go back to the model as text, never thrown.

### 2.5 How today's pipelines fit

| Today | With the agent |
|---|---|
| `POST /api/ask` → proposal job | The chat turn; the model calls `propose_course`; the outline card reuses today's editor with chips; `/api/ask` removed |
| Accept proposal | Same route, plus chip overrides; the chat records "course created" so the next turn knows |
| Lesson jobs (retrieval, citations, streaming) | Unchanged; started by tools or Create lessons |
| Video pipeline | Unchanged; `make_video` enqueues |
| File sources | Unchanged; files added in a chat are tied to it so the 24-h cleanup skips them |
| Folder upload | Removed; "Attach folder" creates a workspace |

### 2.6 Data and API

New shared types: `Chat` (state Idle / Running / WaitingForUser / Interrupted / Failed, workspaces, sources, pending message, usage), ordered `ChatItem`s (user message, assistant text, reasoning summary, tool call, tool result, confirmation, action, error), `Card` (outline, result, passages), `Workspace`. New tables `chat`, `chat_item`, `workspace`; no migration of existing data.

Routes: `GET/POST /api/chats`, `GET/PATCH/DELETE /api/chats/{id}`, `POST /api/chats/{id}/messages|stop|continue|confirm`, `POST/DELETE /api/workspaces`, devices gain `online`. Events: `ChatChanged`, `ChatDeleted`, `ChatItemChanged`, `ChatItemDelta`, `WorkspaceChanged`, `WorkspaceDeleted`, `DeviceChanged`; plus directed `ToolRequest`/`ToolCancel` frames to a device and `ToolResult` back.

### 2.7 Security

- Untrusted content (files, sources, web results) only enters as tool outputs, never the developer prompt; the prompt says it's data (https://developers.openai.com/api/docs/guides/agent-builder-safety). No tool fetches URLs or writes outside the library.
- Device tools only go to the device that registered the workspace, which verifies it locally.
- Path rules unit-tested against `..`, absolute paths, symlinks out, NUL and Unicode tricks.
- The key never leaves the Keychain; logs carry sizes and hashes, never tool output.
- Spend control: step cap per turn (about 40 model calls), output budgets, usage shown in the chat footer.

### 2.8 Framework or custom loop

**A custom loop in Rust on the existing `margin-ai` client.** There is no official OpenAI Rust SDK; the Agents SDK is Python/TypeScript only (https://developers.openai.com/api/docs/guides/agents/sdk). `async-openai` 0.42.2 covers the Responses API but has no loop and would duplicate Margin's tested client; `rig` 0.44 has a loop but breaks every few weeks; `swiftide` is stale; `llm-chain` is dead. Use `schemars` 1.2 for schemas. Separate pre-existing check: on Android, `reqwest` 0.13 verifies TLS through `rustls-platform-verifier`, which needs its Kotlin part and an init call; confirm the Android HTTP client is configured like the WebSocket one.

### 2.9 Testing

A scripted fake model that emits the real streaming events (text and tool calls), checking call order, stored items and replay, events, state changes, confirm/stop/continue after a restart, compaction fallback and 429 retries; a fake device that answers (or ignores) tool requests; tool tests against the in-memory store and temp folders; a strict-schema test; app render tests and offscreen screenshots; manual checks on iOS (iCloud Drive, revoke, stale bookmark) and Android (grant after reboot), and "Mac app closed while chatting from the phone". Tracing spans per turn, model call and tool, with usage persisted.

### 2.10 Build plan (each phase ships something usable)

1. **Chat foundation:** types, routes, events; the agent loop with streaming, replay, retries, stop/continue/recovery; read-only library and source tools; Ask becomes the chat; Chats in the sidebar (new/rename/delete for keyboard and touch). *Usable: a synced assistant that knows your library.*
2. **Courses through the agent:** `propose_course` + outline card + Create lessons; library write tools; delete confirmation; old Ask flow removed.
3. **Jobs, attachments, app control:** lesson/video tools with result cards, files attached to chats, device presence, app-control tools, usage footer.
4. **Folders:** 4a macOS and Linux, 4b iOS/iPadOS, 4c Android; folder context into course proposals.
5. **Hardening:** compaction and fallback, prompt-injection tests, web search if approved, optional iOS continued-processing for long scans.

## 3. Owner decisions (2026-10-14)

These answer the review's questions and are binding for the build.

| Question | Decision |
|---|---|
| Where the agent loop runs | **On the service** (Fable's recommendation, approved). No model relay; the key stays in the Mac's Keychain. Folder tools run on the device that holds the folder; app-control tools on the device that sent the message. |
| Model providers (2026-10-14, later) | **Switchable per chat: OpenAI (API key), ChatGPT (Sign in with ChatGPT; the spike passed) and Claude (API key, Messages API from Rust).** Margin keeps its own loop, tools, cards and confirmations; only the model provider changes. Web search is each provider's hosted tool (supersedes Q1; Claude web fetch on, no per-chat switch). Three model slots per provider (chats, writing, small tasks); Claude defaults Opus 5.5 for chats and writing, Haiku 5.5 for small tasks; OpenAI 401/403 is `auth_failed`. See `docs/agent-providers-brief.md`. |
| Q1 Web search | **Not now** (superseded by the providers decision above). The agent uses the library, sources and attached folders only. The owner is evaluating an alternative search provider to integrate later, so keep the tool registry ready for a search tool (no OpenAI hosted web_search). |
| Q2 Overwrites | **Confirm when it's the owner's own writing** (notes they wrote, lessons they edited); agent-created content changes freely; appending to a note never asks; deletes always confirm. |
| Q3 Folder excerpts | **Load on demand.** Excerpts are stored with the chat on the service (needed for replay) but not pushed to other devices; a device fetches an excerpt when its tool card is expanded. Deleting the chat deletes them. |
| Q4 Folder knowledge in lessons | **Folder-file citations now.** Lessons can cite files from attached folders like sources; opening such a citation reads the file from the device holding the folder (shows that device offline otherwise). |
| Q5 Writes / commands | **Neither.** Attached folders are read-only; no shell commands. |
| Q6 macOS distribution | **Direct distribution, not sandboxed.** Plain paths + bookmarks now; the handle abstraction keeps a sandboxed (security-scoped bookmark) variant possible. |
| Appearance tool | **Left out.** The agent can't change theme/appearance; app control is navigate, open_citation, show_passage, toggle_sidebar. |
| iOS continued processing | **Build it.** Long folder scans on iOS/iPadOS continue in the background with `BGContinuedProcessingTask` (iOS 26) and the system progress UI. Scope (later answer): any running chat that reads a folder this iPhone/iPad holds keeps reading in the background through the task, not only long scans. |
| Chat titles | **Written by the model after the first answer** (a small side call, not a tool); until then the first line of the first message stands in. The owner can rename a chat; the model never replaces the owner's title (`Chat.title_by`: first line / model / owner). |
| Message during a pending confirmation | **Queued**, like a message sent while the agent answers: it becomes the chat's pending message and is sent when the turn ends. |
| `Retry-After` | **Honoured as the server asks, uncapped**; the chat shows it's waiting (until when, and why) with Stop. Computed backoff stays capped. |
| Sidebar | **The sidebar row stays "New chat"** (the new-chat screen and its row keep that name). |
| Reasoning effort | **Low / Medium / High / XHigh / Max** in Settings, default Medium. A stored "minimal" from an older build reads as Low. |
| After Create lessons | **The app opens the first lesson**; the chat keeps the course card. |
| Approved replace | **When the owner approves the agent replacing their note, the note becomes the agent's writing** (later agent changes to it don't ask again). |
| Already decided | Chats stored on the service and synced; Ask = chat with inline cards; Chats section in the sidebar; course options inferred/asked with chips on the outline card; agent acts freely, confirms deletes; 500 MB per audio/video file, 50 MB others; no folder upload, single-file sources stay. |
