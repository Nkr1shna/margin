//! HTTP routes, relative to the service base URL (e.g. `https://margin.krishnanelloore.com`).
//!
//! Everything under `/api` requires `Authorization: Bearer <device token>`, except
//! `/api/version` (open, like `/health`) and the media routes, which are authorized by
//! the signed query string the service puts in `VideoMedia` URLs. Errors use `ApiError`
//! as the body.
//!
//! | Method | Path                                   | Body               | Response            |
//! |--------|----------------------------------------|--------------------|---------------------|
//! | GET    | /health                                |                    | "ok"                |
//! | GET    | /api/version (no token)                |                    | version::VersionInfo { version, sha, environment, started_at, rss_bytes, physical_memory_bytes } |
//! | POST   | /auth/start                            | StartLoginRequest  | StartLogin          |
//! | POST   | /auth/poll                             | PollLoginRequest   | LoginState          |
//! | GET    | /auth/github/callback?code&state       |                    | HTML page           |
//! | GET    | /api/me                                |                    | Me                  |
//! | GET    | /api/library                           |                    | Library             |
//! | POST   | /api/topics                            | NewTopic           | Topic               |
//! | PATCH  | /api/topics/{id}                       | TopicPatch         | Topic               |
//! | DELETE | /api/topics/{id}                       |                    | 204 (cascades)      |
//! | PATCH  | /api/lessons/{id}                      | LessonPatch        | Lesson              |
//! | DELETE | /api/lessons/{id}                      |                    | 204 (cascades)      |
//! | POST   | /api/lessons/{id}/regenerate           |                    | Lesson              |
//! | POST   | /api/notes                             | NewNote            | Note                |
//! | PATCH  | /api/notes/{id}                        | NotePatch          | Note                |
//! | DELETE | /api/notes/{id}                        |                    | 204                 |
//! | GET    | /api/proposals/{id}                    |                    | Proposal            |
//! | POST   | /api/proposals/{id}/accept             | AcceptProposal     | Topic               |
//! | POST   | /api/lessons/{id}/video                |                    | Video (Queued)      |
//! | GET    | /api/videos/{id}                       |                    | Video               |
//! | GET    | /api/search?q=                         |                    | Vec<SearchHit>      |
//! | GET    | /api/settings                          |                    | Settings            |
//! | PATCH  | /api/settings                          | SettingsPatch      | Settings            |
//! | GET    | /api/pipeline/variants                 |                    | pipeline::PipelineVariants |
//! | GET    | /api/tools                             |                    | Vec<ToolStatus>     |
//! | GET    | /api/devices                           |                    | Vec<Device>         |
//! | DELETE | /api/devices/{id}                      |                    | 204                 |
//! | POST   | /api/sources?name&folder&topic_id&workspace_id&modified | file bytes | Source (Queued) |
//! | DELETE | /api/sources/{id}                      |                    | 204 (chunks too)    |
//! | POST   | /api/sources/{id}/reprocess            |                    | Source (Queued)     |
//! | GET    | /api/sources/{id}/transcript           |                    | Vec<TranscriptSegment> |
//! | POST   | /api/source-uploads?name&folder&topic_id&workspace_id&modified&bytes | | SourceUpload |
//! | PUT    | /api/source-uploads/{id}?offset        | part bytes         | SourceUpload        |
//! | POST   | /api/source-uploads/{id}/finish        |                    | Source (Queued)     |
//! | DELETE | /api/source-uploads/{id}               |                    | 204                 |
//! | GET    | /api/embedding                         |                    | EmbeddingModel      |
//! | POST   | /api/embedding/download                |                    | EmbeddingModel      |
//! | DELETE | /api/embedding                         |                    | EmbeddingModel      |
//! | GET    | /api/chats                             |                    | Vec<Chat> (newest first) |
//! | POST   | /api/chats                             | NewChat            | ChatDetail          |
//! | GET    | /api/chats/{id}                        |                    | ChatDetail          |
//! | PATCH  | /api/chats/{id}                        | ChatPatch          | Chat                |
//! | DELETE | /api/chats/{id}                        |                    | 204 (stops its turn; items, excerpts and chat-only sources too) |
//! | POST   | /api/chats/{id}/messages               | NewMessage         | MessageSent         |
//! | DELETE | /api/chats/{id}/pending                |                    | Chat                |
//! | POST   | /api/chats/{id}/stop                   |                    | Chat                |
//! | POST   | /api/chats/{id}/continue               |                    | Chat                |
//! | POST   | /api/chats/{id}/confirm                | ConfirmRequest     | Chat                |
//! | GET    | /api/chats/{id}/items/{item}/output    |                    | ToolOutputText      |
//! | GET    | /api/workspaces                        |                    | Vec<Workspace>      |
//! | POST   | /api/workspaces                        | NewWorkspace       | Workspace (of the calling device) |
//! | DELETE | /api/workspaces/{id}                   |                    | 204 (its files' sources too) |
//! | POST   | /api/workspaces/{id}/sync              | SyncManifest       | SyncPlan (403 not_yours, 404, 400 bad_path) |
//! | GET    | /api/workspaces/{id}/files             |                    | Vec<Source> (newest first, with media_url; 404) |
//! | GET    | /api/projects                          |                    | Vec<context::Project> |
//! | POST   | /api/context/search                    | context::ContextQuery | Vec<context::Hit> (400 bad_scope; `x-margin-agent` names the agent for the trace) |
//! | DELETE | /api/context/documents/{id}            |                    | 204 (404; 403 forbidden: not an agent's) |
//! | GET    | /api/context/documents/{id}/outline    |                    | context::DocumentOutline (404; 409 not_ready) |
//! | GET    | /api/context/documents/{id}/passages?from&to |              | Vec<context::Hit> (404; 409 not_ready) |
//! | POST   | /api/context/ingest                    | context::ContextIngest | context::Document (400 bad_origin) |
//! | GET    | /api/context/changes?since&limit       |                    | context::ContextChanges (one page by generation: limit 1-500, default 100; `more`; 400 outside) |
//! | GET    | /api/traces?kind&from&to&trace_id      |                    | JSONL of context::TraceRow, oldest first (400 bad_range) |
//! | GET    | /api/traces/rollup?bucket&from&to      |                    | Vec<context::TraceRollup> (400 bad_range) |
//! | GET    | /api/events  (WebSocket upgrade; `?traces=1` adds Event::Trace) | DeviceFrame frames | ServerFrame frames  |
//! | GET    | /media/videos/{id}/{file}?exp&sig      |                    | video.mp4, poster.png, storyboard.png, captions.srt (range requests) |
//! | GET    | /media/sources/{id}/{file}?exp&sig     |                    | original, page-{n}.png, frame-{ms}.jpg (range requests) |

