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
| ✅ | **Deploy**: dev and prod on the Mac mini (Tart/Orchard), Cloudflare tunnels, CI per platform (Linux runner `margin-ci`, macOS for the app), CD per product, local Linux check | `docs/plan-deploy.md` |
| ✅ | **Linux service**: ONNX EmbeddingGemma, pdfium, tesseract, ffmpeg, Parakeet, pocket-tts, bubblewrap, file secrets, systemd guests | `docs/plan-linux-service.md` |
| ✅ | **macOS service leftovers removed** (Core ML, PDFKit/Vision/ImageIO/AVFoundation, FluidAudio, `sandbox-exec`, Keychain store, launchd): Linux is the only service platform; the apps stay on Mac, iPhone, Android. Core ML-era passages are re-embedded by the one-pass re-embed | `docs/plan-linux-service.md` §0 |
| ✅ | **Phase 5 — hardening**: injection guard (`text::for_model` at every edge, injection suite), one retry policy with visible waits, compaction trace markers, per-device rate limits and body limits, checked backups + schema guard + restore drill, alerts from traces + `perf`, iOS continued processing for folder syncs (on `feat/p5`) | [`docs/phase5-brief.md`](phase5-brief.md) |
| ✅ | **Switchable providers**: OpenAI key, Sign in with ChatGPT, Claude; per chat, three model slots each, web search/fetch with citations | `docs/agent-providers-brief.md` |
| ✅ | **L1 — accounts and principals**: people (the `people-v1` migration makes the owner), invitations by GitHub login, groups, agents owned by a person with their own tokens (`margin-mcp token`), rows owned and private by default, per-principal events, Organisation pane in the app; a second *untrusted* account waits for L2 | `docs/l1-brief.md` |
| ✅ | **R2 — rerank, query rewriting, prompt-version tracking**: `listwise-small` and `small-expand-v1` on the Small route, the `onnx-crossencoder` stage (ms-marco-MiniLM, downloaded on demand), `written_with` on lessons/proposals/videos, the prompt catalog and `evals/prompts.lock` in CI, recorded reranks/rewrites in the eval. Every stage stays `"none"` by default: variants until the eval promotes them | `docs/r2-brief.md` |

## In progress

| | Item | Notes |
|---|---|---|
| 🟡 | **L2 — access control inside search**: hierarchical groups, grants and denials, agents limited by their owner, private knowledge and publishing, audit log | `docs/l2-brief.md`; five areas building on `feat/l2` |
| 🟡 | **App crash logging and the first panic fixes** (panic hook, log file, "quit unexpectedly" notice, the rename-while-saving crash) | branch `fix/app-crash-logs` |
| 🟡 | **Provider settings defaults** (model slots filled, no Keychain copy, sensible default provider) | branch `fix/provider-defaults` |

## Next (in order)

Each step is a vertical slice: a person can use it end to end on desktop, iPhone and Android when it lands. Order: keep the app stable, finish who-sees-what, make the knowledge base usable day to day, get more knowledge in, then run it for a company and ship to prod, then the live features.

