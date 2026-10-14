//! HTTP routes, relative to the service base URL (e.g. `https://margin.krishnanelloore.com`).
//!
//! Everything under `/api` requires `Authorization: Bearer <device token>`, except the
//! media routes, which are authorized by the signed query string the service puts in
//! `VideoMedia` URLs. Errors use `ApiError` as the body.
//!
//! | Method | Path                                   | Body               | Response            |
//! |--------|----------------------------------------|--------------------|---------------------|
//! | GET    | /health                                |                    | "ok"                |
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
//! | GET    | /api/tools                             |                    | Vec<ToolStatus>     |
//! | GET    | /api/devices                           |                    | Vec<Device>         |
//! | DELETE | /api/devices/{id}                      |                    | 204                 |
//! | POST   | /api/sources?name&folder&topic_id      | file bytes         | Source (Queued)     |
//! | DELETE | /api/sources/{id}                      |                    | 204 (chunks too)    |
//! | POST   | /api/sources/{id}/reprocess            |                    | Source (Queued)     |
//! | GET    | /api/sources/{id}/transcript           |                    | Vec<TranscriptSegment> |
//! | POST   | /api/source-uploads?name&folder&topic_id&bytes |            | SourceUpload        |
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
//! | DELETE | /api/workspaces/{id}                   |                    | 204                 |
//! | GET    | /api/events  (WebSocket upgrade)       | DeviceFrame frames | ServerFrame frames  |
//! | GET    | /media/videos/{id}/{file}?exp&sig      |                    | video.mp4, poster.png, storyboard.png, captions.srt (range requests) |
//! | GET    | /media/sources/{id}/{file}?exp&sig     |                    | original, page-{n}.png, frame-{ms}.jpg (range requests) |

pub const HEALTH: &str = "/health";
pub const AUTH_START: &str = "/auth/start";
pub const AUTH_POLL: &str = "/auth/poll";
pub const AUTH_GITHUB_CALLBACK: &str = "/auth/github/callback";
pub const ME: &str = "/api/me";
pub const LIBRARY: &str = "/api/library";
pub const TOPICS: &str = "/api/topics";
pub const NOTES: &str = "/api/notes";
pub const SEARCH: &str = "/api/search";
pub const SETTINGS: &str = "/api/settings";
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
pub const SOURCES: &str = "/api/sources";
/// A source uploaded in parts. `POST ?name&folder&topic_id&bytes` (the query of
/// `SOURCES`, plus the file's size) checks the name and size and starts it; each
/// `PUT source_upload(id)?offset=<received>` appends a part of at most
/// `UPLOAD_PART_BYTES` (a part at any other offset is refused with 409 "wrong_offset",
/// and the body says where to go on); `POST source_upload_finish(id)` once all of it is
/// there turns it into a Queued `Source`. Unfinished uploads are dropped after a day.
pub const SOURCE_UPLOADS: &str = "/api/source-uploads";
pub const EMBEDDING: &str = "/api/embedding";
pub const EMBEDDING_DOWNLOAD: &str = "/api/embedding/download";

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
}
