# Dogfood: Margin as the knowledge base for building Margin

Written 2026-10-14 on `claude/agent-build` (HEAD `b7c73d7`). Read: `docs/agent-rag-architecture.md` §2.11–2.12, §3, §6; `docs/plan-live-context.md` §7; `docs/HANDOFF.md`; `crates/shared/src/{context,routes}.rs`; `crates/service/src/{auth,context_api,ingest}.rs`. Owner: "plan so that we lay out the base quickly to start using the Margin app to build up the knowledge base for this project and use that to develop itself."

The slice is one small area, **D (dogfood)**, plus two habits. It uses what R1 already defines and adds no service code, no new wire types and no auth model: a single-user adapter over the four context routes, behind the owner's own device token. Nothing here waits for L1; R2's MCP server (per-agent tokens) later replaces D's transport and keeps its tools.

## 1. What goes in

| Knowledge | How it gets in | When |
|---|---|---|
| `docs/*.md`, `PLAN.md`, the briefs, `docs/agent-contract.md` | `margin-mcp ingest-dir docs PLAN.md` (D), one document per file, name `docs/<file>@<short sha>`, origin `agent:repo`; re-run replaces by name (same `ingest` diff-by-hash path R1-store builds) | from day one, by hand; then a `post-commit` hook on `claude/agent-build` |
| Design decisions | `margin-mcp ingest --name "Decision: <topic>" --kind decision -` from the session that records them (also written to agent memory, §3) | each time one is made |
| The owner's chats with Margin's agent | R1-store indexes `Origin::Library { kind: Chat }` on `Write` of a chat. **Check at integration**: if R1-store skips library origins, D adds `margin-mcp ingest-chats` over `GET /api/chats` until it does | automatic |
| Claude Code session summaries | **automatic** (owner decision, 2026-10-14): a `SessionEnd` hook in `.claude/settings.json` runs `scripts/claude-session-summary.sh`, which builds a ≤ 60-line summary from facts (the session's commits, what is changed and not committed, where the session ended) and calls `margin-mcp ingest --kind handoff` (actor `agent:claude-code`); with no `margin-mcp`, no sign-in or no service it fails quietly. The `/margin-handoff` skill stays for a fuller hand-written handoff and the session's decisions | end of every session, automatically |
| Source code | **not** through D. Code waits for the repository source (L6 / "after phase 5"). Until then Claude Code reads code from the checkout as it does today; the KB holds *why*, not *what* | — |
| Non-repo material (PDFs, screenshots, mockups) | phase 4: attach the folder from the desktop app once 4a merges | after phase 4 |

Why a CLI for `docs/` instead of attaching the folder: the CLI is git-aware (the commit hash is in the name, a hook keeps it current, a worktree is not a folder on the owner's Mac) and works the day R1-store merges, before 4a. Folders are for material that lives outside git.

## 2. How Claude Code reads and writes

**`margin-mcp serve`** (stdio MCP, in `crates/mcp`, Rust, uses `margin_shared` types; the `rmcp` crate or a hand-written JSON-RPC loop, whichever is smaller) exposes:

| Tool | Route | Notes |
|---|---|---|
| `search_context { query, k?, scope? }` | `POST /api/context/search` | default scope `Library`, `k = 8`; returns hits with `document name, section_path, lines, citation, text` |
| `document_outline { document_id }` | `GET /api/context/documents/{id}/outline` | |
| `read_passages { document_id, from, to }` | `GET …/passages?from&to` | |
| `ingest_document { name, text, kind? }` | `POST /api/context/ingest` | origin `Agent { name: "claude-code" }`; the service stamps `actor: agent:claude-code` |
| `recent_changes { since }` | `GET /api/context/changes?since` | "what changed in the KB since my last session" |

Auth: `margin-mcp login` runs the existing device sign-in (`POST /auth/start` with `device_name: "Claude Code (<host>)"`, opens the GitHub URL, polls `/auth/poll`) and stores the minted token in the macOS Keychain (`keyring` crate, service `margin-mcp`). It shows in Settings → Devices and is revoked there like any device. No token in a file, none in the repo. Server URL from `~/.config/margin/mcp.toml` (`url = "https://margin.krishnanelloore.com"`); tests pass an explicit URL to a fake.

Since L1 (`docs/l1-brief.md` D11) the token to use is an **agent token**: create an agent "claude-code" (kind External) in Settings → Organisation → Agents with the ceiling you want (by default your groups, not your private notes and chats), mint its token, and run `margin-mcp token <mga_…> [--url <service>]` (or `margin-mcp token -` to read it from stdin, out of the shell history). The token is checked with `GET /api/me`, kept in the secret store under `<url>#agent` and never printed back; it is used before a device sign-in when both exist, and `margin-mcp token --forget` removes it. `margin-mcp whoami` prints who margin-mcp signs in as ("agent claude-code of @krishna, may read: company, company/engineering"). `margin-mcp login` (a device token, your whole access) stays as the fallback. `serve`, `ingest`, `ingest-dir`, `purge` and the `SessionEnd` hook call only the routes an agent token may call, so nothing else changes.

Wiring: `.mcp.json` at the repo root (`{"mcpServers":{"margin":{"command":"margin-mcp","args":["serve"]}}}`) and three lines in `CLAUDE.md`: search Margin before reading docs for context; record decisions with `ingest_document`; the session summary is saved automatically at the end (`/margin-handoff` adds a fuller one). `cargo install --path crates/mcp` puts the binary on PATH.

Write-back is bounded: the end-of-session summary hook, the handoff skill and deliberate `ingest_document` calls write; tool results and transcripts are not ingested (the hook keeps only the session's last answer, not the transcript) (noise, and prompts in transcripts are untrusted text).

## 3. Relation to the SurrealDB agent memory (Spectron, scope `project/margin`)

Different shapes: agent memory holds **facts as triples** (status, build order, open questions) shared across every Claude session the owner runs; Margin holds **documents with passages and citations**. Recommend **keep both, mirror decisions, measure, then retire**:

1. Now: decisions go to both (triples to memory as today; a short decision document to Margin). Documents, briefs and summaries go to Margin only.
2. When D has been in use for two weeks, the `dogfood` eval suite (§6) asks the decision questions of both; if Margin answers them as well, memory stops receiving decisions and keeps only session-start status (three keys).
3. Margin gets a first-class "fact" primitive only if the eval says documents are not enough. Do not build one speculatively.

Replacing memory on day one would lose cross-project reach (memory is not Margin-scoped) and make the KB a dependency of the thing building it before it is proven.

## 4. Prerequisites and the D area

Prerequisites, all in the current queue:
- **R1 first commit** (in: `92b3d7d`): `context.rs`, `routes.rs`, `ContextIngest`, `Origin::Agent`, `ContextChanges`. D compiles against these today.
- **R1-store** (`ingest.rs`, migration, `project` bootstrap) and **R1-retrieve** (`context_api.rs` handlers, `retrieval.rs`): the routes return real answers. **R1-trace** for §6. R1-embed for chunking `.md` by headings (`sections-v2`).
- **Not** phase 4 (only for §1's last row), not providers, not L1.

The D area (worktree `agent/d-mcp`, files exclusive):
- `crates/mcp/` (new, `publish = false`): `Cargo.toml`, `src/{main,serve,login,ingest,client}.rs`, `tests/` on a fake axum service (the fake implements the four routes and `/auth/*` in memory, like the fake Responses server).
- `.mcp.json`, `.claude/skills/margin-handoff/SKILL.md`, `.claude/settings.json` + `scripts/claude-session-summary.sh` (the automatic summary), `scripts/ingest-docs.sh` (the post-commit hook body; the hook itself is installed by hand, never committed into `.git`).
- `CLAUDE.md`: three added lines. `docs/HANDOFF.md`: a D row. `Cargo.toml` (workspace): one member line, the only shared file, added in the first commit before branching.
- "Project: Margin" bootstrap: none. R1 creates exactly one project ("My library") and the context API resolves inside it; renaming or a second project is multi-project work R1 explicitly defers. The owner's whole library *is* the Margin project until then.

No file collides with R1 (`crates/{embed,service,ai,evals}`) or phase 4 (`crates/app`). D is about 600 lines of Rust and can be built in parallel with R1's areas today, against the fake; it goes live the moment R1-store and R1-retrieve merge and the owner installs the service.

## 5. Safety: the live instance is real data

- **Backup before the first R1 install**: stop the service, copy `data_dir` (`docs/service-setup.md` §3; today `~/Library/Application Support/Margin/data`) to `…/Margin/backups/<date>/`, start. R1-store's one-pass `passages-v1` migration runs at start; a backup is the only undo. Add `scripts/backup-margin.sh` (D) that does exactly this and refuses to run while the service is up. (Now `deploy/scripts/backup-margin.sh`, the one backup script, also used by every deploy: `docs/plan-deploy.md` §5.)
- **Agent writes are bounded**: everything D writes has `actor: agent:claude-code|repo` and `Origin::Agent`, so `margin-mcp purge --actor agent:repo` can delete it all (via the ordinary delete route) without touching the owner's own documents. The ingest CLI has `--dry-run` and prints what it would replace.
- **Tests never see the live service**: the client requires an explicit URL; the test fake is in-process; CI has no Keychain and `login` is not exercised there. `margin-eval` keeps its own `data_dir/evals/` (RAG §3.4).
- **Size**: `docs/` is ~0.3 MB of Markdown; a hook run is a handful of documents; embedding on the Mac is seconds. No rate concern.
- **Content**: briefs contain no secrets; the hook ingests only tracked `*.md`. Chats are the owner's own (single user), and L2 will make `Origin::Agent` documents private-to-ingester by default when principals exist.

## 6. Feeding the eval suite

Every `search_context` call is a `retrieval` trace row with `via: "mcp"`, `actor: agent:claude-code` and the query itself (margin-mcp names its agent in the `x-margin-agent` header; `route` stays the model slot, and only agent rows keep the query, at most 500 characters); every `read_passages` after it is a `tool_call` row (`tool: read_passages`, the document and the range read), an implicit relevance mark on that document. `margin-eval harvest --service-url <dev> [--since <time>] [--out FILE]` (`crates/evals/src/harvest.rs`; the same `--service-url` checks as `run` and `export`, so never prod) pairs each search with the reads by the same agent within 30 minutes after it and appends new queries to `evals/golden/candidates.jsonl` as `{ query, relevant: [{document, anchor: <first line read>}], kind: "semantic", source: "dogfood", actor, asked_at }`; the owner (or `grade-questions`) promotes the serious ones into `retrieval.jsonl`. The handoff skill's open questions become `courses.jsonl`-style "must-cite" items for the docs. A third suite, `run --suite dogfood`, runs the retrieval suite with `evals/golden/dogfood.jsonl` (the promoted candidates, `RetrievalQuery` rows) over `docs/` itself as the corpus (the repo is private; the corpus stays out of CI's 5 MB fixture set and runs only on the opt-in runner). This is the first eval set made of questions a real agent asked while doing real work, which is what RAG §3.1 wanted and could not synthesise.

## 7. Where it slots in, and owner questions

Order: **R1 + phase 4 (building) ∥ D (now, on the fake)** → owner installs R1 and runs `margin-mcp login`, `ingest-dir`, backup first → providers → L1 → R2 (moves D's transport onto agent tokens; the tools and the skill stay) → … as `plan-live-context.md` §7. D's integration is one e2e: login on the fake → ingest a doc → search finds it with a citation → changes lists it.

1. **Build D now in parallel, or after R1 merges?** Recommend now: it only needs the first commit's types, and the owner gets it the same day R1 lands.
2. **Keep the Spectron agent memory?** Recommend keep both and mirror decisions for two weeks, then let the dogfood eval decide (§3).
3. **One actor `agent:claude-code` for every session, or per session (`agent:claude-code/<session>`)?** Recommend one actor; the session id goes in the summary's name.
4. **`docs/` via the git-aware CLI (recommended) or by attaching the folder once 4a ships?** Recommend CLI for the repo, folders for everything else; never both for the same files.
5. **Token in the macOS Keychain (recommended) or a 0600 file for Linux CI boxes?** Keychain; there is no CI use.
6. **Commit `.mcp.json` and the skill to the repo?** Recommend yes; they carry no secrets and every clone gets the setup.
7. **Should the handoff summary be mandatory?** Decided (owner, 2026-10-14): yes, automatic: a `SessionEnd` hook (once per session, not `Stop`, which fires after every answer) saves the summary and never blocks or fails the session; a session with no commits, changes or answer writes nothing.
8. **Should chats with Margin's agent be in the KB by default?** Recommend yes (single user today; L2 scopes them when principals exist).
