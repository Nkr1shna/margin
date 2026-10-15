# Later

The roadmap lives in the [Linear project](https://linear.app/krishnanellooremargin/project/margin-fc714ebf8f0b). This page keeps only what has no Linear issue yet. Feature work is paused; move an item into Linear when it starts and delete it here.

## Feature slices (paused)

Detail in the linked briefs. Each slice is vertical: usable end to end on desktop, iPhone and Android.

- **S1, find and manage your knowledge** (`docs/basics-brief.md`): in-file and in-chat search with highlights, a Files screen, trash with undo, move and reorder, copy buttons, toasts and help.
- **S2, bring knowledge in**: wire the Office readers, web page as a source (SSRF guard), share sheet on Android, iOS and macOS, repository source and GitHub permission sync (L6).
- **S3, run it for a company**: usage per person, offboarding, prod hardening.
- **S4, serious quizzes** (`docs/phase-q-brief.md`): graded answers, code editor. Re-check the brief against L1 and L2 first.
- **S5, inbox and @mentions** (`docs/plan-live-context.md`).
- **S6, meetings**: LiveKit with per-speaker transcripts, after the `livekit` build spike.
- **S7, voice-first**: ElevenLabs, after the webview spike and ENG-18.

## Ideas

- E-book formats (`.epub`) once an extractor exists; audio and video in folders.
- R2 variants as defaults (listwise rerank, rewriting, the cross-encoder) once the eval earns it (`docs/r2-brief.md` section 6).
- Agents over MCP for other companies' tools.
- A real iOS share extension; LibreOffice headless for legacy `.doc .ppt .xls`; spend caps on the usage route; a GitHub-issue feedback webhook.
- Splitting the one service write lock once the `WriteGuard` parameter makes the rule visible.
- Independent chores: property tests, `nextest` timeouts, scheduled full-platform CI, `shellcheck` on `deploy/**`, splitting files over 2,000 lines as touched, `openat` folder reads, HTTPS for non-local service URLs, a `// SAFETY:` audit.

## Waiting on the owner (not in Linear)

- R2 recordings: `margin-eval record-rewrites`, `record-embeddings`, `record-reranks`, commit them, then the A/B runs in `docs/r2-brief.md` section 6; on the dev guest, download the reranker and check `rerank_ms` in `GET /api/traces`.
- Decide whether to keep offering `bge-reranker-base` (1266 MB RSS, over the 400 MB budget) as an opt-in variant.
- First real-provider runs (OpenAI, ChatGPT, Claude); the ChatGPT per-app cap does not list Margin yet.
- Phase 5 manual checks (`docs/phase5-brief.md` section 5) and the compaction-rejection fixtures from OpenAI and Claude.
- Licence reads before selling: EmbeddingGemma terms; pocket-tts voices (cosette and jean are non-commercial).
