## Phase-3 check and the final phase-4 brief (folders)

Read: HEAD `07a603d`; `crates/shared/src/{chat,device,lib,routes}.rs`; `crates/agent/src/{device,prompt,tool,turn,replay}.rs`; `crates/ai/src/lib.rs` (`InputPart`, `Image`, `Passage`); `crates/embed/src/{lib,pdf,image,extract,chunk,rank}.rs`; `crates/service/src/{agent_tools,ask,chats,db,devices,engines,library,media,sources,uploads,state,turns,workspaces,server}.rs` and `tests/{chats,agent_e2e}.rs`, `tests/common/mod.rs`; `crates/app/src/{main,state,store,api,sources,fake,citations}.rs`, `tools/{mod,app_control}.rs`, `picker/{mod,desktop,ios,android}.rs`, `android/MarginPicker.kt`, `ui/{chat,sources,settings,citations,shell}.rs`, `Cargo.toml`, `build.rs`; `docs/agent-prebuild-review.md` §D phase 4. No files edited.

### Decided by the owner (2026-10-14)

- **Q1 — regenerating a lesson that cites folder files** retrieves again from the stored index (the folder's chunks on the service), exactly as lessons do with sources; nothing is read from a device. (Asked when reads were on demand; with ingestion "stored" means the index. See the note in §4.)
- **Q2 — an allow-list of file types.** Owner: "code should be added by repository, instead of uploading. the folders with images, pdfs, other documents are only allowed. we can take a fixed set of sources and expand as we go whenever needed. even if a folder with other types are stored, we only take from the fixed set." Any folder can be attached; only files whose type is in one fixed table count (prose documents, PDFs, images; D0); everything else is invisible; folders are walked whatever their name.
- **Q3 — no notice on the holding device** about reads from other devices. (With ingestion there are no reads from other devices at all.)
- **Audio and video stay off the allow-list.** **Code comes in from a GitHub repository, as its own phase after phase 5** (not designed here).
- **Folder files are ingested, not read on demand** (owner, on the second draft's open items): "yes in the surreal db, we need to save it into the db as they upload, and pass it through embedding gemma and use the vector db in surrealdb for that"; "no limit, the agent will not read the entire thing, we will use it to embed and then the agent RAG to get relevant information"; "we need to search inside pdfs". So: when a folder is attached (and as its files change) the holding device uploads its allow-listed files to the service, which stores them as sources and runs the Sources pipeline it already has (PDFKit/OCR text, page renders, EmbeddingGemma 2 text and image embeddings) into SurrealDB; the agent's main folder tool is semantic search over the folder's chunks, PDFs and images included; reading a span, a page or a picture comes from what is stored. No per-file size cap and no picture caps beyond what retrieval returns. The allow-list (D0) is kept.

**What the code does today, checked.** Sources already go through exactly this pipeline: `sources::upload`/`uploads::finish` → a `Source` row → the serial `sources::run_worker` → `SourceTools::extract` (PDFKit text, Vision OCR for pages without a text layer, each page rendered to `page-{n}.png`, ImageIO for images) → `chunk` (≤ 480 tokens) → EmbeddingGemma 2 text vectors into `db::CHUNK` rows and image vectors into `db::MEDIA` rows (one per picture or page) → `sources::retrieve` picks `PASSAGES` (8) with ≤ `MAX_IMAGES` (4) pictures by `rank::score` (cosine plus a term boost) → `Passage` with the picture for the lesson writer → `Citation { page, image_url }` → the viewer. The store is **embedded SurrealDB 3.3.4 (SurrealKv on disk, Mem in tests), schemaless tables, each record `{ doc: <json> }`** (`db.rs`). **The vectors are in SurrealDB already, but there is no vector index: `retrieve` loads every chunk and media row of the sources in question and ranks them in Rust.** Phase 4 routes folder files into that same path and changes none of it; retrieval, chunking, ranking, the search-tool contract and citations are being re-architected separately in `docs/agent-rag-architecture.md` (owner, same day), so the sections below that touch them (D5a, D7b, D7c) are written against today's pipeline and marked **to be replaced by that document**.

### 1. What phases 1–3 already give phase 4, checked against the code

| Phase-4 plan item | State | Where |
|---|---|---|
| `Workspace`, `NewWorkspace`, `Platform`, `WorkspaceFile`; `GET/POST /api/workspaces`, `DELETE` only by the holder (403 `not_yours`); `delete_locked` detaches from chats; `delete_for_device_locked` on revoke | **Done** (phase 1, 3). Tested in `chats.rs::folders_belong_to_their_device_and_leave_chats_when_removed`. Phase 4 makes `delete_locked` also delete the folder's sources (D7d). | `workspaces.rs`, `chat.rs` 460–500 |
| `Chat.workspace_ids`, `NewChat.workspace_ids`, `ChatPatch.workspace_ids` (replace), `check_workspaces` (400 `unknown_workspace`) | **Done.** | `chats.rs` 213–224, 399–416 |
| Folder device tools `list_files`/`find_files`/`search_text`/`read_file` in the agent crate, routed to the holding device | **Built for on-demand reads; superseded.** With ingestion the service answers from what it stores; the only device call left is `sync_folder` (D5a). 4-agent reshapes them; the `DeviceToolCall` folder variants go. | `device.rs` 303–560 |
| `tool_choice` excludes `FOLDER_TOOLS` when the chat has no workspace | **Done**; the const changes to the four names in D5a and the gate follows. | `turn.rs` 455 |
| Message context lists folders with online/offline | **Done**; gains the sync state (D12). | `prompt.rs` 105–122, `turns.rs` 298–325 |
| Path rules (`relative_components`, `is_skipped_name`, limits) | **Done** and tested, but `is_skipped_name` refuses `build`, `dist`, `target`, … anywhere. Under Q2 that list goes; hidden and secret-looking names stay refused (D0). The `READ_FILE_*`/`SEARCH_*` limits now bound service-side reads of stored files. | `device.rs` 217–311 |
| Device transport: `DeviceHub::call` → `ToolRequest` → app `Runner` (per-request task, deadline floor 5 s, `ToolCancel`, `cancel_all` on disconnect) | **Done** (phase 3). Carries `SyncFolder` in phase 4; folder calls answer `FOLDERS_NOT_YET` today. | `tools/mod.rs`, `main.rs` 566–690 |
| Source upload, in one request or in resumable parts (`GET source_upload(id)` says how much arrived) | **Done**; phase 4 adds `workspace_id`/`modified` to the upload query and lifts the per-kind size cap for folder files (D7a). | `sources.rs` 141–335, `uploads.rs` |
| Source ingestion, chunking, embedding, retrieval, citations, media routes, viewer | **Done** (above). Phase 4 adds only workspace scoping (which source ids a retrieval covers) and the tools in D5a; the pipeline itself is the RAG review's. | `sources.rs`, `embed/*`, `media.rs`, `citations.rs` |
| Unattached sources purged after 24 h; `Library.sources` lists every source; `search_sources`/`list_sources` default to every source | **As built**; folder files must be exempt from the purge and out of those defaults (D7d). | `server.rs` 307, `library.rs` 146, `agent_tools.rs` 635–810 |
| `Citation.file: Option<WorkspaceFile>` | **Not there.** First commit (display only now: which folder, which path). | `chat.rs` 574–585 |
| App keeps `LibraryState.workspaces` current from events | **Half**: nothing calls `GET /api/workspaces`. 4-core loads it in `load_library`. | `store.rs` 39, `main.rs` 460 |
| Folder pick on each platform | Pickers pick folders only to **upload their files** through the topic Sources dialog ("Add folder…"), against the owner's "no folder upload" decision. The first commit removes that path; 4a/4b/4c build `pick_folder` (a persistent handle), and the **walk-and-upload now lives in the sync engine** (D6), which uploads to the folder, not to a topic. | `picker/*`, `sources.rs` (app) 241–349, `ui/sources.rs` 755–905 |
| Citation viewer | `citations::viewer` dispatches on `SourceKind` and already shows a PDF page picture, an image, or a text excerpt; folder citations are ordinary citations of folder sources. 4-core adds the "in `folder` on `device`" line and the gone/changed states. | `citations.rs` 169–211 |
| Fake service | Has the workspace routes; no sync route, no folder sources. 4-core. | `fake.rs` 4649–4706 |
| Test infrastructure | `common::Socket` drives a device over the socket; the real client uploads sources (single and in parts); `FakeSourceTools` fakes extraction and embedding; `FakeGenerator.lesson_body` lets a lesson cite `[^1]`. Enough. | `tests/common/mod.rs` |

**Phase-3 open questions and phase 4.** None blocks it. *iOS closes the socket in the background*: a sync runs only while Margin is in front on an iPhone/iPad (D11); phase 5's `BGContinuedProcessingTask` keeps it going. "Android never compiled" matters for 4c.

### 2. Refinements to the pre-build plan

- **Folders are sources.** A folder file is a `Source` row with `workspace_id` set, in its own place (`source_dir(id)` as today), never in a topic. Everything built for sources applies; what phase 4 adds is the sync, the scoping (folder sources stay out of topic and chat defaults), the index and the tools.
- **R4.1 (4-core before 4a/b/c)**: the first commit lands the `Dir` trait and `StdDir`; 4-core builds the sync engine and the UI against the trait; 4a/4b/4c build `open`/`forget`/`pick_folder`. All branch from the first commit; 4a/b/c merge after 4-core.
- **R4.2 (spans read at proposal time) is withdrawn.** Lessons retrieve from the folder's index when they are written, as they do from sources; a proposal records which folders it draws on (D7c). Nothing is read from a device for a course.
- **No citation proxy route, no on-demand device reads, no service-side cache**: opening a folder citation shows what the service stores (the page render, the image, the chunk) — it works with the device off.
- **A 4-agent area exists** (small): the folder tools as service tools over a `FolderIndex` trait and `sync_folder` (D5a).
- **R4.6 (macOS TCC) is handled at attach time**: `pick_folder` on macOS does one `read_dir` of the root right after the pick (the first sync would otherwise prompt from a blocking thread).

### 3. Phase 4 — final build brief

#### D0 settled: the allow-list and the name rules (first commit, `margin_shared::device`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FolderFileType { Text, Pdf, Image }

/// The only files a folder sync takes. One table, extended as the owner allows more.
pub const FOLDER_FILE_EXTENSIONS: &[(&str, FolderFileType)] = &[
    ("md", Text), ("markdown", Text), ("txt", Text), ("text", Text), ("rst", Text),
    ("adoc", Text), ("org", Text), ("tex", Text), ("bib", Text), ("html", Text), ("htm", Text),
    ("pdf", Pdf),
    ("png", Image), ("jpg", Image), ("jpeg", Image), ("heic", Image), ("heif", Image),
    ("webp", Image), ("gif", Image), ("tif", Image), ("tiff", Image), ("bmp", Image),
];
/// Extension-less names that are text.
pub const FOLDER_FILE_NAMES: &[&str] = &["readme", "license"];
/// The type of a file the sync takes, by name (case-insensitive); `None` is invisible.
pub fn folder_file_type(name: &str) -> Option<FolderFileType>;
/// "text documents (.md, .txt, .rst, .tex, .html…), PDFs and images".
pub const FOLDER_FILE_TYPES_TEXT: &str;
impl FolderFileType { pub fn source_kind(self) -> SourceKind /* Text, Pdf, Image */ }
```

Confirmed set (owner, item 1): `SourceKind::Text` minus data/config (`csv tsv json yaml yml toml xml ini srt vtt`) and the build-tool names; every `SourceKind::Image` extension; `pdf`. No code, audio or video. `is_skipped_name` loses its folder list and keeps the hidden rule (leading `.`) and the secret patterns, so `credentials.txt` stays out. Tests as before (no duplicate extension; the type of `Notes/Ch1.MD`, `main.rs`, `README`, `scan.PDF`, `photo.heic`, `.env`, `build`).

#### First commit (orchestrator lands on `claude/agent-build` before branching)

`crates/shared`:
1. `Source` gains `#[serde(default)] workspace_id: Option<Id>` (a folder file; `folder` then holds the folder's name and `name` the path inside it, as uploads from folders already did) and `#[serde(default)] modified: Option<DateTime<Utc>>` (the file's modification time on the device; with `bytes` it is the file's **stamp**). `Citation.file: Option<WorkspaceFile>` (`#[serde(default, skip_serializing_if)]`), `WorkspaceFile { workspace_id, path }` (the span fields go: a folder citation is a source citation and already carries `page`/`image_url`/`excerpt`). Convention on `file`: set by `sources::retrieved` when the source has a `workspace_id`; `source_name` = "`folder`/`path`". Test: old citations parse; one with `file` round-trips.
2. `Workspace` gains `#[serde(default)] sync: WorkspaceSync { files: u32, bytes: u64, uploading: u32, reading: u32, failed: u32, last_synced_at: Option<DateTime<Utc>> }` (service-maintained counts; `uploading` = files the last manifest listed and the service hasn't received, `reading` = Queued/Processing, `failed` = Failed). `SyncManifest { files: Vec<ManifestFile { path, bytes, modified: Option<DateTime<Utc>> }> }`, `SyncPlan { upload: Vec<String>, kept: u32, removed: u32 }`.
3. `device.rs`: D0; the `DeviceToolCall` folder variants (`ListFiles`, `FindFiles`, `SearchText`, `ReadFile`) and `DeviceToolOutput::{Files, Matches, FileText}`, `FileEntry`, `TextMatch` **move out of the device protocol** into plain shared types (`FileEntry { path, is_dir, bytes, modified, kind: Option<FolderFileType> }`, `TextMatch { path, line: Option<u32>, page: Option<u32>, text }`, used by the service tools' outputs and tests); `DeviceToolCall::SyncFolder { workspace_id }` is added (answer: `Done { message }`); `workspace_id()` follows. Constants beside the limits: `FOLDER_SYNC_INTERVAL_SECS = 900`, `MAX_LISTED_ENTRIES` keeps its value (listings); the `READ_FILE_*`/`SEARCH_*`/`TextMatch` items stay for the RAG review to reuse or drop. Round-trip tests updated.
4. `routes.rs`: `pub fn workspace_sync(id)` = `/api/workspaces/{id}/sync` (`POST SyncManifest → SyncPlan`, 403 `not_yours`, 404); `pub fn workspace_files(id)` = `/api/workspaces/{id}/files` (`GET → Vec<Source>`, newest first, with `media_url`); the `SOURCES` and `SOURCE_UPLOADS` queries gain `workspace_id` and `modified` (doc: with `workspace_id` the file is a folder file of the calling device's folder: `name` is its path, `folder` is ignored and set to the folder's name, `topic_id` must be absent, the kind must be a `folder_file_type`, and the kind's size limit doesn't apply — only `UPLOAD_PART_BYTES` per part); `routes::source_limits_text` unchanged (it describes topic and chat files).

`crates/app` (so every area builds against one trait):
5. `src/folders/mod.rs` with `FolderHandle` (`#[serde(tag = "kind")]`: `Path { path, bookmark_b64 }`, `IosBookmark { bookmark_b64 }`, `AndroidTree { uri }`), `FolderPick { name, location, handle }`, `DirKind`, `DirEntry { name, kind, bytes, modified }`, `DirError { NotFound, NotADir, IsADir, Outside, Revoked(String), Io(String) }`, the `Dir` trait (D1), `open`/`forget` dispatch by `cfg` with the "isn't built on this platform yet" bodies; submodules `store`, `sync` declared empty (4-core fills them).
6. `src/folders/dir.rs`: `StdDir` over `std::fs` with the symlink rule (D1) and its temp-dir tests (list sorted/kinds/sizes/modified, `stat`, `open` streams, symlinked file and folder are `Outside`, missing is `NotFound`).
7. `src/picker/mod.rs`: `pick(folder: bool)` becomes `pick_files() -> Option<Selection>`; new `pick_folder() -> Option<FolderPick>` with the per-platform "isn't built" `None`. The folder-upload path is removed where it reaches the UI: `ui/sources.rs` loses "Add folder…" and `add_sources(.., folder)`; `kinds_text` reads "Add {kinds} files."; `picker/desktop.rs` keeps only `pick_files`; `picker/ios.rs` keeps the files path; `picker/android.rs` keeps `select_documents` and `copy`, drops `DocumentTree`/`select_document_tree`/`children`; `MarginPicker.kt` keeps `treeRoot`/`children` (4c reuses them). In `sources.rs` (app) the `Tree`/`walk` machinery, `Selection.folder`, `Candidate.folder` and `Upload.folder` **stay** (the sync engine reuses `Selection`/`Upload`/`stage` to queue folder uploads); only `select_folder`/`SKIPPED_DIRS` go (the sync walks through `Dir`). `cargo test -p margin-app`, the iOS check and the Android check must pass.
8. `Cargo.toml` (app): `globset = "=0.4.<latest>"`, `base64 = "=0.23.1"`; macOS `objc2-foundation` features add `NSURL`, `NSData`, `NSError`, `NSString`, `NSArray`; iOS adds `NSData`. `Cargo.toml` (service): nothing. No area edits `Cargo.toml` after this.
9. Contract (`docs/agent-contract.md`): §2 phase-4 row; §3 `Source.workspace_id`/`modified`, `Workspace.sync`; §5 "Phase 4" rewritten from D5a/D7; §6 `SyncFolder`; §7 the constants; §8 the tests in D13; storage: `workspace_id` on chunk/media rows.

`crates/service`, `crates/agent`, `crates/embed`: nothing in the first commit.

#### Areas (worktree each; files are exclusive; all branch from the first commit)

| Area | Worktree | Builds | Owns (exclusively) |
|---|---|---|---|
| **4-agent** | `agent/p4-agent` | `FOLDER_TOOLS` = the four names; `list_files`, `find_files`, `search_folder` as `Read` tool types over a `FolderIndex` trait (D5a); `sync_folder` as the one device tool; `device_output_text` for `Done`; prompt sentence; tests against a fake `FolderIndex` | `crates/agent/src/{device,tool,turn,replay,prompt,lib}.rs` and its tests |
| **4-S service** | `agent/p4-service` | Folder files as sources (upload queries, limits, exemptions) (D7a); `workspace_id` on chunk/media rows and the workspace scope of today's retrieval (D7b); the sync route and `Workspace.sync` upkeep (D7a); folders in courses (D7c); scoping and deletion (D7d); `FolderIndex` over the store; `GET workspace_files`; queue priority; tests | `agent_tools.rs`, `ask.rs`, `workspaces.rs`, `folders.rs` (new: the sync route, `FolderIndex`, `Workspace.sync`), `sources.rs` (service), `uploads.rs`, `library.rs`, `db.rs`, `engines.rs`, `state.rs` (service), `server.rs` (route lines + housekeeping), `tests/{chats,courses,sources,agent_e2e}.rs`, `tests/common/mod.rs` |
| **4-core app** | `agent/p4-core` | The handle store and its mirror (D2); the sync engine (D6): scan through `Dir`, manifest, uploads through the existing upload queue, triggers, pause/resume, progress; `Host::folder` running `SyncFolder`; workspaces loaded with the library; Attach folder (D3) and the Folders pane with progress and storage (D10); the strip (D4); `ask_workspaces`; the citation viewer's folder line and states (D8); `Api::{sync_workspace, workspace_files}`, the upload calls with `workspace_id`; `fake.rs` sync/files routes + sample folder sources and citations; render tests | `folders/{mod,store,sync}.rs` (`mod.rs` only its `store`/`sync` bodies), `tools/mod.rs`, `state.rs`, `main.rs`, `store.rs`, `api.rs`, `sources.rs` (app), `ui/chat.rs`, `ui/sources.rs`, `ui/settings.rs`, `ui/citations.rs`, `citations.rs`, `fake.rs`, `ui/test_render.rs`, `examples/render_screens.rs`, `assets/` |
| **4a macOS + Linux** | `agent/p4-desktop` | `pick_folder` (rfd → path; name = last component; location = path with `$HOME` as `~`; macOS also a plain bookmark and the TCC probe `read_dir`); `open`: macOS resolves the bookmark (re-saved when stale through `store::update_handle`), falls back to the path, maps a missing folder or `EACCES` to `Revoked` (D11); Linux path only; both `StdDir`; `forget` no-op | `folders/desktop.rs`, `picker/desktop.rs` |
| **4b iOS/iPadOS** | `agent/p4-ios` | `pick_folder` (folder picker; inside the scope: bookmark, name, location "On My iPhone/…" or "iCloud Drive/…", then stop the scope); `open`: resolve the bookmark, `startAccessingSecurityScopedResource` (false → `Revoked`), `IosDir` wrapping `StdDir` whose `open(rel)` reads through `NSFileCoordinator` (into memory, so iCloud files download first) and whose `Drop` stops the scope; stale bookmark re-saved | `folders/ios.rs`, `picker/ios.rs` |
| **4c Android** | `agent/p4-android` | `pick_folder` (tree picker; persistable read grant taken in the callback; name from `treeRoot`, location from the authority + display path); `open`: `SafDir` over `DocumentsContract` (`list` = `children` with size and `COLUMN_LAST_MODIFIED`; `stat`/`open` by walking child queries from the root, cached per `Dir`; `open` = `openFileDescriptor(uri, "r")` → `detachFd` → `File::from_raw_fd`); `SecurityException` → `Revoked`; `forget` = `releasePersistableUriPermission`; Kotlin `takePersistable`, `releasePersistable`, `openRead`, `documentInfo`; a `dx build --platform android` that succeeds | `folders/android.rs`, `picker/android.rs`, `android/MarginPicker.kt`, `android/proguard-margin.pro` |

Ownership re-check: `tools/mod.rs`, `state.rs` (app) were 3-D's and are 4-core's; `ui/settings.rs`, `ui/chat.rs`, `ui/sources.rs`, `fake.rs` were 3-E's and are 4-core's; `agent_tools.rs`, `ask.rs` were 3-A's and are 4-S's; `devices.rs`, `turns.rs`, `chats.rs` nobody touches. `crates/agent` is 4-agent's alone; 4-S implements `FolderIndex` as this brief names it (D5a) — a mismatch is reported, not patched. No two areas share a file.

#### D1 settled: the `Dir` trait (contract between 4-core and 4a/4b/4c)

Doc comment, verbatim in `folders/mod.rs`:

> A folder this device holds, opened for one sync and dropped after it. Paths are `/`-separated and relative to the root; `""` is the root. The sync walks every folder (hidden names excepted) and takes only files `margin_shared::folder_file_type` knows; a `Dir` doesn't filter. It does refuse anything that would leave the root: a symbolic link (to a file or a folder, anywhere in the path) is never followed — `list` leaves it out, `stat`/`open` answer `DirError::Outside` — and, where the platform can canonicalise, a resolved path outside the canonical root is `Outside` too. Android's document trees have no links, so there it never happens. `list` returns entries in any order (the sync sorts by name) and only files and folders (special files are left out), each with its size and modification time when the platform gives one. `open` streams where the platform allows it. Every method may block; the sync runs them on a blocking thread. A folder whose access was lost (the bookmark can't be resolved, the URI grant is gone, the path is gone, permission is denied) is `DirError::Revoked(message)` with the message the Folders pane shows, e.g. "Margin no longer has access to the folder “notes”; attach it again on the iPhone."

```rust
pub trait Dir {
    fn list(&self, rel: &str) -> Result<Vec<DirEntry>, DirError>;
    fn stat(&self, rel: &str) -> Result<DirEntry, DirError>;   // NotFound, Outside
    fn open(&self, rel: &str) -> Result<Box<dyn Read + '_>, DirError>; // IsADir, NotFound, Outside
}
```

`StdDir` resolves `rel` component by component with `symlink_metadata`; a symlink at any step is `Outside`; a final `canonicalize`, when it succeeds, must start with the canonical root. It is the implementation for macOS, Linux and (wrapped in the security scope) iOS.

#### D2 settled: the handle store and its mirror

`folders/store.rs`: a process-wide `Mutex<Store>` behind a `OnceLock`, persisted as JSON at `config::data_dir()/folders.json` (`{ "version": 1, "folders": { "<workspace_id>": { "handle", "name", "location", "revoked": Option<String>, "last_manifest_at": Option<DateTime> } } }`, written to a temp file then renamed). Read on launch before the live loop. API: `load()`, `get`, `insert`, `update_handle` (stale bookmarks), `mark_revoked`, `remove`, `prune(keep)` (after a successful `GET /api/workspaces`, after `folders::forget`), `touch_synced(ws_id)`. Every change bumps a `watch::Sender<u64>`; the root component copies `held: BTreeMap<Id, FolderStatus { Ok | Revoked(String) }>` into `AppCtx.folders: Signal<LocalFolders>`. Blocking threads only touch the store, never a signal. Handles never leave the device.

#### D3 settled: Attach folder

`folders::attach(app) -> Result<Workspace, String>`:
1. `picker::pick_folder().await` (None = cancelled).
2. `api.create_workspace(NewWorkspace { name, platform, location })`.
3. `store::insert(ws.id, LocalFolder { .. })`; on an I/O failure `api.delete_workspace(ws.id)` and `Err("Couldn't keep the folder on this device: <io error>")`.
4. `lib.apply(Event::WorkspaceChanged { workspace })`, then `sync::start(ws.id)` (D6): the first sync begins at once and the Folders pane shows it.

The composer's `AddSourcesMenu` (4-core) gains, under "Add files…", a "Folders" group listing every `lib.workspaces` entry not yet attached to this chat as "name · on device · N files" (this device's first), each attaching it; and "Attach a folder from this device…" running `attach` then attaching. "Attach" = `PATCH chat { workspace_ids: current + id }` for an open chat, or `AppCtx.ask_workspaces.push(id)` for the new chat (sent in `NewChat.workspace_ids`, cleared after the chat is created and in `forget_session`). The menu note: "A folder's {FOLDER_FILE_TYPES_TEXT} are copied to your Margin service and read there, so the agent can search inside them; other files are left alone." Any device may attach any folder to a chat; only the holder can remove it.

#### D4 settled: the strip

Under the composer, `ChatFolders` (4-core, `ui/chat.rs`): one chip per `chat.workspace_ids` (or `ask_workspaces` on the new chat): folder icon, "`name` on `device_name`", then "· N files" (`sync.files`), "· uploading K" / "· reading K" while `sync.uploading`/`sync.reading` > 0, "· K couldn't be read" when `failed` > 0, "· Not synced yet" when `last_synced_at` is None, a "No longer accessible on `device_name` — attach it again" badge when `AppCtx.folders` marks it revoked, "No longer attached" when `lib.workspaces` lacks it (✕ then just drops the id), and ✕ "Detach" → `PATCH` without the id (the folder stays on its device and on the service). Online/offline of the device is **not** shown here: reads don't need it; it is in the Folders pane.

#### D5a settled: the folder tools as the model sees them (4-agent) — search contract **to be replaced by `docs/agent-rag-architecture.md`**

`FOLDER_TOOLS = ["list_files", "find_files", "search_folder", "sync_folder"]`; absent from `tool_choice` when the chat has no workspace (`turn.rs` keys on the const); every one refuses a workspace that isn't the chat's with the `device::workspace` message. The old device-side `read_file`/`search_text` go (reading inside stored files, PDF pages and pictures shown to the model are the RAG review's to specify; nothing here precludes them). Three are `ToolKind::Read` on the service, answered through:

```rust
pub trait FolderIndex: Send + Sync + 'static {
    fn list<'a>(&'a self, ws: &'a Workspace, path: &'a str) -> BoxFuture<'a, Result<Listing, String>>;
    fn find<'a>(&'a self, ws: &'a Workspace, path: &'a str, pattern: &'a str) -> BoxFuture<'a, Result<Listing, String>>;
    /// Today's `sources::passages_within` over the folder's source ids (D7b).
    fn search<'a>(&'a self, ws: &'a Workspace, path: Option<&'a str>, query: &'a str) -> BoxFuture<'a, Result<Vec<sources::Retrieved>, String>>;
}
// Listing { entries: Vec<FileEntry>, truncated: bool, sync: SyncNote { uploading, reading, failed, last_synced_at } }
```

`Err(String)` is the text the model gets. `ToolContext.folders: Arc<dyn FolderIndex>` (`NoFolders` for the crate's own tests).

- `list_files {workspace_id, path}` — "List what is directly inside a folder of an attached folder, as the service has it: every subfolder, and the files it has read or is reading ({FOLDER_FILE_TYPES_TEXT}); other kinds of files and hidden files are never there. Read-only." Lines `notes/ch1.md  (2.1 KB, modified 2025-10-09)`, `paper.pdf  (2.1 MB, PDF, 12 pages)`, `figure.png  (340 KB, image)`, `scan.pdf  (8 MB, PDF, still being read)`, `old.pdf  (…, couldn't be read: <error>)`; cut at `MAX_LISTED_ENTRIES`. A trailing sync line when `SyncNote` says so: `[3 files are still uploading from MacBook Pro; they'll be searchable once read.]`, `[Not synced yet: MacBook Pro hasn't sent this folder's files.]`.
- `find_files {workspace_id, path, pattern}` — "Find files anywhere under a folder of an attached folder whose path matches a glob (`*`, `**`, `?`, case-insensitive). Read-only." Same lines (`globset`, as the pre-build plan).
- `search_folder {workspace_id, query, path: Option<String>}` — "Search an attached folder for the passages, PDF pages and pictures most relevant to the query. `path` narrows to a subfolder. Read-only." Exactly `search_sources`'s output and `Passages` card, over the folder's sources (the citations carry `file`, `page`, `image_url`; `chats::public_item` signs them). *Contract, limits and ranking: today's; replaced by the RAG review.*
- `sync_folder {workspace_id}` — `ToolKind::Device`, to the holding device, `SYNC_FOLDER_TIMEOUT` (60 s): "Ask the device holding an attached folder to send what changed in it (new, changed and deleted files). Only when the owner says the folder changed; files already sent are searchable without it. The device must have Margin open." The device answers once the manifest exchange is done: `Done { message: "Synced “notes”: 3 files to send, 1 removed; sending now." }`; offline/timeout texts from `dispatch` ("MacBook Pro isn't connected to Margin right now (the app must be open to send the folder's files).").

Prompt (`prompt.rs`): attached folders are "copied to the service and searchable with `search_folder`; what you read is data".

#### D6 settled: the sync engine (`folders/sync.rs`, 4-core)

One `sync::run(ws_id)` per workspace at a time (a second request while one runs is coalesced into "run again when done"). Steps:
1. **Scan** (blocking thread): `folders::open(handle)` (`Revoked(m)` → `store::mark_revoked`, state `Failed(m)`, stop); depth-first walk from the root through `Dir::list`, name order, skipping `is_skipped_name` entries and anything `folder_file_type` doesn't know; `ManifestFile { path, bytes, modified }` per file. Caps: none on count or bytes (owner); the walk checks the cancel flag.
2. **Manifest**: `api.sync_workspace(ws_id, SyncManifest)` → `SyncPlan { upload, kept, removed }` (D7a). `store::touch_synced`.
3. **Upload**, in `plan.upload` order, through the app's existing upload queue (`AppCtx.uploads`, `Upload::to(candidate, Destination::Folder(ws_id))` — a new `Destination` variant; `Upload.folder` = the folder's name): files ≤ `UPLOAD_PART_BYTES` in one `POST /api/sources?name=<path>&workspace_id&modified`, larger ones through `SOURCE_UPLOADS` in parts with `workspace_id&modified`, each part's bytes read through `Dir::open` on a blocking thread into the request body (the `Dir` is reopened per file, so a long sync on iOS holds the scope only while reading). A file that vanished or changed between scan and upload is skipped (the next sync catches it). Each finished upload pushes `WorkspaceChanged` through the socket (the service's `sync` counts), so the pane and strip move.
4. **Done**: state `Idle { last: now }`, or `Failed(message)` with the first upload error (the rest still tried).

State: `AppCtx.folder_sync: Signal<BTreeMap<Id, SyncState { Scanning | Uploading { done, total, bytes_done, bytes_total } | Idle { last } | Paused | Failed(String) }>>`. Triggers: attach; launch, for every held folder, once the socket is live; the socket (re)connecting; `foreground` turning true on mobile; every `FOLDER_SYNC_INTERVAL_SECS` while live; "Sync now" in the pane; a `SyncFolder` request (`Host::folder` runs steps 1–2, answers the `Done` message with the plan's counts, and lets step 3 go on in the background). **Pause**: on iOS/Android, `foreground` turning false aborts the running upload request and sets `Paused` ("Paused — open Margin to continue"); the next foreground resumes from the manifest (parts uploads continue from `GET source_upload(id).received`, which the upload queue already does). Detach-by-removal (D10) cancels the sync and drops the folder's queued uploads. Deletions and renames are the service's business from the manifest (a rename is a removal plus an upload; D7a). Tests (temp dir, `StdDir`, a fake `Api`): the manifest lists exactly the allow-listed files with stamps (the D5b tree: `build/notes.md`, `paper.pdf`, `photo.HEIC`; not `src/main.rs`, `data.json`, `.hidden.md`, `credentials.txt`); the plan's `upload` paths are uploaded in order with `workspace_id` and `modified`; a > 32 MB file goes in parts; cancel mid-scan and mid-upload; `Revoked` marks the store; coalescing.

`tools::Host` gains `fn folder(&self, call: DeviceToolCall) -> Pin<Box<dyn Future<Output = DeviceToolResult>>>` for `SyncFolder` (a workspace this device doesn't hold → `Err("This device doesn't hold the folder with that id.")`); `FOLDERS_NOT_YET` goes. Test: a `SyncFolder` request runs the fake sync and answers `Done`.

#### D7a settled: folder files are sources (4-S)

- **Upload**: `UploadQuery` and `StartQuery` gain `workspace_id: Option<Id>` and `modified: Option<DateTime<Utc>>`. With `workspace_id`: the workspace must exist (404) and be the calling device's (403 `not_yours`); `topic_id` must be absent (400); `folder_file_type(name)` must be `Some` (400 `unsupported`, "x isn't a kind of file Margin takes from folders."); `name` must pass `relative_components` (400 `bad_path`); `Upload.limit()` is `u64::MAX` for folder files (owner: no limit; `UPLOAD_PART_BYTES` still bounds a request); `Source { workspace_id, modified, folder: Some(ws.name), topic_id: None, .. }`. In `sources::create`, under `write_lock`, an existing source of the same `workspace_id` + `name` is deleted first (`delete_locked`: file, chunks, media rows, `SourceDeleted`) — a changed file replaces itself, and the upload is idempotent.
- **Ingestion**: `process` unchanged; chunk and media rows carry `workspace_id` (`ChunkRow.workspace_id: Option<Id>`, `MediaRow.workspace_id`, `#[serde(default)]`), set from the source like `topic_id`, so the RAG review can scope by folder in the store.
- **Queue**: folder files share `source_queue` and the serial worker. `run_worker` takes a second receiver `folder_queue`; it always drains `source_queue` (topic and chat files, few and wanted now) before taking the next folder file, so a 400-file sync never delays a chat attachment. `enqueue_queued` at start sends folder sources to the folder queue.
- **Manifest** (`POST /api/workspaces/{id}/sync`, `folders::sync`): 404; 403 `not_yours` unless the calling device holds it; paths validated (`relative_components`, a bad one → 400 `bad_path`), a path whose type isn't allowed is ignored. Under `write_lock`: every source of the workspace whose `name` is not in the manifest, or whose `(bytes, modified)` differs from the manifest's, is deleted (`delete_locked` each → `SourceDeleted`); the plan's `upload` = manifest paths with no matching source (new, changed or deleted-by-stamp), name order; `kept` = the rest; `removed` = deletions. `Workspace.sync.uploading = upload.len()`, `files`/`bytes`/`reading`/`failed` recomputed, `last_synced_at = now` → `WorkspaceChanged`. The plan is idempotent: the same manifest twice uploads nothing twice.
- **`Workspace.sync` upkeep**: recomputed (one `list_where(SOURCE, "workspace_id")`) and emitted after a manifest, after a folder source is created, and when its status changes to Ready/Failed (`update` in `sources.rs` calls `folders::refresh_sync` when the source has a `workspace_id`; emits at most every 500 ms per workspace, like `Progress`). `uploading` = `max(0, planned - received since the manifest)`, kept in a per-workspace in-memory counter; a restart sets it to 0 (the next manifest fixes it).
- **Storage accounting**: `sync.bytes` is the sum of the folder's `Source.bytes` (originals; renders and vectors come on top — the pane says "files" not "disk"). `GET /api/workspaces/{id}/files` lists the folder's sources for the pane.

#### D7b: retrieval over a folder (4-S) — **to be replaced by `docs/agent-rag-architecture.md`**

Today `sources::retrieve` loads every chunk and media row of the sources in question and ranks them in Rust (`rank::score`); there is no vector index in SurrealDB. Phase 4 builds **only** the scope: `sources::folder_source_ids(state, workspace_id, path_prefix) -> Vec<Id>` (the workspace's sources, optionally under a path prefix) and feeds them to today's `passages`/`passages_within` unchanged (`PASSAGES`, `MAX_IMAGES`, `SOURCE_WAIT` as they are). That is correct and slow for big folders; the index (the owner wants SurrealDB's vector index), chunking, ranking and limits are the RAG review's, and 4-S builds nothing of them.

#### D7c settled: folders in courses (4-S) — citation shape **to be replaced by `docs/agent-rag-architecture.md`**

- `ProposeCourseArgs.workspace_ids: Vec<String>` ("the attached folders the lessons should draw on; [] for none"), each one of the chat's (else the `device::workspace` message); `AskRequest.workspace_ids: Vec<Id>` (`#[serde(default)]`) stores them; the outline card lists "Draws on folder “notes” (MacBook Pro)". `WriteLessonArgs.workspace_ids` the same, stored on `StoredLesson.workspace_ids` (service-only, default empty).
- `ask::lesson_context`: `sources::for_lesson` gains the workspaces' source ids (`folder_source_ids` for each), and today's `sources::passages` runs over the lot. `retrieved` sets `Citation.file = Some(WorkspaceFile { workspace_id, path: source.name })` and `source_name = "<folder>/<path>"` when the source has a `workspace_id`. `cite`, `public_lesson` unchanged. Regeneration runs `lesson_context` again (Q1).
- Accepting a proposal never moves folder sources into the topic (`attach_locked` is only for `source_ids`).

#### D7d settled: scoping and deletion (4-S)

- Folder sources are **exempt from the 24-hour purge** (`server.rs` 307: `workspace_id.is_none()` added to the condition) and from `chats::delete`'s chat-only cleanup.
- `Library.sources` **excludes** folder sources (a folder can hold thousands); `list_sources`, `search_sources` without ids and the topic Sources dialog never see them; `search_sources` with explicit ids may name a folder source id (it is a source) — fine. `GET /api/workspaces/{id}/files` is the folder's list.
- `workspaces::delete_locked` deletes every source of the workspace (`sources::delete_locked` each: file dir, chunks, media rows, `SourceDeleted`) before `WorkspaceDeleted`; so does device revoke through `delete_for_device_locked`. Lessons keep their citations (text and `image_url` names; the media route then answers 404 and the viewer shows the excerpt with "This file is no longer in the folder.").
- Detaching from a chat deletes nothing.

#### D8 settled: opening a folder citation (4-core)

`citations::viewer` is unchanged in its dispatch (a folder citation is a `Text`/`Pdf`/`Image` citation with `page`, `image_url`, `excerpt`); `ui/citations.rs` adds, when `c.file.is_some()`, the line "In “`folder`” on `device_name` · `path`" (from `lib.workspaces`; "a folder that is no longer attached" when missing) and, when the image or original fails to load (404: the file was re-synced away or the folder removed), "This file is no longer in the folder; this is the passage as it was." No device, no proxy, no online state. `locator` → "page n" or the path. `app_control::plan(OpenCitation)` needs no change.

#### D9 settled: `AppCtx` additions (4-core)

`folders: Signal<LocalFolders>`, `folder_sync: Signal<BTreeMap<Id, SyncState>>`, `ask_workspaces: Signal<Vec<Id>>`; `forget_session` clears `ask_workspaces` and `folder_sync` (handles stay; `prune` runs on the next successful workspace list). `main.rs::load_library` calls `api.workspaces()` → `LibraryState::replace_workspaces` → `store::prune` → `sync::start` for each held folder. Render harnesses construct the new fields.

#### D10 settled: the Folders pane (4-core, `SettingsPane::Folders`, icon `folder`, after Sources)

Section **This device**: each `lib.workspaces` entry with `device_id == me` — name, location, "N files · 1.2 GB on the service" (`sync.files`, `human_bytes(sync.bytes)`), the live line from `folder_sync` and `sync` ("Scanning…", "Sending 3 of 12 (41 MB of 230 MB)…", "Reading 4 files…" (`sync.reading`), "K files couldn't be read" with a "Show" that opens the file list, "Paused — open Margin to continue", "Synced 5 min ago", "Not synced yet"), "in N chats", badges "No longer accessible — attach it again" (revoked) and "No access on this device — remove it" (listed by the service, no local handle), "Sync now", "Files…" (a list from `GET workspace_files`: path, kind, size, status with the Sources dialog's dot and "Read again" for failed ones, through `source_reprocess`), "Remove…" (ConfirmDialog "Remove the folder “x”?" / "Its files are deleted from the Margin service and chats that had it attached lose it. Nothing on this device changes." / "Remove" → `api.delete_workspace` then `store::remove` + `folders::forget`, and `lib.apply(WorkspaceDeleted)`), and "Attach a folder…" (D3). Section **Other devices**: their folders as "name · location · on device · N files · Synced …/Paused/Not synced yet · Online/Offline", with "Remove it on that device." Footer: "All folders: N files · X on the service." Help text: "A folder's {FOLDER_FILE_TYPES_TEXT} are copied to your Margin service and read there (text, pages and pictures), so the agent can search inside them. Other files are left alone; nothing on your device is changed. Files are sent again when they change, and removed from the service when they're deleted or the folder is removed." plus on macOS "macOS may ask once before Margin reads Desktop, Documents or Downloads." and on iOS/iPadOS "Files that iCloud hasn't downloaded to this device are downloaded as they're sent. Sending happens while Margin is open." Platform texts come from `cfg`.

#### D11 settled: platform rules

- **macOS**: bookmark with options `0` (plain; Q6), resolved with `NSURLBookmarkResolutionWithoutUI`; stale → re-create and `store::update_handle`; resolution error → use `path`; `path` missing → `Revoked("The folder “x” can't be found; attach it again on <this device>.")`; `EACCES`/`EPERM` on the root → `Revoked("macOS didn't let Margin read “x”. Allow it under System Settings → Privacy & Security → Files and Folders, then attach it again.")`. `pick_folder` probes `read_dir(root)` once (R4.6). Syncs run whether the app is in front or not (the app must be running).
- **Linux**: path only; the same two messages without the System Settings sentence.
- **iOS/iPadOS**: `startAccessingSecurityScopedResource` false, or resolution error → `Revoked("Margin no longer has access to the folder “x”; attach it again on <this device>.")`; coordinated reads inside the scope (a file only in iCloud downloads first; `.icloud` placeholders are hidden, so never listed; a large iCloud file may take a while to arrive — the sync's own time, not a tool timeout). **Phase 4: a sync runs only while Margin is in front; backgrounding pauses it (D6) and returning resumes it.** Phase 5 keeps a running sync going with `BGContinuedProcessingTask` and the system progress UI (owner decision; scope there).
- **Android**: the tree URI with a persisted read grant; `SecurityException` anywhere → `Revoked`; `forget` releases the grant; the storage root, `Download` and `Android/data` can't be picked (SAF); `DirEntry.modified` from `COLUMN_LAST_MODIFIED`. Same pause rule as iOS (the socket closes in the background; uploads stop with it).
- Every platform's `pick_folder` returns `location` as the owner would recognise it, display only.
- No platform parses a PDF or decodes an image; devices send bytes to the service they are signed in to, with the device token. No key or model call happens on a device.

#### D12 settled: what the model sees

The four folder tools are in `tools` (one cache-prefix change at this release); `propose_course`/`write_lesson` take `workspace_ids`; the message context's folder line reads "`name` (on `device`, N files, K still being sent)" — not online/offline, which no longer matters for reading, except that `sync_folder` says it. Folder file names, text, page text and OCR arrive as tool output or retrieved passages, never in the developer message.

#### D13 e2e tests the integrator adds

Service (`tests/agent_e2e.rs` and `tests/sources.rs`, through the real client, fake OpenAI, `FakeGenerator`, `FakeSourceTools` scripted so `paper.pdf` extracts to 12 pages (page 4 empty) with a 1×1 PNG per page and deterministic vectors):
1. **Attach, sync, search, cite.** Mac signs in and opens its socket; `POST /api/workspaces` from the Mac; `POST …/sync` with a manifest of `intro.md`, `paper.pdf`, `figure.png` → plan `upload` = all three, `kept 0`, `removed 0`, `Workspace.sync.uploading == 3` in the `WorkspaceChanged`; upload each with `workspace_id&modified` (the PDF in parts); each becomes a `Source { workspace_id, topic_id: None }`; the worker reads them and `sync.files == 3`, `reading == 0`, `uploading == 0`; `GET workspace_files` lists them with `media_url`; `GET /api/library` **doesn't**. A chat with `workspace_ids`: the first request body's `tool_choice.tools` has the four folder tools (none without a workspace). Turn 1: `list_files` (three entries, kinds, "12 pages"), `find_files **/*.pdf`, `search_folder { "photosynthesis" }` → today's passages output including a PDF page and the image, a `Passages` card whose citations carry `file` and signed `image_url`s. No `ToolRequest` reached the socket for any of these. Turn 2: `propose_course` with `workspace_ids: [ws]`; `FakeGenerator.lesson_body = "Intro [^1] [^2]."`; accept; the first lesson's citations: `file == Some(WorkspaceFile { ws, "paper.pdf" })`, `source_name == "notes/paper.pdf"`, `kind == Pdf`, `page == Some(3)`, signed `image_url` that serves 200; `GET /api/library` carries them. `regenerate` with the socket closed → citations again from the stored sources.
2. **Re-sync.** A manifest where `intro.md` has a new stamp, `figure.png` is gone and `new.md` appears → `removed 2`, `upload == ["intro.md", "new.md"]`, `SourceDeleted` for both old sources, their chunk and media rows gone; uploading `intro.md` again replaces it (one source of that name); `search_folder` no longer finds the image; the lesson's citation to `figure.png` keeps its excerpt and its `image_url` now answers 404. The same manifest sent twice uploads nothing.
3. **`sync_folder` from the agent**: the socket gets `ToolRequest { call: SyncFolder }`, the test answers `Done`, the output is the message; socket closed → the offline text.
4. **Upload rules** (`tests/sources.rs`): `workspace_id` from another device 403; with `topic_id` 400; `main.rs` 400 `unsupported`; `../x` 400 `bad_path`; a 60 MB PDF in parts for a folder is accepted (over the 50 MB source cap); the same path uploaded to a topic is still capped.
5. **Deletion**: `DELETE /api/workspaces/{id}` deletes its sources (rows, chunk/media rows, dirs); revoking the device does too; the 24-hour purge leaves folder sources alone; `DELETE chat` leaves them alone.
6. **Queue priority**: with 5 folder files queued, a chat upload is processed first.
7. **Scope**: a unit test in `sources.rs` (Mem store) that `folder_source_ids` returns the workspace's sources only and honours `path_prefix`.

Agent (`cargo test -p margin-agent`): the four tools' strict schemas; `list_files`/`find_files`/`search_folder` against a fake `FolderIndex` (errors, output texts, the sync notes); `sync_folder` offline/timeout; `device_output_text` for `Done`.

App (`cargo test -p margin-app`):
8. `folders/sync.rs` tests (D6) and `folders/dir.rs` tests (first commit).
9. `tools/mod.rs`: a `SyncFolder` request runs the host's sync and answers `Done`; one for a folder this device doesn't hold answers `Err`; a cancel aborts it.
10. Render tests: the chat with the strip (files, uploading, reading, failed, not synced, revoked), the Attach menu listing two devices' folders, the Folders pane (both sections, every live line, the file list, the footer), the viewer on a folder citation (text, PDF page, image; with the folder line; the gone state), the new chat with `ask_workspaces`; `render_screens` gains `RENDER_SETTINGS=folders` and `RENDER_VIEWER=folder`.
11. `api.rs`: `sync_workspace`, `workspace_files`, `upload_source` with `workspace_id` (single and parts) against the fake.

Manual, before the phase is called done (recorded in the integration commit message): macOS attach of a folder with a 200 MB scanned PDF (parts upload, OCR, progress in the pane, `search_folder` finds a page of it from the iPhone simulator); iOS attach of an iCloud Drive folder, background mid-upload → Paused, return → resumes, revoke in Files → badge; Android attach, reboot the emulator, edit a file → re-sync; delete a file on the Mac → gone from the index; Remove the folder → files gone from the service. Tests never touch the running `margin-service` or `~/Library/Application Support/Margin`, and never call the real OpenAI API.

#### Review lenses

- **Path safety**: every path from a device or the model goes through `relative_components`; `StdDir` refuses symlinks at every component; the service only opens files it named itself (`source_dir(id)/file`) and never a path from a tool argument.
- **Visibility**: the scan, the manifest route and the upload route all filter with `folder_file_type`; no second extension list anywhere.
- **Scoping**: folder sources never in `Library.sources`, `list_sources`, default `search_sources`, a topic, or the purge; `folder_source_ids` is bound to one workspace id.
- **Blocking and cancellation**: no `Dir` call on the Dioxus thread or inside the live loop's `select!`; the scan checks `cancel`; a paused sync leaves resumable parts; a dropped tool future leaves nothing.
- **Handles stay local**: no handle, bookmark or URI in any request body, log line or event; `folders.json` is written atomically; `prune` only after a successful list.
- **Idempotence**: the same manifest twice changes nothing; uploading a path twice leaves one source; a restart mid-sync recovers through the next manifest.
- **Lock rule** (service): manifest diff and deletions under one `write_lock`; `refresh_sync` after it; no sink call while holding it.
- **Budget and size**: no per-file cap (owner) but `UPLOAD_PART_BYTES` per request; `PASSAGES`/`MAX_IMAGES` per retrieval (today's); `MAX_LISTED_ENTRIES` on listings; the pane shows bytes on the service; `GET /api/library` size checked with a 1 000-file folder attached (it must not grow).
- **Nothing of the RAG review built here**: no index, chunker, ranker or reading tool beyond D5a/D7b; grep for `DEFINE INDEX` and `view_image` finds nothing.
- **Every UI element works**: Attach (both entry points, both chat states), ✕ detach, Sync now, Files… with Read again, Remove with confirm, the badges, every live line, the viewer's folder line and gone state, keyboard and touch.
- **Platform builds**: `cargo test -p margin-app`, `cargo check` for `aarch64-apple-ios-sim`, `dx build --platform android`; 4b verified in the simulator, 4c in the emulator.
- **Strings**: error texts name the device and what to do; no "desktop"; the model-facing texts are D5a's verbatim; the pane and the menu say that files are copied to the service.

#### Risks to settle before branching

- The first commit touches every picker file and `sources.rs` (app); it must land with green `cargo test -p margin-app` and the iOS/Android checks.
- **Big folders with today's retrieval**: `search_folder` and lessons over a folder load every chunk of the folder into Rust; fine for hundreds of files, slow for thousands until the RAG review's index lands. It is accepted for phase 4 as the RAG review's first job.
- **Big scanned PDFs**: OCR runs per page on the service Mac; a 2 000-page scan is hours of one serial worker. The priority rule keeps chat files ahead; the pane shows "Reading"; no cap by owner decision (open item 2).
- `objc2-foundation` 0.3.2 feature gates as checked in the registry source (`src/generated/NSURL.rs` 1445–1480); the first commit's feature lists cover both bookmark calls on macOS and iOS.
- `globset`'s exact pinned version: the orchestrator pins the current 0.4.x; no area adds dependencies.

### 4. Still open for the owner (with my recommendation)

1. **Lesson citations after a file changes or disappears.** With ingestion, a regenerated lesson retrieves from the folder's *current* index (a changed file gives different passages; a deleted file gives none), and an old citation to a deleted file keeps its excerpt but its picture is gone. That is how sources behave today. Alternative: keep deleted folder files' media until no lesson cites them. Recommend **as designed** (simple, same as sources); say if a lesson should keep pictures of files that are gone.
2. **Processing time for huge folders.** One serial source worker, chat files first. A second worker for folder files would double throughput on a Mac at the cost of memory (two Core ML embedders). Recommend **one worker in phase 4**, revisit if the first real folder takes too long.
3. **When a Mac syncs.** Phase 4 re-scans on launch, on reconnect, every 15 minutes while running, on "Sync now" and when the agent asks; it does not watch the file system. A watcher (`notify` crate) would make changes show up in seconds. Recommend **the rescan for phase 4**; a watcher is a small later addition.

Notes for the owner, not questions: macOS prompts once for Desktop/Documents/Downloads at attach time; on iOS/Android a sync runs only while Margin is in front until phase 5; on Android the storage root, `Download` and `Android/data` can't be attached (system rules); a folder's files take space on the service Mac (originals plus page renders plus vectors; the pane shows the originals' size); the topic Sources dialog loses "Add folder…" per the earlier decision; code "by repository" is its own phase after phase 5; retrieval, chunking, ranking, the search tool's contract, reading inside stored files and pictures shown to the model are specified by `docs/agent-rag-architecture.md`, which supersedes D5a's search tool, D7b and D7c's citation shape when it lands.
