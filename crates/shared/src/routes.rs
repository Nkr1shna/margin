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
//! | POST   | /api/ask                               | AskRequest         | Proposal (Drafting) |
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
//! | GET    | /api/events  (WebSocket upgrade)       |                    | Event frames        |
//! | GET    | /media/videos/{id}/{file}?exp&sig      |                    | video.mp4, poster.png, storyboard.png, captions.srt (range requests) |

pub const HEALTH: &str = "/health";
pub const AUTH_START: &str = "/auth/start";
pub const AUTH_POLL: &str = "/auth/poll";
pub const AUTH_GITHUB_CALLBACK: &str = "/auth/github/callback";
pub const ME: &str = "/api/me";
pub const LIBRARY: &str = "/api/library";
pub const TOPICS: &str = "/api/topics";
pub const NOTES: &str = "/api/notes";
pub const ASK: &str = "/api/ask";
pub const SEARCH: &str = "/api/search";
pub const SETTINGS: &str = "/api/settings";
pub const TOOLS: &str = "/api/tools";
pub const DEVICES: &str = "/api/devices";
pub const EVENTS: &str = "/api/events";

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
pub fn device(id: &str) -> String {
    format!("/api/devices/{id}")
}
