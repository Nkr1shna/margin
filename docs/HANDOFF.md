# Handoff: Margin agent build

Written 2026-10-14 so the build can continue from a new Claude Code project. Work happens on branch **`claude/agent-build`**. GitHub `main` is still old (dbd08ff); don't build on it.

## Read first

| Doc | What it is |
|---|---|
| `PLAN.md` | Product plan and UI rules |
| `docs/agent-architecture.md` | Binding agent design. §2.10 lists the five phases and §3 the owner's decisions |
| `docs/agent-contract.md` | Contract shared by all phases (types, routes, protocol) |
| `docs/agent-prebuild-review.md` | Fable's architect review; section D has the original phase briefs |
| `docs/agent-phase3-brief.md` | Phase 3 brief (built), and the shape later briefs follow |
| `docs/agent-phase4-brief.md` | **Final phase 4 brief (folders), reworked for the owner's allow-list answer; Q1–Q3 decided** |
| `docs/agent-providers-brief.md` | **Switchable model providers brief (OpenAI key / ChatGPT sign-in / Claude API)**; see the status below |
| `docs/agent-engine-review.md` | Fable's review of the ACP/CLI-engine idea. Background only, superseded by the providers decision |
| `docs/service-setup.md` | How the service and Cloudflare Tunnel run |

## Status

- **Phase 1** (agent loop, chats, device tool transport): merged, reviewed and fixed.
- **Phase 2:** merged, reviewed and fixed (34021c9).
- **Phase 3** (job tools, Stop awaiting writes, Retry-After waiting, model titles, app-control tools, chat attachments, mobile reconnect): merged, reviewed, 6 findings fixed (a615637). 527 tests at integration. Clippy is clean, and the desktop and iOS-sim builds pass.
- The chat thinking-block overflow fix is merged (420bc16).
- **Phase 4** (attached folders on desktop, iOS and Android): brief written, not built.
- **Providers** (owner decision 2026-10-14): Fable's brief is written in `docs/agent-providers-brief.md`, not built.
  - **Shape:**
    - Two traits in `crates/ai/src/model.rs`: `TextModel` and `Provider`. `ResponsesClient` and a new `ClaudeClient` implement both.
    - History stays verbatim per provider (`Chat.provider` is immutable), with Claude-specific replay.
    - Web search and fetch show as tool rows, a `Card::WebResults` card and cited Sources.
    - New error codes: `auth_failed`, `spend_limit_reached`, `plan_limit_reached`, `plan_ineligible`.
    - Three Keychain entries on the service Mac.
    - The SIWC OAuth runs in the service, and the owner signs in in a browser on that Mac.
  - **Areas:** P-ai-openai, P-ai-claude, P-agent, P-service and P-app, after a first commit.
  - **Sequencing (Fable):**
    1. Phase 4 first.
    2. The SIWC spike runs in parallel with phase 4. It's a throwaway example the owner runs by hand, never merged.
    3. Then providers.
    4. Then phase 5.
  - **Spike outcome:** if registration or inference fails, ChatGPT is dropped (unselectable) and the rest goes ahead. If only web search fails, ChatGPT ships without it.
- **Phase 5** (hardening plus iOS BGContinuedProcessingTask for any chat reading a folder on that device): not started.
- **Hands-on run:** when every phase is in, rebuild and install the app and service.

## Owner decisions (latest; older ones are in architecture §3)

**Providers:**
- Margin keeps its own agent loop, and the model provider becomes switchable:
  - OpenAI Responses with an API key, or with "Sign in with ChatGPT" (SIWC) on the owner's plan.
  - Anthropic Claude Messages API with an API key, called directly from Rust. There's no official Rust SDK; the Agent SDK and claude.ai login were rejected.
- Web search uses each provider's hosted tool (Claude `web_search_20260318` plus web fetch; OpenAI `web_search`). This replaces the old "no OpenAI hosted web search" decision.
- The provider is picked per chat at New chat, with a default in Settings. A chat stays on its provider.
- Lessons, videos, outlines, titles and condense also follow the default provider.
- The SIWC path needs a spike first. If it fails, drop only that option.

