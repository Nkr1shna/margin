//! Replay on the Messages API (Claude), D4: a stored entry is
//! `{"role": "assistant"|"user", "block": {…one content block verbatim…}}`; replay
//! regroups the blocks into messages.
//!
//! The rendering is a pure function of the stored items: the same items always render
//! the same prefix (what preserved thinking checks), so nothing sent is ever edited.
//! Built by area P-agent.

use margin_shared::ChatItem;
use serde_json::Value;

use super::ReplayItem;

/// How an `Action` is stored on Claude: a user-role text block starting with this
/// (D6); the prompt tells the model such a note is the app's record, not the owner's
/// words.
pub const ACTION_NOTE_PREFIX: &str = "Margin: ";

/// The `messages` that continue the chat: items in `seq` order from the latest
/// `compaction` block (or condensed-history note), consecutive blocks of one role as
/// one message, the safety net for an unanswered `tool_use`, an `Action` note between
/// a step's `tool_use`s and their results rendered after the results (or deferred while
/// a server tool is pending), a trailing `server_tool_use` without its result and a
/// trailing lone `thinking` block left out.
pub fn messages(items: &[ReplayItem<'_>]) -> Vec<Value> {
    let _ = items;
    unimplemented!("providers, P-agent")
}

/// `tool_use` ids with no `tool_result`, and the item each belongs to.
pub fn unanswered_calls<'a>(items: &[ReplayItem<'a>]) -> Vec<(String, &'a ChatItem)> {
    let _ = items;
    unimplemented!("providers, P-agent")
}

/// The stored `tool_result` of a call cut short by Stop or a restart:
/// `{"role":"user","block":{"type":"tool_result","tool_use_id":…,"is_error":true,…}}`.
pub fn cancelled_call_output(call_id: &str) -> Value {
    let _ = call_id;
    unimplemented!("providers, P-agent")
}

/// The stored user-role note an `Action` is replayed as (`ACTION_NOTE_PREFIX` + text).
pub fn action_note(text: &str) -> Value {
    let _ = text;
    unimplemented!("providers, P-agent")
}
