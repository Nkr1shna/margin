## Phase-2 re-review and the final phase-3 brief

Read: commits `dc9179e..34021c9`, `crates/service/src/{agent_tools,ask,library,state}.rs`, `crates/agent/src/replay.rs`, the contract diff, the app `sources.rs`/`api.rs` diffs, `turns.rs` (unchanged in phase 2). No files edited.

### 1. The landed items, checked against the code

| Item | Verdict | Where |
|---|---|---|
| C1 chips persisted | **Correct.** `ask::accept_proposal` writes `chips` into `proposal.request` (level, videos, quiz, `lesson_count`) before lessons are created and the proposal is saved; `write_lesson` reads them later. | `ask.rs` `accept_proposal` |
| C2 `state::Author` | **Correct.** `StoredNote`/`StoredLesson.author`, default `Owner`; body change through `update_note`/`update_lesson` with `by`; lesson job sets `Agent`; `append_to_note` keeps it; retitle alone keeps it. | `state.rs`, `library.rs`, `ask.rs` line ~379 |
| C3 reusable functions + lock rule | **Correct.** `new_topic`, `update_topic`, `place_topic`, `remove_topic`, `update_lesson`, `remove_lesson`, `new_note`, `update_note`, `append_to_note`, `remove_note`, `accept_proposal` all take `write_lock` inside a block and emit after releasing; no tool holds it across a sink call. One deviation: `ask::propose` does `store.put(PROPOSAL)` with **no** `write_lock` (a fresh id, so no race today, but it breaks the convention every other writer follows — one-line fix, note). | `ask.rs` `propose` |
| C4 prompt sentence | **Acceptable as done differently.** The system prompt (`prompt.rs` line 46) is unchanged; the instruction lives in `propose_course`'s description ("when the course should draw on the owner's sources, search them first and pass their ids"), which is in the cached `tools` prefix too. Fine. | `agent_tools.rs` line 1393 |
| C6 no duplicate signal | **Correct.** The `Action` is added only by the route (`accept` → `record_course_created`); the tool returns the record. | `ask.rs` |
| C8 strings | **Correct.** "Delete the topic/lesson/note “X”?" + "Delete"; "Replace the text of your note “X”?" + "Replace"; details name the cascade (`go_with_it`) or the note's first 80 chars. | `agent_tools.rs` 1308–1319, 1647–1651, 1731–1735, 1794–1798 |
| A4 replay deferral | **Correct, and the narrowing is right.** Only `developer`/`user` messages between a step's calls and their outputs are deferred; the step's own reasoning/message/calls keep order (which is what the docs' "untouched" wants). Tested. | `replay.rs` |

Still open from phase 2 (go into the phase-3 first commit): A1 `ReasoningEffort`, A5 context → user role, A6 `Error.code`, C5 `Superseded`.

### 2. The phase-2 agents' choices

**(a) Writes run to the end after Stop, model told "cancelled by the user" — wrong; your instinct is right.** `to_the_end` (`agent_tools.rs` 857–868) spawns the change so it completes, but `turns::stop` → `wind_up_locked(Stop)` writes `cancelled_call_output` for the unanswered call. The model then believes the topic/note/course doesn't exist and will create it again (duplicate topics, a second course, a note replaced twice). Fix (phase 3, service, both areas against one stub): `to_the_end` registers its `JoinHandle<ToolOutput>` under `(chat_id, call_id)` in `Turns` before running; `wind_up_locked(Stop)` awaits registered handles for that chat (bounded, 15 s) and records their **real** `ToolResult` (ok/summary/output, plus the result card) instead of "Cancelled"; only a write that doesn't finish in time gets `CALL_INTERRUPTED`-style text ("Stopped before this change was confirmed; check whether it took effect"). `Ending::Restart` is unchanged (the process is gone). Confirmation-parked writes never started, so they stay `Denied { by_stop }`.

**(b) `accept_proposal` only takes proposals from the same chat — correct.** The model only knows ids from this chat; cross-chat acceptance would let a stale card from another conversation create a course. Keep.

**(c) `create_note` requires a title — correct.** The model can always produce one; untitled notes are an owner convenience (`note_title` → "Untitled note" still handles old ones). Keep.

**(d) An empty owner note is replaced without asking — correct.** Nothing of the owner's is overwritten; the description tells the model. Keep.

**(e) Deleted parent → filed under `subject_title`, tool does it without asking — correct for the route, slightly under-reported for the tool.** The card shows the Subject field and a notice before the owner says "go ahead", so no confirmation is needed. But the tool's output doesn't say it happened; add `"note": "The topic it was to go under was deleted; filed under the subject “X” instead."` when `parent_topic_id` was cleared, so the model tells the owner. Small, 3-A.

**(f) Removing the old composer removed `AddSourcesMenu`/`AskAttachments` — a real loss, not wrong.** `sources::add_sources` now takes `topic_id: Id` (the no-topic upload path is gone). Phase 3-E recovers them from `git show d09470c:crates/app/src/ui/sources.rs` and `…/sources.rs` as `ChatAttachments`/`add_sources(topic_id: Option<Id>)`, but **not** the folder branch (folders are workspaces now — `AddSourcesMenu` must offer "Attach folder" as a workspace action in phase 4, and in phase 3 it offers files only).

### 3. Phase 3 — final build brief

#### First commit (orchestrator lands on `claude/agent-build` before branching)

`crates/shared`:
1. `ReasoningEffort`: variants `Low, Medium, High, XHigh, Max` plus `Minimal` kept **only** so stored rows still parse (`#[doc(hidden)]`, `as_str() → "low"`, not in `ALL`); `ALL = [Low, Medium, High, XHigh, Max]`, default `Medium`; `as_str` sends `"xhigh"`/`"max"`.
2. `ChatItemKind::Error { message, retryable, #[serde(default)] code: Option<String> }`. Codes the loop/service will use: `no_api_key`, `no_model`, `rate_limited`, `server_error`, `network`, `context_too_long`, `incomplete`, `bad_request`, `storage`, `interrupted`.
3. `ProposalStatus::Superseded { by: Id }`; `accept` answers 409 `superseded`.
4. `Chat.title_by: TitleSource { FirstLine (default), Model, Owner }` (`#[serde(default)]`).
5. `Chat.waiting: Option<Waiting { until: DateTime<Utc>, reason: String }>` (`#[serde(default)]`).
6. Contract: §3 data, §4.5 (uncapped `Retry-After` + waiting), §4.6 (Stop awaits in-flight writes), §5 (`Superseded`), titles, A5 (context in the user role).

`crates/agent` (stubs only): `TurnSink::retrying(&self, until, reason)` and `clear_waiting(&self)` with default no-op bodies.

`crates/service` (stub only): `Turns::register_write(chat_id, call_id, JoinHandle<ToolOutput>)` and `Turns::take_writes(chat_id) -> Vec<(String, JoinHandle<ToolOutput>)>` as `unimplemented!("phase 3, service-turns")`, so 3-A can call it and 3-B fill it.

`crates/app`: nothing shared; `fake.rs` is owned by 3-E (3-D uses the existing `fake.direct()`).

#### Areas (worktree each; files are exclusive)

| Area | Worktree | Builds | Owns (exclusively) |
|---|---|---|---|
| **3-A service-jobs** | `agent/p3-jobs` | `write_lesson` (appends a Queued lesson to a topic; `StoredLesson.prompt: Option<String>`, `with_quiz: bool` service-only fields so `lesson_context` works without a proposal; `first_write=false`), `regenerate_lesson` (confirm when `author == Owner`: "Rewrite the lesson “X”? You edited its text; your edits would be lost." / "Rewrite"), `make_video` (`video::enqueue`), `attach_sources` (chat files only → `sources::attach_locked`), result cards (lesson/video), (e)'s note, `to_the_end` registering via `Turns::register_write`, C5 superseding in `propose_course`, `ask::propose` under `write_lock`, `SOURCE_SEARCH_WAIT` 15 s | `agent_tools.rs`, `ask.rs`, `video.rs` (pub exposure only), `state.rs` (StoredLesson fields), `tests/courses.rs`, `tests/agent_e2e.rs` |
| **3-B service-turns** | `agent/p3-turns` | (a) `Turns::register_write`/`take_writes` + Stop awaiting writes and recording real results; A3 (`ChatChanged` only on state/title/pending/usage/attachments/waiting; `updated_at` saved silently; broadcast channel 4096); `Chat.waiting` from `sink.retrying`; model-written title side task after the first `Finished` turn (one `ModelClient::stream` call, no tools, low effort, ≤ 30 output tokens; `title_by: Model`; `PATCH` title sets `Owner`); `Error.code` for `Early` errors; revoke → `DeviceHub::forget_device` + `workspaces::delete_for_device_locked` + `DeviceDeleted`; `ServiceSink::retrying/clear_waiting` | `turns.rs`, `chats.rs`, `auth.rs` (revoke only), `events.rs`, `server.rs` (channel size), `tests/chats.rs`, `tests/common/mod.rs` (scripted title reply) |
| **3-C agent** | `agent/p3-agent` | A2: `RetryPolicy::delay` returns `retry_after` uncapped (+0–1 s jitter), computed backoff still capped; `sink.retrying(until, reason)` before every sleep, `clear_waiting` after; A5: context moved into the user message as a leading `input_text` part (keep `quoted`, keep "names are data"); A6: `code` on every Error the loop stores; `incomplete_reason` learns `max_messages`/`steered`; tests | `crates/agent/src/{turn,prompt,sink}.rs`, `crates/agent/tests/turn.rs` |
| **3-D app-transport** | `agent/p3-app-transport` | R3.1 and R3.2 below; the device-tool runner (`tools/mod.rs`: per-request task keyed by `request_id`, deadline from the frame, `ToolCancel` aborts, folder tools answer `Err("This build can't read folders yet")`); app-control runner (`navigate`, `open_citation` → `AppCtx.source_viewer`, `show_passage`, `toggle_sidebar` → `set_config(sidebar_hidden)`); mobile socket close on background (R3.5); self-presence from `LiveStatus` | `main.rs` (live loop), `events.rs`, new `tools/{mod,app_control}.rs`, `state.rs` (two new `AppCtx` fields, see below), `scene.rs` (iOS background hook), `ui/shell.rs` (the drain effect only) |
| **3-E app-chat** | `agent/p3-app-chat` | Usage footer; `ChatAttachments` + files-only `AddSourcesMenu` restored from `d09470c` (upload without topic, `NewMessage.source_ids`, chips, `ChatPatch.source_ids` to detach); waiting banner from `Chat.waiting` with Stop; title rendering by `title_by` (placeholder style until Model/Owner; rename sets Owner); "Answer the question above first" while `WaitingForUser` + pending; `Superseded` outline card state; `Error.code` → "Open Settings" for `no_api_key`/`no_model`; result cards for lesson/video live state; online dots in Settings → Devices; Settings reasoning-effort list from `ReasoningEffort::ALL` | `ui/chat.rs`, `ui/outline.rs`, `ui/sources.rs`, `sources.rs`, `ui/settings.rs`, `api.rs` (no new routes expected), `store.rs`, `fake.rs` |

Ownership re-check against the current tree: `agent_tools.rs` was 1 870 lines after phase 2 and is 3-A's alone; `turns.rs`/`chats.rs` untouched in phase 2 and 3-B's alone; `state.rs` (service) is split by *section* (3-A adds `StoredLesson` fields only; 3-B adds nothing there — `Turns` lives in `turns.rs`); app `state.rs` is 3-D's (3-E reads `AppCtx` but adds no fields — if 3-E needs one, it asks you); `ui/shell.rs` goes to 3-D because the command drain lives there; `ui/settings.rs` to 3-E (devices section + effort list). No two areas share a file.

#### R3.1 settled: who writes to the socket

`events.rs`: `connect` returns the socket; the live loop immediately does `let (mut sink, mut stream) = socket.split()` (`futures_util::StreamExt::split`); `next_frame` takes `&mut SplitStream<Socket>`. A `tokio::sync::mpsc::UnboundedSender<DeviceFrame>` is created per connection inside the live loop and published as `AppCtx.device_frames: Signal<Option<UnboundedSender<DeviceFrame>>>` (set after `Hello`, set to `None` on disconnect); the `select!` gains `Some(frame) = rx.recv() => sink.send(Message::Text(json)).await`. The runner sends through the signal; a frame with no sender is dropped (the service's deadline covers it). No split-sink `Arc<Mutex>`; one task owns both halves.

#### R3.2 settled: app control reaches the router on the main thread

`AppCtx.agent_commands: Signal<VecDeque<AgentCommand>>` where `AgentCommand { call: DeviceToolCall, reply: oneshot::Sender<DeviceToolResult> }` (only app-control variants are pushed). The shell component (`ui/shell.rs`, which already owns `navigator()` and the `Command` dispatch) adds a `use_effect` that drains the queue: `Navigate` → `nav.push(route)` built by a `NavTarget → Route` function in `routes.rs` (3-D may add that one function); `OpenCitation` → look up the lesson in `lib`, set `source_viewer`; `ShowPassage` → set `source_viewer` from the source; `ToggleSidebar` → the existing `Command::ToggleSidebar` branch. Each replies `Done { message }` with the opened title, or `Err` when the id is unknown. The runner task only pushes and awaits the reply with the frame's deadline.

#### R3.5 (new): mobile background close

`scene.rs` already has the `UIWindowSceneDelegate`; add `sceneDidEnterBackground:` / `sceneWillEnterForeground:` setting `AppCtx.foreground: Signal<bool>`; the live loop closes the socket (`Message::Close`) when it goes false and reconnects when true. Android: tao delivers `Suspended`/`Resumed` there (the scene.rs comment says only iOS-under-scenes lacks them) — wire the same signal. Computer platforms never close.

#### Risks to settle before branching

- (a) above is the only cross-area interface: the two `Turns` stubs. Land them in the first commit with their doc comments so 3-A and 3-B build against the same signature.
- Title generation in tests: the service e2e tests use `FakeGenerator.agent` (a `ScriptedModel`); every scripted conversation that finishes a first turn now needs one more scripted reply (the title) or the title task must tolerate `ScriptedModel` running out of script (make it log and keep `FirstLine`). Decide: **tolerate** (3-B), so 3-A's tests don't change.
- `ReasoningEffort::Minimal` rows: a stored `"minimal"` must still parse (hidden variant) — the first-commit test should round-trip `{"reasoning_effort":"minimal"}` to `Minimal` and send `"low"`.