**Still open with the owner:**
1. Phase 4 Q1–Q3 are decided (2026-10-14): stored excerpts on regenerate; an allow-list of file types (prose documents, PDFs, images) instead of a skip list, no audio/video for now; no notice on the holding device. Code from a GitHub repository becomes its own phase after phase 5. The reworked brief's own "Still open for the owner" list is the next set to confirm.
2. (removed)
3. (removed)
4. How much of the ChatGPT plan Margin may use (the SIWC per-app cap).
5. Questions left over from phase 3:
   - Should OpenAI 401/403 map to no_api_key, so the app offers "Open Settings"?
   - Should the chat show "Stopping…" during Stop's 15 s write wait?
   - Should navigate-to-topic open the topic's notes or its first lesson?
   - Should non-agent jobs honour Retry-After uncapped? They're capped at 60 s now.
   - Should the title call's tokens count in the usage footer?
   - Wording: the waiting banner, "Attach / Add files…", and the Settings → Devices offline note.
6. Providers Q1: Claude web fetch on or off? Recommended: on, max_uses 8, 50k content tokens.
7. Providers Q2: a per-chat web on/off chip? Recommended: no.
8. Providers Q3: should titles and condense run on the default provider's cheapest model? Recommended: `claude-haiku-5-5` on Claude, the chat model elsewhere.
   - Providers' answer to item 5: OpenAI 401/403 maps to the new `auth_failed` code.
   - Default Claude model: `claude-opus-5-5`.

## Next steps

1. Confirm the reworked phase 4 brief's remaining owner items. Then run the phase 4 workflow from `docs/agent-phase4-brief.md` (first commit, then areas 4-S, 4-core, 4a desktop, 4b iOS and 4c Android).
2. In parallel, run the SIWC spike (`docs/agent-providers-brief.md` D11). The owner runs it by hand.
3. Get the answers to Providers Q1–Q3. Have Fable re-check its brief against phase 4 as merged, then run the providers workflow.
4. Phase 5: have Fable write the brief, then build it.
5. Rebuild and reinstall the app and service, update the iOS simulators, and hand over for the owner's hands-on run.

## How the build runs

Each phase is one workflow of parallel subagents:
1. A shared-first commit on `claude/agent-build`.
2. Areas built in parallel worktrees with exclusive file ownership. Each starts with `git checkout -B agent/pN-<area> claude/agent-build`.
3. Integrate and run the e2e tests.
4. Two reviewers.
5. A fixer.

The reusable script is `margin-agent-phase` (args: phase, base, extra, first, areas[{key,prompt}], e2e, mobileCheck, lenses, resumeNote). If it isn't in the new project, recreate it from this description.

**Before each phase**, the architect (a Fable model agent) reviews or writes the brief, and blockers go to it before the owner. Product decisions still go to the owner.

## Rules (from the owner)

- Never narrow scope on your own ("for now", "desktop only", "later"). Ask instead.
- Every UI element must be fully functional; no placeholders.
- Tests use in-memory stores and fakes only. Never call real model APIs. Never touch the running margin-service or `~/Library/Application Support/Margin`.
- API keys and tokens never go on devices.
- Commit messages end with a `Co-Authored-By:` line.

## Build gotchas

- PATH: `export PATH="$(ls -d ~/.rustup/toolchains/stable-*/bin | head -1):$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"`
- Run `dx` from the workspace root with `-p margin-app`. Run from a crate directory, it panics.
- iOS check: `cargo check -p margin-app --target aarch64-apple-ios-sim --no-default-features --features mobile`
- Android: `ANDROID_NDK_HOME=$ANDROID_HOME/ndk/27.3.13750724`, `JAVA_HOME=/opt/homebrew/opt/openjdk@17/libexec/openjdk.jdk/Contents/Home`. Android has never been built for the agent; phase 4c must run `dx build -p margin-app --platform android`.
- Release app: `dx bundle -p margin-app --platform desktop --package-types macos --release`, then `codesign --force --deep --sign -`.
- Every new service binary path triggers a macOS Keychain "Always Allow" prompt for media-signing-key. The owner has to click it.
- Known flaky test: `sources::unsupported_oversized_and_misfiled_uploads_are_rejected`. The service closes the connection before the oversized upload finishes. Needs fixing.

## Owner's current test install

- `/Applications/Margin.app` and the service, both built from worktree `.claude/worktrees/install`. That's phases 1–2 plus the overflow fix.
- The iOS simulators still run an older app.
