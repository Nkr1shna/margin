# Margin roadmap

The one place to see what's built, what's in flight and what's next. Update it in the same PR that changes the status. Detail lives in the linked plans; `docs/HANDOFF.md` keeps the build log.

**Product:** a "Live context" app for a company's projects: humans through a voice-first GUI, agents through context, with learning (courses, lessons, quizzes) built in. Margin is a startup and the vision moves, so designs stay pluggable and every RAG/prompt/model change is measured by evals.

**Branches:** work goes to a feature branch, then a PR **squash-merged** into `dev` (deploys `devmargin.krishnanelloore.com`; `smoke-dev` comments on the PR). `dev` stays shippable (required CI, smoke-dev after each deploy, revert-on-red via the **revert-pr** workflow). To ship, the **promote-release** workflow opens a PR into `release` (deploys `margin.krishnanelloore.com`) at a dev commit whose smoke-dev passed, listing the PRs it brings; dev PRs squash, release PRs merge with a merge commit; merging tags `release-<date>-<sha>` and labels the PRs `shipped`. Unfinished features ship dark behind a setting (`needs-flag`); `verified-on-dev` is an optional label. Which PR is where: `docs/RELEASES.md`; how: `docs/plan-deploy.md` §4a. `main` is unused. Before a PR: `deploy/local/linux-check.sh`.

Status: ✅ done · 🟡 in progress · ⏳ next · 🗓 later · 👤 waiting on the owner

## Done