| | Slice | Items | Plan | Notes |
|---|---|---|---|---|
| 🟡 | **S0 — Stability** (from the quality reviews, `margin-quality` page) | **Done:** app crash reports, logs and the borrow-across-await fixes (#33); CI fails closed, SHA-pinned actions, Dependabot (#32); corrupt drafts and config set aside, offsite backup fails on zero copies, README (#34); eval gate on (#23). **Now:** request ids and an info-level request log with error reference ids; supervised background workers and a real `/health`; required migrations complete before readiness, no cleanup after a failed one; a mutex-poison policy (ONNX load outside its lock); bounded work queues with a 503 on overload; the 24 fixed sleeps in tests replaced with condition waits; cargo-deny and workspace `[lints]` against unwrap/expect/panic; one packaged desktop smoke test against dev. **Then (formerly "next month"):** idempotent cascade deletes now, transactional deletes with an outbox after; an `ErrorCode` enum shared by the service and the app, and a route-table contract test plus a shared behaviour corpus between the app's fake and the service; newtype ids; paging for `/api/library` and narrower retrieval catalog loads, with spans around retrieval, rerank, provider calls and render; split files over 2,000 lines as they are touched, `fake.rs` first; property tests for event application, upload offsets and path normalisation; `nextest` with per-test timeouts, a scheduled full-platform CI run, the release build in CI; keep the last N dev builds; `openat` folder reads, HTTPS for non-local service URLs, a `// SAFETY:` audit; a schema-upgrade rehearsal from a real backup | `margin-quality` review page | Order set by the blast-radius pass |
| ⏳ | **S1 — Find and manage your knowledge** | Search inside files and chats with highlighted matches on the EmbeddingGemma hybrid pipeline (not plain text); a Files screen (open, rename, download, sort); trash with undo and restore; move and reorder topics, lessons and notes; copy an answer and a code block; toasts, help, feedback and the version everywhere | `docs/basics-brief.md` | After L2 (results respect the reader) |
| ⏳ | **S2 — Bring knowledge in** | Office formats (docx, pptx, xlsx); a web page as a source; the share sheet on iOS and Android; repository source and GitHub permission sync (L6) | `docs/basics-brief.md`, `docs/plan-live-context.md` | L6 moves up: code comes in this way |
| ⏳ | **S3 — Run it for a company, and ship** | Usage per person on L2's audit log; offboarding session, then removal with content reassignment and per-person export; **prod on Linux** (`margin-prod` guest) with the infra review's settings and runner fixes | `docs/basics-brief.md`, `docs/plan-deploy.md` | The point where a second company can be invited |
| ⏳ | **S4 — Learn it properly: Q, serious quizzes** | Graded answers, code editor, review-only code grading | `docs/phase-q-brief.md` | Needs L2's per-person attempts |
| ⏳ | **S5 — L3 inbox and @mentions** | For people and agents, with push notifications and deep links | `docs/plan-live-context.md` | |
| ⏳ | **S6 — L4 meetings** | LiveKit, per-speaker transcripts into knowledge | `docs/plan-live-context.md` | Zed's collab as inspiration only (GPL) |
| ⏳ | **S7 — L5 voice-first** | ElevenLabs | `docs/plan-live-context.md` | |

## Later / ideas

- 🗓 E-book formats (`.epub`) once an extractor exists.
- 🗓 Audio and video in folders.
- 🗓 R2's variants as defaults (listwise rerank for lessons, rewriting, the cross-encoder for agent search) once the eval earns it (`docs/r2-brief.md` §6); a `by-purpose` reranker if both rerankers win.
- 🗓 Agents over MCP for other companies' tools (after L1's agent tokens).

## Waiting on the owner

- 👤 `margin-eval record-embeddings` + baseline, then un-ignore the CI eval gate (`crates/evals/tests/gates.rs`).
- 👤 R2's recordings, in this order: `margin-eval record-rewrites --variant rewrite-small` → `margin-eval record-embeddings` (adds the rewrites' vectors) → `margin-eval record-reranks --variant rerank-small`; commit them so the R2 CI gate runs instead of skipping; then the A/B runs and `compare` of `docs/r2-brief.md` §6, and flip a default in Settings only if its rule passes.
- 👤 R2 on the dev guest: `margin-service download-model --reranker ms-marco-minilm-l6-v2`, a real `search` with `rerank-xenc` showing `rerank_ms` in `GET /api/traces`.
- 👤 `bge-reranker-base`: needs ~1.3 GB loaded (1266 MB RSS measured, over the 400 MB budget MiniLM meets). It stays an opt-in variant (`rerank-xenc-bge`) for non-English corpora; decide whether to keep offering it or drop it.
- 👤 Hands-on runs: phase 4's manual D13 checks (macOS scanned PDF, iOS iCloud folder, Android), the providers screens, a real course from a real PDF.
- 👤 First real-provider runs (OpenAI, ChatGPT, Claude); ChatGPT's per-app cap doesn't list Margin yet.
- 👤 Phase 5 manual checks (`docs/phase5-brief.md` §5): the iPhone continued task (iOS 26, a 1 000-file iCloud folder synced with Margin closed, expiry → Paused; the task identifier may need a `.*` wildcard), `restore-drill.sh dev` (first run with `--hard-stop`), `perf` against dev, an alert end to end, one 429 per provider.
- 👤 Phase 5 D3d fixtures: the live 400 texts of a compaction rejection from OpenAI and Claude, copied into the fake servers so `rejects_compaction` is tested against real text.
- 👤 Licence reads before selling: EmbeddingGemma terms; pocket-tts voices (cosette and jean are blocked as non-commercial).
