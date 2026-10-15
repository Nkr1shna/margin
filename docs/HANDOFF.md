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
| `docs/plan-dogfood.md` | **D (dogfood)**: Margin as the knowledge base for building Margin. `crates/mcp` (`margin-mcp serve/login/ingest/ingest-dir/purge`), `.mcp.json`, `/margin-handoff`, the automatic `SessionEnd` summary, `scripts/{ingest-docs,claude-session-summary}.sh` (the backup script is now `deploy/scripts/backup-margin.sh`); merged with R1 (live once the owner installs the service and runs `margin-mcp login`) |
| `docs/plan-deploy.md` | **E (deploy)**: dev and prod as Orchard-managed Linux (Ubuntu 24.04, systemd) VMs on the Mac mini (first built as macOS 26 VMs) (`devmargin.` / `margin.krishnanelloore.com`), everything durable on a host folder shared into each VM (Orchard re-clones the guest disk on every restart), `deploy/scripts/{provision-guest,deploy,backup-margin,restore-into-dev}.sh`, systemd and cloudflared templates, `.github/workflows/{test,build-service,deploy-dev,promote-prod,apps}.yml` on the host runner (`[self-hosted, macOS, ARM64, margin]`), `GET /api/version`, `environment` in `service.toml`, `margin-service scrub-restored`, Prod / Dev / Custom on the sign-in screen, `margin-eval --service-url` (never prod). Built on `agent/e-deploy`; the owner's steps are §7 and the GitHub settings §4 |
| `docs/plan-linux-service.md` | **LX (Linux service)**: inference on Linux VMs; built, and since `chore/linux-only-service` the only service platform (the macOS backends are removed, see its status line). Built on `agent/lx-embed`: the first commit (`margin_embed::Backend` + per-backend `embedder_version`, `ModelFiles.backend`, per-backend download manifests, `OnnxOptions`, the `onnx`/`linux-readers`/`heic`/`speech-onnx` features and the Linux dependency table, `PipelineConfig.embedder`, `service.toml` `embedder`/`secrets`, the dense leg's `embedder = $embedder` conjunct, rows stamped with `Embedder::version`) and LX-embed-text (`OnnxEmbedder` text path on `ort` CPU with `PreferCPU`, real batching, `embed_pipelined`, idle-unload scaffold). Media, speech, service wiring, sandbox, evals and deploy areas follow §6 |
| `docs/agent-rag-architecture.md` | **R1** (RAG pipeline): target architecture, eval suite, build brief §6; §2.2 lists the SurrealDB 3.3.4 deviations the build found |

## Status

- **Phase 1** (agent loop, chats, device tool transport): merged, reviewed and fixed.
- **Phase 2:** merged, reviewed and fixed (34021c9).
- **Phase 3** (job tools, Stop awaiting writes, Retry-After waiting, model titles, app-control tools, chat attachments, mobile reconnect): merged, reviewed, 6 findings fixed (a615637). 527 tests at integration. Clippy is clean, and the desktop and iOS-sim builds pass.
- The chat thinking-block overflow fix is merged (420bc16).
- **R1 (RAG pipeline) + phase 4 app areas + D (dogfood)**: integrated on `claude/agent-build` (2026-10-14). Merged `agent/r1-{embed,store,retrieve,ai,trace,evals}`, `agent/p4-{core,desktop,ios,android}`, `agent/d-mcp`; the integration commits wire the areas together (see their messages) and add `crates/service/tests/r1_e2e.rs` (§6.4 e2e 1–6). `cargo test --workspace` green twice, clippy and fmt clean, iOS-sim check and `dx build` for Android and desktop pass. 779 tests pass, 1 ignored (the CI eval gate, e2e 7, waiting on the recordings).
  - **Waiting on the owner (by hand, on the service Mac):** `margin-eval record-embeddings`, `record-generations` and `run --suite retrieval --variant baseline --write-baseline`, committed; then un-ignore `crates/evals/tests/gates.rs` (e2e 7). Back up the data folder before installing (the one-pass `passages-v1` migration runs at first start; `deploy/scripts/backup-margin.sh`). A real course from a real PDF with traces visible through `GET /api/traces`. Then `cargo install --path crates/mcp`, `margin-mcp login`, `margin-mcp ingest-dir docs PLAN.md`.
  - **Every Claude Code session's summary now goes to Margin automatically** (owner decision): the `SessionEnd` hook in `.claude/settings.json` (`scripts/claude-session-summary.sh`, fails quietly without `margin-mcp`, a sign-in or the service).
  - **Not done here, with reasons:** phase 4's service side (4-S: folder files as sources, the sync route, `FolderIndex`) and 4-agent are still to build, rebriefed on R1's `ingest`/`Retriever` (§6.6), and with them D13's service e2e 1–7; R1-app (Settings pipeline/model slots, Passages locators) waits for nothing now and can start; `ModelInfo.context_window` comes with providers (retrieval keeps its window table until then); `view_picture` pixels need `ToolOutput.images` (4-agent); the iOS folder picker and the macOS/iOS/Android manual runs of D13 need the owner's hands-on run; the Linux desktop target isn't checked (no cross toolchain here).
- **Round 2: 4-S + 4-agent + R1-app**: integrated on `claude/agent-build` (2026-10-14). Merged `agent/p4-service`, `agent/p4-agent`, `agent/r1-app`; the shim for `ToolOutput::with_image` is gone, D13's service e2e 1–7 run un-ignored (`crates/service/tests/{folders,sources}.rs`, including `sync_folder` to the holding device and `view_picture` pixels). Shared: `routes::PIPELINE_VARIANTS`, `pipeline::{PipelineVariants, NamedVariant, STAGE_KINDS}` (moved from the service) with `stage_name`/`with_stage`/`with_stages_of`, `Citation.section_path` (set by `pipeline::hit_citation`, shown on the Passages card). Settings → AI → Pipeline has the variant and per-stage pickers over `GET /api/pipeline/variants` (Save stages / Undo changes; routes and budget kept), with the fake serving the same and render/fake-service tests. `cargo test --workspace` green twice (831 pass, 1 ignored: the eval gate), clippy and fmt clean, iOS-sim check and `dx build` for Android and desktop pass. `agent/lx-embed` is not merged (still building).
  - **Pending (owner decision, not applied here):** `main` becomes the base and deploy branch: build-service/deploy-dev on pushes to `main`, test.yml on all pushes and PRs into main, promote-prod/apps refusing refs other than main, and `docs/plan-deploy.md` §4 and the runbook to match (free plan: no branch protection or prod required reviewers, so prod is gated by the manual "Run workflow" only; fork-PR workflows disabled; environments dev/prod accept main and claude/agent-build). The workflow edit was blocked by this session's permission classifier and needs the owner's approval.
- **LX-embed**: integrated on `claude/agent-build` (2026-10-14). Merged `agent/lx-embed` (bb2bf19) with no conflicts against the retries, change feed, folders and variants work; the dense leg's `embedder = $embedder` keeps Core ML rows (`EMBEDDER_VERSION` = `Backend::CoreMl.embedder_version()`) and the fake embedder's (it stamps the same version) visible. Fixed a parallel-test race in `crates/embed/tests/corpus.rs` (a shared temp folder). `cargo test --workspace` green twice (852 pass, 1 ignored), `cargo test -p margin-embed --features onnx`, clippy (also with `onnx,linux-readers,speech-onnx`) and fmt clean, iOS-sim check and `dx build` for Android and desktop pass. Left for LX-service: one-pass re-embed + `rewrite_vectors`, `load_embedder`, per-backend remote sizes, the registry check for the `embedder` stage, `Document.extractor`, Linux sandbox and tool names, per-OS config paths.
- **LX round 2 (sandbox, speech, media, service)**: integrated on `claude/agent-build` (2026-10-14). Merged `agent/lx-sandbox` (cc0ab4c), `agent/lx-speech` (3ecff55), `agent/lx-embed-media` (c867f6e), `agent/lx-service` (5edaca1). Conflicts only in `crates/service/src/{config,secrets}.rs` (dev's `secrets_file`/`FileStore` vs LX-service's superset: kept LX-service's `open_store`/`secrets_path`/`location()`/privacy warning and dev's deploy-script test; one `FileStore`). Wiring: `speech-onnx = ["onnx", "dep:parakeet-rs"]`, `transcript_parakeet` module; `video::sandbox_mode` → `SandboxMode::native()`; Tools rows use `bubblewrap::{find_bwrap, INSTALL_COMMAND, USERNS_SETUP}`, carry `MARGIN_TTS_SCRIPT`, and on Linux take tesseract/ffmpeg/pdfium from `margin_embed::find_*` (`margin_video::detect_tools_with`); `set_tools_dir(data_dir/tools)` at serve; `EmbedTools::new(models_dir)` transcribes with Parakeet on Linux (loaded on use, swept every minute by housekeeping); `SourceTools::{speech_model, no_transcriber, ocr_missing, sweep}`; OCR warning on images/scanned PDFs without tesseract. Wire: `EmbeddingModel.{speech, reindex}`, `routes::SPEECH_MODEL{,_DOWNLOAD}`; Settings → Sources shows the speech model (download/progress/remove) and "Re-indexing N of M documents"; `margin-service download-model` also fetches Parakeet on Linux. `STAGE_KINDS` has "embedder": the service stamps its own (`PipelineConfig::with_embedder`), Settings shows it read-only, `with_stages_of` keeps it. Settings refuses voices cosette/jean (400 `voice_not_allowed`). Evals USAGE names the secret store per OS. Linux deploy facts are in `docs/plan-linux-service.md` §5.1 (for the Mac mini session; `deploy/` untouched). Fixed a flaky trace-rollup snapshot in `tests/chats.rs`. Not done: no `aarch64-unknown-linux-gnu` target or sysroot here, so the Linux-only arms compile first on the Linux runner (the service's `linux-readers`/`speech-onnx` features check most of them on macOS); lavapipe timing and real-model/real-tool runs need a Linux guest.
- **Phase 4** (attached folders on desktop, iOS and Android): all areas built and integrated; the manual runs of D13 (macOS 200 MB scanned PDF, iOS iCloud folder, Android) need the owner.
- **Providers** (owner decision 2026-10-14): built and integrated on `feat/providers` (2026-10-14). Merged `agent/prov-{openai,claude,agent,service,app,evals}` (no conflicts). Integration: the service stores/replays through `margin_agent::replay` (its `turns::wire` copy is gone); the window/compaction/step hooks live in `impl TurnSink for ServiceSink`; agent steps are traced with `AGENT_PROMPT_VERSION`; `retrieval::route_window` resolves the route's model through `model_for` (the search tool uses the chat's own provider) and packs `floor(window × share) − output reserve` (D13; pinned eval windows reserve nothing; `ask::write_window` is gone); `margin_ai::{write_lesson_with, write_scene_with, plan_course, rerank}` take `&dyn TextModel` (new defaulted `TextModel::stream_json`: strict `text.format` on Responses, schema in words on Claude), `margin_ai::clean` is public and `engines::on_text` is gone; `globe` is in the shared icon set; the Claude prefix gate runs. New e2e: `crates/service/tests/claude_e2e.rs` (the phase-1 chat and R1's course through the real `ClaudeClient` against a fake Messages server: tools, byte-identical replay with thinking signatures, web search with card and citations, Stop with a pending server tool, restart and Continue, the course packed to the fake's window less the reserve, traces with `provider: "claude"` and prices), a ChatGPT chat after sign-in through the fake auth server with web search and `cost_usd: None` (`tests/providers.rs`), an OpenAI web search (`tests/agent_e2e.rs`). `cargo test --workspace` green twice (1010 pass, 4 ignored: the eval gate, lavapipe, two Linux-only bubblewrap tests), clippy and fmt clean, iOS-sim check and `dx build` for desktop and Android pass.
  - **Review fixes (2026-10-14):** Claude `Usage.input_tokens` counts cache reads and writes (pricing, traces and the compaction threshold were short); a refusal's `stop_details.category` reaches the Error item as "(category)" (`Incomplete { reason: "refusal:<category>" }`); account limits end a turn with D7's fixed texts; ChatGPT forced refreshes pass the refused token (one refresh for concurrent 401s, none before `earliest_refresh_at`), a code exchange in flight can't undo a sign-out or a newer sign-in, the authorize URL carries no `id_token_hint`, a failed re-sign-in shows `SignedIn { last_error }`; the app's fake refuses an empty slot as `no_model`, slot lists refetch after a new key/base URL/account, the usage footer counts fetches without searches. Web search stays `web_search_20260318` (the live docs' newest; the claude-api skill's cached `20260209` is older).
  - **Left:** `gpt-5.6-*` windows are not in `windows.rs` and `{"type":"web_search"}` inside `allowed_tools` is unverified live; the screens' visual check (`render_screens` with `RENDER_PROVIDER`/`RENDER_CHATGPT`) and real-provider runs need the owner.
  - **Shape:**
    - Two traits in `crates/ai/src/model.rs`: `TextModel` and `Provider`. `ResponsesClient` and a new `ClaudeClient` implement both.
    - History stays verbatim per provider (`Chat.provider` is immutable), with Claude-specific replay.
    - Web search and fetch show as tool rows, a `Card::WebResults` card and cited Sources.
    - New error codes: `auth_failed`, `spend_limit_reached`, `plan_limit_reached`, `plan_ineligible`.
    - Three secrets in the service's secrets file.
    - The SIWC OAuth runs in the service, and the owner signs in in a browser.
  - **Areas:** P-ai-openai, P-ai-claude, P-agent, P-service and P-app, after a first commit.
  - **Sequencing (Fable):**
    1. Phase 4 first.
    2. The SIWC spike runs in parallel with phase 4. It's a throwaway example the owner runs by hand, never merged.
    3. Then providers.
    4. Then phase 5.
  - **Spike outcome:** if registration or inference fails, ChatGPT is dropped (unselectable) and the rest goes ahead. If only web search fails, ChatGPT ships without it.
- **Linux-only service** (owner, 2026-10-14: "now that linux is up can you remove macos related service leftovers?"): on `chore/linux-only-service`. Removed from the service side: the Core ML embedder (`Backend::CoreMl`, `coreml.rs`, its downloads and manifests), the PDFKit/Vision/ImageIO/AVFoundation readers, FluidAudio (speech and narration), `sandbox-exec`, the Keychain secret store, the LaunchAgent branch of `backup-margin.sh`, every macOS `cfg` arm and the `objc2-*`/`keyring`/`libc` dependencies of the service crates; the `onnx`/`linux-readers`/`speech-onnx` features are gone (all always built). Defaults are the Linux ones on every OS (`onnx-q8`, the secrets file, `/etc/margin`, `/var/lib/margin`, bubblewrap, `margin-tts`); the service still builds and tests on macOS, where rendering refuses with "only available on Linux". Core ML-stamped passages are re-embedded by the existing one-pass re-embed; the pipeline's default `embedder` stage keeps the name `coreml` so stored hashes keep their meaning. The apps are untouched (their own device-token Keychain use included).
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

1. Review the R1 + phase 4 integration, round 2 included (two reviewers on the §6.5 and phase 4 lenses, then a fixer). Merge `agent/lx-embed` when it finishes.
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
- Fixed: `sources::unsupported_oversized_and_misfiled_uploads_are_rejected` was flaky because the service answered 413 while the client was still sending; refused bodies are now drained (bounded) before the answer.

## Owner's current test install

- `/Applications/Margin.app` and the service, both built from worktree `.claude/worktrees/install`. That's phases 1–2 plus the overflow fix.
- The iOS simulators still run an older app.
