# Margin — plan and decisions

Margin is a personal study app: ask for a topic, get a course outline, generate lessons,
take notes beside them, and turn lessons into narrated explainer videos. One service on
the owner's Mac holds the data and does all generation; desktop, iOS and Android apps are
thin, reactive clients.

Every decision below was made explicitly by the owner. Do not change one without asking.

## Decisions

| Area | Decision |
| --- | --- |
| UI framework | **Dioxus 0.7.9** (webview renderer) for desktop, iOS and Android from one codebase. |
| Layout | Mockup **Option D · Notebook + Ask** (`mockups/study-app/index.html`, `data-option="d"`), styled with openai.com's Oct 2026 tokens (the `.win` / `.app-scope` CSS block in that file). Sidebar becomes a drawer on narrow screens. |
| Scope | Sidebar topic tree, Ask (outline → lessons), Lesson with Read / Notes / Video views, **margin notes from text selection**, Notes, Video, Settings, **⌘K search**. **No** document tabs, **no** separate Videos page. |
| UI rule | Every UI element is fully functional. No mock or placeholder controls. If something can't be built, leave it out and ask. |
| Data | A **service** on the owner's Mac with **SurrealDB embedded** (`kv-surrealkv` on disk; `kv-mem` in tests). Apps talk only to the service and receive live updates over a WebSocket. |
| Exposure | **Cloudflare Tunnel** to `margin.krishnanelloore.com`. No open ports. |
| Accounts | Single user. Only GitHub login **`Nkr1shna`** is allowed. Each device gets its own revocable token (stored hashed). |
| Sign-in | GitHub OAuth **browser redirect** to the service; the app **waits on the service** (start → open browser → poll). No custom URL schemes. |
| AI | OpenAI **Responses API only**, configurable base URL, model and key. |
| Secrets | **OS keychain**. The OpenAI key is entered in the app, sent to the service, stored in the Mac's Keychain, never returned. Devices keep their own token in their OS keychain (`keyring` crate). |
| Video | **ManimGX** (`manimgx` CLI, Manim CE API) renders; narration via `self.say(...)`, voiced by **PocketTTS through the FluidAudio CLI** (`fluidaudiocli tts … --backend pocket`). |
| Tool install | The app shows whether `manimgx` and `fluidaudiocli` are found on the service Mac, with **install commands to copy** — it never runs installers. |
| Scene code | AI-written scene code renders **automatically, sandboxed** (macOS `sandbox-exec`: no network, writes only in the video's folder). Elsewhere rendering is refused unless the service runs with `--allow-unsandboxed-render` (tests only). |
| Playback | In-app `<video>` element (the Dioxus webview), streaming from the service's signed media URLs. |
| Agent | Ask becomes a **chat with an agent**. The agent loop runs **on the service** (new `crates/agent`, custom Rust loop on the Responses API, `store: false` with full replay). Device-only tools run on a device over the events socket: **folder tools** on the device holding the folder, **app-control tools** on the device that sent the message. Full design: `docs/agent-architecture.md`. |
| Chats | Stored on the service and synced live; one running turn per chat (a message sent meanwhile is queued); any device can stop/continue/confirm; "Chats" section in the sidebar; inline cards (editable outline card with chips, result cards). |
| Agent permissions | Acts freely; **confirms deletes** and **overwrites of the owner's own writing**; no web search for now (an alternative provider will be integrated later); attached folders **read-only**, no shell commands. |
| Folders (workspaces) | "Attach folder" on every platform (macOS/Linux path + bookmark, iOS/iPadOS security-scoped bookmark, Android persisted SAF tree URI); the handle stays on that device; **no folder upload**. Excerpts load on demand on other devices. Lessons can **cite folder files**, opened from the holding device. macOS stays directly distributed (unsandboxed). |
| Sources | Files and folders added **in the Ask composer**, uploaded from the app to the service and **kept with the resulting topic** (the topic menu's "Sources…" adds/removes later). Types: **text, code, PDF (text and page images), images, audio, video** (frames + soundtrack). Used through **retrieval**: chunks embedded locally, the most relevant passages go into outline and lesson prompts. |
| Embeddings | **EmbeddingGemma 2, Core ML** (`FluidInference/embeddinggemma-2-coreml`) run **inside the service** from Rust (`objc2-core-ml`, `tokenizers`); no helper process. **Never bundled**: downloaded from Hugging Face by a button in Settings. Text, image and audio encoders, all downloaded by one button (~1.4 GB). |
| PDF text | Apple **PDFKit** (`objc2-pdf-kit`); pages without a text layer are rendered and read with **Vision OCR** (`objc2-vision`). |
| Media in lessons | Retrieved images (image files, PDF pages, video frames) go to the writer as **image inputs**; audio is **transcribed locally** (FluidAudio `transcribe`, Parakeet) and its transcript span goes in as text. |
| Citations | Lessons cite with Markdown footnotes `[^n]`; no inline images. A **Sources** list follows the lesson and a **source viewer** opens the exact page / time / image / passage. |
| Upload limits | **500 MB** per audio or video file, **50 MB** for every other kind (`routes::max_source_bytes`). Uploads stream to disk; files over 32 MB go up in parts (Cloudflare refuses bodies over 100 MB). One add (files or a folder) takes at most 300 files and 2 GB. |
| Transcripts | The source viewer shows an audio/video source's **whole transcript**, the cited span marked and scrolled into view; any line seeks. A recording that couldn't be transcribed is **Ready with a warning** and offers **Read again**. |
| Note drafts | Unconfirmed note edits are kept in `drafts.json` in the app's data folder and sent again on the next launch or when the note reopens; cleared once saved. |
| Touch reveal | On touch, a long press on cut-off text shows it in full above the finger; rows whose long press opens a menu head it with the full name. |
| Mobile pickers | Adding files **and folders** works on iOS/iPadOS (UIDocumentPicker) and Android (Storage Access Framework), not only desktop. |

## Layout

```
margin/
├── crates/shared   # wire contract: records, requests, events, routes (DONE — the contract)
├── crates/ai       # Responses API client + prompts (outline, lesson, ManimGX scene)
├── crates/video    # ManimGX + FluidAudio pipeline, sandbox, tool detection
├── crates/service  # axum server, SurrealDB, auth, jobs, WebSocket, media
└── crates/app      # Dioxus app (desktop / iOS / Android)
```

`crates/shared` is the contract. `crates/ai/src/lib.rs` and `crates/video/src/lib.rs`
contain the public signatures the service codes against; implementations must keep those
signatures (adding items is fine; changing existing ones requires updating every caller).

## Service

- Binary `margin-service`. Subcommands:
  - `serve [--allow-unsandboxed-render]` — run the server.
  - `setup` — interactive: writes the config file and stores the GitHub client secret in
    the Keychain.
- Config file: `~/Library/Application Support/Margin/service.toml` (path overridable with
  `MARGIN_CONFIG`): `public_url` (default `https://margin.krishnanelloore.com`), `bind`
  (default `127.0.0.1:8787`; cloudflared points here), `data_dir`, `github_client_id`,
  `allowed_github_login` (default `Nkr1shna`), `github_api_url` / `github_oauth_url`
  (default github.com; overridden in tests by a fake GitHub).
- Keychain items (service name `com.krishnanelloore.margin`): `github-client-secret`,
  `openai-api-key`, `media-signing-key` (generated on first run). Behind a `SecretStore`
  trait with an in-memory implementation for tests.
- Data: SurrealDB embedded under `data_dir/db`; videos under `data_dir/videos/{id}/`.
- Auth: `/auth/*` per `margin_shared` docs; device tokens are 32 random bytes, base64url,
  stored as SHA-256. Every `/api` request updates `last_seen_at` (at most once a minute).
- Jobs: generation runs in background tasks with progress published as `Event`s to every
  connected socket. Lesson generation streams `LessonDelta`. Video pipeline:
  `Scripting` (ai::write_scene) → render (video::render). On `RenderError::Scene`, retry
  once with `FailedScene` (write_scene repairs) before failing. One render at a time
  (a queue); lessons may generate concurrently (max 3).
- Media URLs: `/media/videos/{id}/{file}?exp=<unix>&sig=<hex HMAC-SHA256(key, "{id}/{file}/{exp}")>`,
  valid 6 hours, range requests supported.
- Startup recovery: jobs left `Generating`/`Scripting`/`Voicing`/`Rendering` by a crash
  are marked `Failed { error: "Interrupted when the service stopped." }`.

## Sources and embeddings

- Crate `crates/embed` (macOS): `Embedder` (download from Hugging Face with progress,
  compile the `.mlpackage` once, load with all compute units, `embed(texts, Purpose)`),
  `extract` (text/code decode, PDFKit + Vision OCR), `chunk` (≤ 512 tokens with the Gemma
  tokenizer, prose by paragraphs, code by lines, PDFs per page then split).
- Preprocessing per the model card: prefix `title: none | text: ` (documents) or
  `task: search result | query: ` (queries); tokenize with `tokenizer.json`, `<bos>`=2 …
  `<eos>`=1; look up rows of `embeddings.bf16` (262144 × 512 bf16) × √512 → fp16; run the
  smallest `embed_S` (32, 48, 64, 128, 256, 512) with `inputs_embeds` [1,S,512] and
  `attention_mask` [1,S]; output `embedding` [1,768], L2-normalized.
- Service: `POST /api/sources` stores the file under `data_dir/sources/{id}`, then a
  background queue extracts, chunks and embeds (one source at a time; `Queued` while the
  model isn't downloaded). Chunks (text, page, vector) live in SurrealDB. Retrieval for a
  topic: cosine similarity over its chunks plus a boost for exact query words; the top 8
  passages (with source name and page) go into the outline and each lesson prompt.
- Model files: `data_dir/models/embeddinggemma-2/`. `GET/POST/DELETE /api/embedding`.

## App

- Dioxus 0.7.9, `dx serve --platform desktop` (also `ios`, `android`).
- First run: ask for the service URL (default `https://margin.krishnanelloore.com`),
  then "Sign in with GitHub" (start → open browser → poll). Token in the OS keychain.
- State: a global store loaded from `GET /api/library`, kept current by the `/api/events`
  socket (reconnects with backoff, reloads the library after reconnect).
- Screens (Option D): sidebar (Ask, Search ⌘K, All notes, topic tree with video status
  dots, Settings); Ask (composer, chips, outline proposal with inline edit, Create
  lessons); Lesson (Read with margin notes from selection, Notes for this lesson, Video:
  player, storyboard, scene code, status/log, Make video / Re-render); Notes (list +
  editor); Settings (service, account & devices, AI provider + key, voice, render size,
  tools status with copyable install commands, appearance).
- Theme: CSS variables copied from the mockup's app tokens, light and dark following the
  system, Albert Sans (bundled, OFL) standing in for OpenAI Sans.

## Testing

- Here (macOS): unit + integration tests per crate. The service's tests run against an
  in-memory SurrealDB, an in-memory secret store, a fake GitHub and a fake Responses
  API (local axum servers). Video tests render real ManimGX scenes with `Voice::Silent`;
  PocketTTS tests run only when `fluidaudiocli` is installed (skipped otherwise, with a
  message).
- Manual: real GitHub OAuth app, real OpenAI key, Cloudflare Tunnel, iOS/Android builds.