pub const HEALTH: &str = "/health";
/// `GET` → `{ version, sha, environment, started_at, rss_bytes, physical_memory_bytes }`,
/// without a token like `/health` (`docs/plan-deploy.md` §8).
pub const VERSION: &str = "/api/version";
pub const AUTH_START: &str = "/auth/start";
pub const AUTH_POLL: &str = "/auth/poll";
pub const AUTH_GITHUB_CALLBACK: &str = "/auth/github/callback";
pub const ME: &str = "/api/me";
pub const LIBRARY: &str = "/api/library";
pub const TOPICS: &str = "/api/topics";
pub const NOTES: &str = "/api/notes";
pub const SEARCH: &str = "/api/search";
pub const SETTINGS: &str = "/api/settings";
/// `GET` → `pipeline::PipelineVariants`: the stage names the service registers per kind
/// and the named variants (`evals/variants/*.toml`) Settings can pick a pipeline from.
pub const PIPELINE_VARIANTS: &str = "/api/pipeline/variants";
pub const TOOLS: &str = "/api/tools";
pub const DEVICES: &str = "/api/devices";
pub const EVENTS: &str = "/api/events";
/// Chats with the agent. `POST` with `NewChat.message` creates the chat and starts its
/// first turn; the answer streams in as events.
pub const CHATS: &str = "/api/chats";
/// Attached folders. `POST` registers a folder the calling device holds; only that
/// device may delete it (or a revoke of the device deletes its workspaces).
pub const WORKSPACES: &str = "/api/workspaces";
/// Upload: the body is the file's bytes (`application/octet-stream`, at most
/// `max_source_bytes` for its kind); `name` (required), `folder` and `topic_id` are query
/// parameters. Without `topic_id` the source waits to be named in an
/// `AskRequest.source_ids`. Files over `UPLOAD_PART_BYTES` go through `SOURCE_UPLOADS`
/// instead. Too big: 413 with code "too_large".
///
/// With `workspace_id` (and `modified`, RFC 3339) the file is a file of the calling
/// device's attached folder: `name` is its path inside the folder (400 "bad_path" unless
/// `device::relative_components` accepts it), `folder` is ignored and set to the folder's
/// name, `topic_id` must be absent (400), its type must be one
/// `device::folder_file_type` knows (400 "unsupported_source"), the folder must exist
/// (404) and be the caller's (403 "not_yours"). The kind's size limit doesn't apply
/// (owner: no limit); only `UPLOAD_PART_BYTES` bounds one request. An earlier source of
/// the same folder and path is replaced.
pub const SOURCES: &str = "/api/sources";
/// A source uploaded in parts. `POST ?name&folder&topic_id&bytes` (the query of
/// `SOURCES`, plus the file's size) checks the name and size and starts it; each
/// `PUT source_upload(id)?offset=<received>` appends a part of at most
/// `UPLOAD_PART_BYTES` (a part at any other offset is refused with 409 "wrong_offset",
/// and the body says where to go on); `POST source_upload_finish(id)` once all of it is
/// there turns it into a Queued `Source`. Unfinished uploads are dropped after a day.
/// `workspace_id` and `modified` work as for `SOURCES`.
pub const SOURCE_UPLOADS: &str = "/api/source-uploads";
pub const EMBEDDING: &str = "/api/embedding";
/// The projects (corpora) on this service; today exactly one, "My library".
pub const PROJECTS: &str = "/api/projects";
/// Hybrid retrieval for agents: the same code path as Margin's own agent's `search`
/// tool. `POST context::ContextQuery` → `Vec<context::Hit>`; a scope naming something
/// that doesn't exist is 400 "bad_scope".
pub const CONTEXT_SEARCH: &str = "/api/context/search";
/// `POST context::ContextIngest` → the new `context::Document` (indexed in the
/// background; `Event::DocumentChanged` when it is Ready). The origin must be
/// `Origin::Agent` (400 "bad_origin"); the document's `actor` is `agent:<name>`.
pub const CONTEXT_INGEST: &str = "/api/context/ingest";
/// `GET ?since=<generation>` → `context::ContextChanges`: documents indexed or deleted
/// since then, for agents keeping their own copy fresh.
pub const CONTEXT_CHANGES: &str = "/api/context/changes";
/// `GET ?kind&from&to&trace_id` → trace rows as JSON lines (`application/x-ndjson`),
/// oldest first. `from`/`to` are RFC 3339; a missing `to` is now, a missing `from` a day
/// before `to`; `from > to` is 400 "bad_range".
pub const TRACES: &str = "/api/traces";
/// `GET ?bucket=hour|day&from&to` → `Vec<context::TraceRollup>` from the pre-computed
/// views.
pub const TRACES_ROLLUP: &str = "/api/traces/rollup";
pub const EMBEDDING_DOWNLOAD: &str = "/api/embedding/download";
/// `DELETE` removes the speech model's files; `POST SPEECH_MODEL_DOWNLOAD` starts its
/// download. Both answer with the whole `EmbeddingModel` (its `speech` part changed),
/// and progress arrives as `Event::EmbeddingModelChanged`. 409 "no_speech_model" on a
/// service that transcribes with an installed program instead (`EmbeddingModel.speech`
/// is `None` there).
pub const SPEECH_MODEL: &str = "/api/embedding/speech";
pub const SPEECH_MODEL_DOWNLOAD: &str = "/api/embedding/speech/download";

