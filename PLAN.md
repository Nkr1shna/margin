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
| Layout | Mockup **Option D · Notebook + Ask** (`../mockups/study-app/index.html`, `data-option="d"`), styled with openai.com's Oct 2026 tokens (the `.win` / `.app-scope` CSS block in that file). Sidebar becomes a drawer on narrow screens. |
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