| | Feature | Where |
|---|---|---|
| ✅ | **Phases 1–3**: agent loop, chats, device tools, job tools, Stop, Retry-After, model titles, app-control tools, chat attachments, mobile reconnect | `docs/agent-architecture.md` |
| ✅ | **R1 — RAG pipeline**: pluggable stages, SurrealDB HNSW + full-text hybrid search, incremental re-index, budgets by model window, planner, traces in SurrealDB time-series tables, eval suite (`margin-eval`), context API | `docs/agent-rag-architecture.md` |
| ✅ | **Phase 4 — attached folders** on Mac, iPhone, Android: allow-listed types (docs, PDFs, images), sha256 sync into the Sources pipeline, unreadable files greyed out, folder search for the agent | `docs/agent-phase4-brief.md` |
| ✅ | **Dogfooding**: `margin-mcp` (Claude Code reads and writes Margin), session summaries saved automatically | `docs/plan-dogfood.md` |
| ✅ | **Deploy**: dev on the Mac mini (Tart/Orchard), Cloudflare tunnels, CI per platform (Linux runner `margin-ci`, macOS for the app), CD per product, local Linux check. Prod is not live: the `margin-prod` guest exists and takes its first release at the end of W2 | `docs/plan-deploy.md` |
| ✅ | **Linux service**: ONNX EmbeddingGemma, pdfium, tesseract, ffmpeg, Parakeet, pocket-tts, bubblewrap, file secrets, systemd guests | `docs/plan-linux-service.md` |
| ✅ | **macOS service leftovers removed** (Core ML, PDFKit/Vision/ImageIO/AVFoundation, FluidAudio, `sandbox-exec`, Keychain store, launchd): Linux is the only service platform; the apps stay on Mac, iPhone, Android. Core ML-era passages are re-embedded by the one-pass re-embed | `docs/plan-linux-service.md` §0 |
| ✅ | **Phase 5 — hardening**: injection guard (`text::for_model` at every edge, injection suite), one retry policy with visible waits, compaction trace markers, per-device rate limits and body limits, checked backups + schema guard + restore drill, alerts from traces + `perf`, iOS continued processing for folder syncs (#25) | [`docs/phase5-brief.md`](phase5-brief.md) |
| ✅ | **Switchable providers**: OpenAI key, Sign in with ChatGPT, Claude; per chat, three model slots each, web search/fetch with citations; provider defaults per provider and a default provider (#38). ChatGPT sign-in finishes on the machine that runs the service, now a headless guest (In progress) | `docs/agent-providers-brief.md` |
| ✅ | **L1 — accounts and principals**: people (the `people-v1` migration makes the owner), invitations by GitHub login, groups, agents owned by a person with their own tokens (`margin-mcp token`), rows owned and private by default, per-principal events, Organisation pane in the app; the second account's hands-on run is in Waiting | `docs/l1-brief.md` |
| ✅ | **R2 — rerank, query rewriting, prompt-version tracking**: `listwise-small` and `small-expand-v1` on the Small route, the `onnx-crossencoder` stage (ms-marco-MiniLM, downloaded on demand), `written_with` on lessons/proposals/videos, the prompt catalog and `evals/prompts.lock` in CI, recorded reranks/rewrites in the eval. Every stage stays `"none"` by default: variants until the eval promotes them | `docs/r2-brief.md` |
| ✅ | **L2 — access control inside search**: the reader's two conjuncts (`access CONTAINSANY $paths AND deny CONTAINSNONE $deny`) inside both legs of the hybrid query, by-id reads, the change feed and ⌘K; exact group membership (no prefix rule); private by default with `passages-access-v1`; publishing (`PUT /api/audience/…`) with its cascade; derived lesson audiences; agents within owner ∩ ceiling and nameable in `deny`; the audit trail and the host-only export; the Audience control and Audit view in the app; the leakage gate in CI (#46) | [`docs/l2-brief.md`](l2-brief.md) |
| ✅ | **S0 so far**: app crash reports, logs and the borrow-across-await fixes (#33, #44); CI fails closed, SHA-pinned actions, Dependabot (#32); corrupt drafts and config set aside, offsite backup fails on zero copies, README (#34); the eval gate on the recorded corpus (#23); CI prunes the runner's cargo cache (#47); the APK step skips on a runner without the Android SDK (#48) | `margin-quality` review page |

## In progress

| | Item | Notes |
|---|---|---|
| 🟡 | **Sign in with ChatGPT on a headless service**: the loopback callback sits on the service host, now a Linux guest with no browser | branch `fix/chatgpt-signin-remote`, PR next; then #43 (jsonwebtoken), which edits the same `providers.rs` code |
| 🟡 | **Office readers** (docx, pptx, xlsx in `crates/embed`) | branch `agent/b-office`; built, parked until `cargo-deny` lands (W1), merged unwired, wired in S2 |
| 🟡 | **Voice UI study**: the orb/scratchpad mockups (pearl, pond, night) | prototype only, no app code; the reviewed mockup S7 needs (plan Q17) |

## Next (in order)

Four waves first, fixing what every later feature writes against; then the slices. Each wave starts when the previous one's exit check passes, and nothing starts that a later wave would rewrite. Order and evidence: the blast-radius plan. Each slice is vertical: a person can use it end to end on desktop, iPhone and Android when it lands.

| | Wave / slice | Items | Exit check |
|---|---|---|---|
| ✅ | **W0 — Land L2** | #46, #44, #36 merged; the owner's GitHub settings applied (rulesets split, 0 approvals, `smoke-dev` required on `release`, Actions may open PRs, a `prod` reviewer, secret scanning and push protection, Dependabot alerts). Left for the owner: install the post-#33 Dev build, `check-data` on the newest dev and prod backups, widen the `claude-code` agent's ceiling (Waiting) | Done once dev deploys L2 and `smoke-dev` is green; eval gates 6/6 |
| ⏳ | **W1 — Subtract and scaffold** | Condition waits for the fixed test sleeps (24 in `crates/service/tests`, 8 in `src`); a root `clippy.toml` guard lint replacing `guard_tests.rs`, workspace `[lints]` (unwrap, expect, panic deny; indexing warn), `cargo-deny`; delete the legacy retrieval path and the `passages-v1` conversion (after the backup check); finish Sign in with ChatGPT, then #43; #39, #41; the crash trail on the packaged app (a debug-only panic must leave `crash-*.log` and an `.ips`; the delayed-save typing case); mutex-poison policy, ONNX load outside its lock; merge `b-office` unwired | CI enforces lints and deny; no `legacy` in `retrieval.rs`; no fixed sleep before an absence check |
| ⏳ | **W2 — Shared contracts** | `WriteGuard` parameter on every `*_locked` function (first, alone); one migration ledger (version, pre-migration backup, required tables and readiness from one list; a failed conversion never records completion; the interrupted-migration test first); `ErrorCode` enum and the `ApiFailure` sum type, then request ids, an info-level request log, error references; a worker that returns or panics ends the process (`/health` unchanged), `recover()` re-queues `Indexing` documents; disk headroom where uploads are staged. Then the first promote-release to prod, right after the ledger rehearsal | Rehearsal from a real backup passes; no string error codes; workspace tests green |
| ⏳ | **W3 — App tests on the real service** | `margin-service` `test-support` feature; port the 47 fake-service tests; keep render fixtures and a fault-injection proxy; delete the fake's route handlers (`fake.rs`, `fake_l2.rs`) | No app test starts `FakeService` |
| ⏳ | **S1 — Find and manage your knowledge** (`docs/basics-brief.md`) | Basics' first commit, trimmed to S1. Search inside files and chats with highlighted matches on the EmbeddingGemma hybrid pipeline; a Files screen (open, rename, download, sort; time `/api/library` at 10k sources first); trash with undo and restore (cascades emit the events of rows already deleted); move and reorder topics, lessons and notes; copy an answer and a code block; toasts, help, feedback and the version everywhere; the rename input disabled while saving. L2 leftovers while `search.rs` is open: batch the re-stamp under the write lock (5,000 passages in 3.3 s, budget 2 s), drop the `retain(can_read)` re-checks | Brief e2e 1–4, 9, 10; `[search]` gates |
| ⏳ | **S2 — Bring knowledge in** (`docs/basics-brief.md`, `docs/plan-live-context.md`) | Wire Office (odt/odp/ods when cheap, legacy formats refused); a web page as a source (SSRF guard, W2's disk check); the share sheet: Android `SEND` (decide jni 0.22, #42, first), iOS/macOS document types + `margin://` + a Shortcut; L6 repository source and GitHub permission sync | Brief e2e 5, 8 |
| ⏳ | **S3 — Run it for a company** (`docs/basics-brief.md`, `docs/plan-deploy.md`) | Usage per person on L2's audit log; offboarding session, then removal with content reassignment and per-person export; prod hardening: the runner split and ephemeral `margin-ci` (H5, H6), `smoke-dev` on the promoted sha and `deploy-prod` refusing without it, a `smoke-prod` twin, an ancestry check on manual deploys | Brief e2e 6, 7; a second company can be invited |
| ⏳ | **S4 — Learn it properly: Q, serious quizzes** (`docs/phase-q-brief.md`) | Graded answers, code editor, review-only code grading | Per-person attempts on L1's principals, audience from L2; the brief (written before L1) re-checked first |
| ⏳ | **S5 — L3 inbox and @mentions** (`docs/plan-live-context.md`) | For people and agents, with deep links; push notifications are owner Q7 (recommended: badge in L3, push with L4) | |
| ⏳ | **S6 — L4 meetings** (`docs/plan-live-context.md`) | LiveKit, per-speaker transcripts into knowledge | After the `livekit` crate build spike; LiveKit on a Linux VM with TURN (Q10); Zed's collab as inspiration only (GPL) |
| ⏳ | **S7 — L5 voice-first** (`docs/plan-live-context.md`) | ElevenLabs | After the ElevenLabs-in-webview spike and the reviewed voice UI study |

Dropped from S0 by the quality review: bounded queues with a 503 (disk headroom instead), a readiness `/health`, the route-table contract test and behaviour corpus (W3 replaces them), transactional deletes with an outbox (S1's cascade events replace it). Deferred: newtype ids (revisit at S3), paging `/api/library` (only if S1's measurement needs it). Any time, independent: property tests, `nextest` with per-test timeouts, a scheduled full-platform CI run, the release build in CI, keeping the last N dev builds, the restore drill on a schedule with a service-minted token, `deploy/**` in CI scope with `shellcheck`, one `deploy` concurrency group, job timeouts in `apps.yml`, splitting files over 2,000 lines as they are touched (`ui/settings.rs` next; `fake.rs` shrinks in W3 instead), `openat` folder reads, HTTPS for non-local service URLs, a `// SAFETY:` audit.

## Later / ideas

- 🗓 E-book formats (`.epub`) once an extractor exists.
- 🗓 Audio and video in folders.
- 🗓 R2's variants as defaults (listwise rerank for lessons, rewriting, the cross-encoder for agent search) once the eval earns it (`docs/r2-brief.md` §6); a `by-purpose` reranker if both rerankers win.
- 🗓 Agents over MCP for other companies' tools (after L1's agent tokens).
- 🗓 A true iOS share extension (a native app-extension target with an app group beside the dx build); LibreOffice headless for legacy `.doc .ppt .xls`; spend caps on the usage route; a GitHub-issue feedback webhook (`docs/basics-brief.md` §10).
- 🗓 Splitting the one service write lock once W2's `WriteGuard` makes the rule visible.

## Waiting on the owner

- 👤 Delete the two unused `MARGIN_SIGNING_P12_*` repository secrets.
- 👤 On the Mac mini: the runner split (CI under an unprivileged user, the deploy key under the owner's, H5) and ephemeral `margin-ci` without sudo (H6); the Android toolchain on the Mac runner so the APK step stops skipping (#48).
- 👤 Install the post-#33 Margin Dev build on the Mac that crashes (W1's crash trail starts there).
- 👤 `margin-service check-data` on the newest dev and prod backups; read the `migrations` line (W1 deletes the legacy path only once both record `passages-v1`).
- 👤 After L2 deploys: widen the `claude-code` agent's ceiling in Organisation → Agents, or publish the Margin docs topic, else the dogfood agent finds nothing.
- 👤 Hands-on runs: phase 4's manual D13 checks (macOS scanned PDF, iOS iCloud folder, Android), the providers screens, a real course from a real PDF; L1 (`docs/l1-brief.md` §8: watch `people-v1`, sign in on Mac and iPhone, invite a second GitHub account, an agent token used from Claude Code); L2 (`docs/l2-brief.md` §5: watch `passages-access-v1`, the second account sees exactly one published topic and loses it on deny without a reload, the Audit view on the phone).
- 👤 R2's recordings (embeddings are recorded, #23): `margin-eval record-rewrites --variant rewrite-small` → `record-embeddings` again (adds the rewrites' vectors) → `record-reranks --variant rerank-small`; commit them so the R2 CI gate runs instead of skipping; then the A/B runs and `compare` of `docs/r2-brief.md` §6 (the default flip is in Later).
- 👤 R2 on the dev guest: `margin-service download-model --reranker ms-marco-minilm-l6-v2`, a real `search` with `rerank-xenc` showing `rerank_ms` in `GET /api/traces`.
- 👤 `bge-reranker-base`: needs ~1.3 GB loaded (1266 MB RSS measured, over the 400 MB budget MiniLM meets). It stays an opt-in variant (`rerank-xenc-bge`) for non-English corpora; decide whether to keep offering it or drop it.
- 👤 First real-provider runs (OpenAI, ChatGPT, Claude); ChatGPT's per-app cap doesn't list Margin yet. Until the headless sign-in lands, ChatGPT sign-in against a guest needs an SSH port-forward (`-L 1455:127.0.0.1:1455`).
- 👤 Phase 5 manual checks (`docs/phase5-brief.md` §5): the iPhone continued task (iOS 26, a 1 000-file iCloud folder synced with Margin closed, expiry → Paused; the task identifier may need a `.*` wildcard), `restore-drill.sh dev` (first run with `--hard-stop`), `perf` against dev, an alert end to end, one 429 per provider.
- 👤 Phase 5 D3d fixtures: the live 400 texts of a compaction rejection from OpenAI and Claude, copied into the fake servers so `rejects_compaction` is tested against real text.
- 👤 Licence reads before selling: EmbeddingGemma terms; pocket-tts voices (cosette and jean are blocked as non-commercial).