const MB: u64 = 1024 * 1024;
/// Largest audio or video source.
pub const MAX_MEDIA_SOURCE_BYTES: u64 = 500 * MB;
/// Largest source of any other kind (text, code, PDF, image).
pub const MAX_OTHER_SOURCE_BYTES: u64 = 50 * MB;
/// Largest part of an upload in parts, and the size above which the app uploads in parts:
/// well under the 100 MB a Cloudflare-proxied request body may be.
pub const UPLOAD_PART_BYTES: u64 = 32 * MB;

/// The largest file of `kind` the service accepts as a source.
pub fn max_source_bytes(kind: crate::SourceKind) -> u64 {
    use crate::SourceKind::*;
    match kind {
        Audio | Video => MAX_MEDIA_SOURCE_BYTES,
        Text | Code | Pdf | Image => MAX_OTHER_SOURCE_BYTES,
    }
}

/// The largest source of any kind.
pub const MAX_ANY_SOURCE_BYTES: u64 = MAX_MEDIA_SOURCE_BYTES;

/// "500 MB": `max_source_bytes(kind)` as people read it.
pub fn max_source_size_text(kind: crate::SourceKind) -> String {
    format!("{} MB", max_source_bytes(kind) / MB)
}

/// The sentence that says how big sources can be.
pub fn source_limits_text() -> String {
    format!(
        "Audio and video files can be up to {} MB each; other files up to {} MB.",
        MAX_MEDIA_SOURCE_BYTES / MB,
        MAX_OTHER_SOURCE_BYTES / MB
    )
}

