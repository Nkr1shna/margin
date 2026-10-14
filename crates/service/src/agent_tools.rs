//! The service's tools for the agent: the library, courses, sources and jobs, run
//! in-process through the same functions the HTTP handlers use (same validation,
//! `write_lock`, events). Device tools come from `margin_agent::device_tools`.
//!
//! By phase (`docs/agent-contract.md`):
//! - 1, Read: `search_library`, `list_topics`, `get_topic`, `read_lesson`, `read_note`,
//!   `list_notes`, `search_sources` (passages card), `list_sources`.
//! - 2, Write: `create_topic`, `rename_topic`, `move_topic`, `edit_lesson_title`,
//!   `create_note`, `append_to_note`, `replace_note_body` (confirms when the note is the
//!   owner's own writing), `propose_course` (outline card), `accept_proposal` (result
//!   card); Destructive: `delete_topic`, `delete_lesson`, `delete_note`.
//! - 3, Write: `write_lesson`, `regenerate_lesson` (confirms when the lesson body was
//!   edited by the owner), `make_video` (result cards), `attach_sources`.

use margin_agent::ToolRegistry;

use crate::state::AppState;

/// Every tool, service tools first then `margin_agent::device_tools()` (from phase 3),
/// in a fixed order so the `tools` array is identical on every call.
pub fn registry(state: &AppState) -> ToolRegistry {
    let _ = state;
    unimplemented!("service: agent_tools::registry (phase 1, service owner)")
}
