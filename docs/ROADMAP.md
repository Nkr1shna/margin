# Margin roadmap

The one place to see what's built, what's in flight and what's next. Update it in the same PR that changes the status. Detail lives in the linked plans; `docs/HANDOFF.md` keeps the build log.

**Product:** a "Live context" app for a company's projects: humans through a voice-first GUI, agents through context, with learning (courses, lessons, quizzes) built in. Margin is a startup and the vision moves, so designs stay pluggable and every RAG/prompt/model change is measured by evals.

**Branches:** work goes to `claude/agent-build` or a feature branch, then a PR **squash-merged** into `dev` (deploys `devmargin.krishnanelloore.com`; `smoke-dev` comments on the PR). Once tried on devmargin the PR is labelled `verified-on-dev`; the **promote-release** workflow cherry-picks verified PRs into one PR into `release` (deploys `margin.krishnanelloore.com`) and labels them `shipped` when it merges. Big features ship dark behind a setting (`needs-flag`). Which PR is where: `docs/RELEASES.md`; how: `docs/plan-deploy.md` §4a. `main` is unused. Before a PR: `deploy/local/linux-check.sh`.

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
| ✅ | **Switchable providers**: OpenAI key, Sign in with ChatGPT, Claude; per chat, three model slots each, web search/fetch with citations | `docs/agent-providers-brief.md` |

## In progress

| | Item | Notes |
|---|---|---|
| 🟡 | **Remove the macOS service leftovers** (Core ML, PDFKit/Vision/AVFoundation, FluidAudio, `sandbox-exec`, Keychain store, launchd) | Linux is the only service platform; the apps stay on Mac, iPhone, Android |

## Next (in order)

| | Item | Plan | Notes |
|---|---|---|---|
| ⏳ | **Prod on Linux** | `docs/plan-deploy.md`, `plan-linux-service.md` §5.1 | Same as dev: a Linux `margin-prod` guest |
| ⏳ | **L1 — accounts and principals**: people, groups, agents owned by a person, invitations by GitHub login, per-principal events | `docs/plan-live-context.md` | Fixes events going to every device |
| ⏳ | **R2 — rerank and query rewriting**, eval-gated | `docs/agent-rag-architecture.md` §4 | |
| ⏳ | **L2 — access control inside search**: hierarchical groups, grants/denials, agents limited by their owner, private knowledge and publishing | `docs/plan-live-context.md` | |
| ⏳ | **Phase 5 — hardening** | `docs/agent-architecture.md` §2.10 | iOS background continuation for folders |
| ⏳ | **Q — serious quizzes**: graded answers, code editor, review-only code grading | `docs/phase-q-brief.md` | After L2 (needs per-person attempts) |
| ⏳ | **L3 — inbox and @mentions** for people and agents | `docs/plan-live-context.md` | |
| ⏳ | **L4 — meetings** on LiveKit, per-speaker transcripts into knowledge | `docs/plan-live-context.md` | Zed's collab as inspiration only (GPL) |
| ⏳ | **L5 — voice-first** with ElevenLabs | `docs/plan-live-context.md` | |
| ⏳ | **L6 — permission sync from GitHub + repository source** (code comes in this way) | `docs/plan-live-context.md` | |

## Later / ideas

- 🗓 Office and e-book formats (`.docx .pptx .epub`) once an extractor exists.
- 🗓 Audio and video in folders.
- 🗓 A cross-encoder reranker if the eval earns it.
- 🗓 Agents over MCP for other companies' tools (after L1's agent tokens).

## Waiting on the owner

- 👤 `margin-eval record-embeddings` + baseline, then un-ignore the CI eval gate (`crates/evals/tests/gates.rs`).
- 👤 Hands-on runs: phase 4's manual D13 checks (macOS scanned PDF, iOS iCloud folder, Android), the providers screens, a real course from a real PDF.
- 👤 First real-provider runs (OpenAI, ChatGPT, Claude); ChatGPT's per-app cap doesn't list Margin yet.
- 👤 Licence reads before selling: EmbeddingGemma terms; pocket-tts voices (cosette and jean are blocked as non-commercial).