/// "talk.mov is too big: audio and video sources can be at most 500 MB." Said by the
/// service (413) and by the app before it uploads.
pub fn too_large_text(name: &str, kind: crate::SourceKind) -> String {
    use crate::SourceKind::*;
    let what = match kind {
        Audio | Video => "audio and video sources",
        Text | Code | Pdf | Image => "text, code, PDF and image sources",
    };
    format!(
        "{name} is too big: {what} can be at most {}.",
        max_source_size_text(kind)
    )
}

pub fn topic(id: &str) -> String {
    format!("/api/topics/{id}")
}
pub fn lesson(id: &str) -> String {
    format!("/api/lessons/{id}")
}
pub fn lesson_regenerate(id: &str) -> String {
    format!("/api/lessons/{id}/regenerate")
}
pub fn lesson_video(id: &str) -> String {
    format!("/api/lessons/{id}/video")
}
pub fn note(id: &str) -> String {
    format!("/api/notes/{id}")
}
pub fn proposal(id: &str) -> String {
    format!("/api/proposals/{id}")
}
pub fn proposal_accept(id: &str) -> String {
    format!("/api/proposals/{id}/accept")
}
pub fn video(id: &str) -> String {
    format!("/api/videos/{id}")
}
pub fn source(id: &str) -> String {
    format!("/api/sources/{id}")
}
/// Read the source again in place (same id and topic): for one that failed or was read
/// with a warning, e.g. once `fluidaudiocli` is installed.
pub fn source_reprocess(id: &str) -> String {
    format!("/api/sources/{id}/reprocess")
}
/// An audio or video source's whole transcript, line by line; empty when it has none.
pub fn source_transcript(id: &str) -> String {
    format!("/api/sources/{id}/transcript")
}
pub fn source_upload(id: &str) -> String {
    format!("/api/source-uploads/{id}")
}
pub fn source_upload_finish(id: &str) -> String {
    format!("/api/source-uploads/{id}/finish")
}
pub fn device(id: &str) -> String {
    format!("/api/devices/{id}")
}
pub fn chat(id: &str) -> String {
    format!("/api/chats/{id}")
}
/// Send a message. Starts a turn when the chat is idle, interrupted or failed; while a
/// turn is busy (running or waiting on a confirmation) the message becomes
/// `Chat.pending` (409 "already_pending" if there already is one).
pub fn chat_messages(id: &str) -> String {
    format!("/api/chats/{id}/messages")
}
/// Drop the pending message.
pub fn chat_pending(id: &str) -> String {
    format!("/api/chats/{id}/pending")
}
/// Stop the running turn (a parked confirmation counts as denied). 409 "not_running"
/// when no turn is busy.
pub fn chat_stop(id: &str) -> String {
    format!("/api/chats/{id}/stop")
}
/// Resume an interrupted or failed turn. 409 "cannot_continue" otherwise.
pub fn chat_continue(id: &str) -> String {
    format!("/api/chats/{id}/continue")
}
/// Answer the confirmation card the turn is waiting on. 409 "not_waiting" when that
/// card is no longer pending.
pub fn chat_confirm(id: &str) -> String {
    format!("/api/chats/{id}/confirm")
}
/// The full output of a `ToolResult` item.
pub fn chat_item_output(chat_id: &str, item_id: &str) -> String {
    format!("/api/chats/{chat_id}/items/{item_id}/output")
}
pub fn workspace(id: &str) -> String {
    format!("/api/workspaces/{id}")
}
/// The holding device sends its folder's manifest (`SyncManifest`) and gets back what to
/// upload (`SyncPlan`). Sources no longer listed, or whose stamp changed, are deleted;
/// the same manifest twice uploads nothing twice. 404; 403 "not_yours" unless the
/// caller holds the folder; 400 "bad_path" for a path `relative_components` refuses (a
/// path whose type isn't allowed is ignored).
pub fn workspace_sync(id: &str) -> String {
    format!("/api/workspaces/{id}/sync")
}
/// The folder's files as sources, newest first, with `media_url`. 404.
pub fn workspace_files(id: &str) -> String {
    format!("/api/workspaces/{id}/files")
}
/// `DELETE`: a document an agent added (`Origin::Agent`) and its passages; the change
/// feed lists it as deleted. 204; 404 unknown; 403 "forbidden" for any other document.
pub fn context_document(id: &str) -> String {
    format!("/api/context/documents/{id}")
}
/// A document's sections (`context::DocumentOutline`). 404 unknown document; 409
/// "not_ready" while it is first being indexed.
pub fn context_document_outline(id: &str) -> String {
    format!("/api/context/documents/{id}/outline")
}
/// A document's passages by position: `?from=<ord>&to=<ord>` (inclusive; at most
/// `TOOL_READ_PASSAGES_MAX` of them) → `Vec<context::Hit>`. 404; 409 "not_ready".
pub fn context_document_passages(id: &str) -> String {
    format!("/api/context/documents/{id}/passages")
}

// Retrieval constants (`docs/agent-rag-architecture.md` §2.2, §2.3, §2.8). Defaults the
// pipeline's stages start from; `PipelineConfig` params override them per variant.

/// Candidates the vector (HNSW) leg returns per query string.
pub const RETRIEVAL_K_DENSE: usize = 40;
/// Candidates the full-text (BM25) leg returns per query string.
pub const RETRIEVAL_K_LEX: usize = 40;
/// Candidates kept after fusing the legs.
pub const RETRIEVAL_K_FUSED: usize = 24;
/// The `k` of reciprocal rank fusion.
pub const RRF_K: u32 = 60;
/// Relevance vs diversity in the packer's MMR (1.0 = relevance only).
pub const MMR_LAMBDA: f32 = 0.7;
/// Most hits the agent's `search` tool returns.
pub const TOOL_SEARCH_K_MAX: usize = 12;
/// Longest passage text in a `search` tool output: `TOOL_SEARCH_K_MAX` of them with
/// their locators fit the 16 KB a tool output may be.
pub const TOOL_OUTPUT_PASSAGE_CHARS: usize = 1_100;
/// Most adjacent passages one `read_passage` returns.
pub const TOOL_READ_PASSAGES_MAX: usize = 4;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SourceKind;

    #[test]
    fn limits_depend_on_the_kind() {
        assert_eq!(max_source_bytes(SourceKind::Video), 500 * 1024 * 1024);
        assert_eq!(max_source_bytes(SourceKind::Audio), 500 * 1024 * 1024);
        for kind in [
            SourceKind::Text,
            SourceKind::Code,
            SourceKind::Pdf,
            SourceKind::Image,
        ] {
            assert_eq!(max_source_bytes(kind), 50 * 1024 * 1024);
        }
        const { assert!(UPLOAD_PART_BYTES < 100 * 1000 * 1000) };
        assert_eq!(max_source_size_text(SourceKind::Audio), "500 MB");
        assert_eq!(
            too_large_text("talk.mov", SourceKind::Video),
            "talk.mov is too big: audio and video sources can be at most 500 MB."
        );
        assert_eq!(
            too_large_text("scan.pdf", SourceKind::Pdf),
            "scan.pdf is too big: text, code, PDF and image sources can be at most 50 MB."
        );
        assert_eq!(
            source_limits_text(),
            "Audio and video files can be up to 500 MB each; other files up to 50 MB."
        );
    }

    #[test]
    fn workspace_routes() {
        assert_eq!(workspace_sync("w1"), "/api/workspaces/w1/sync");
        assert_eq!(workspace_files("w1"), "/api/workspaces/w1/files");
    }

    #[test]
    fn context_routes() {
        assert_eq!(
            context_document_outline("d1"),
            "/api/context/documents/d1/outline"
        );
        assert_eq!(
            context_document_passages("d1"),
            "/api/context/documents/d1/passages"
        );
        for path in [
            CONTEXT_SEARCH,
            CONTEXT_INGEST,
            CONTEXT_CHANGES,
            TRACES,
            TRACES_ROLLUP,
            PROJECTS,
        ] {
            assert!(path.starts_with("/api/"), "{path}");
        }
        const { assert!(RETRIEVAL_K_FUSED <= RETRIEVAL_K_DENSE + RETRIEVAL_K_LEX) };
        const { assert!(TOOL_SEARCH_K_MAX <= RETRIEVAL_K_FUSED) };
    }
}
