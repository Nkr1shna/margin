//! Rebuilding a chat's model input from its stored items, per provider
//! (`docs/agent-providers-brief.md` D4). A chat stays on one provider, and its
//! `model_items` hold that provider's wire items verbatim: Responses items for OpenAI
//! and ChatGPT ([`openai`]), Messages content blocks with their role for Claude
//! ([`claude`]). Nothing stored is ever rewritten.
//!
//! The functions at this level take the chat's provider and dispatch: the service
//! stores and replays through them, so a chat on one provider never gets another's
//! shapes. (The unqualified re-exports from [`openai`] are the Responses shapes, kept
//! for callers that only know that provider.)

pub mod claude;
pub mod openai;

use margin_shared::{ChatItem, Provider};
use serde_json::Value;

use crate::MessageContext;

pub use openai::{
    CONDENSED_HISTORY_PREFIX, ReplayItem, TOOL_PICTURES_PREFIX, action_input,
    cancelled_call_output, function_call_output, replay_input, tool_pictures_input,
    unanswered_calls,
};
pub(crate) use openai::{
    PICTURE_REMOVED, compaction_start, condensed_history_input, is_condensed_history, item_type,
    tool_pictures_call_id,
};

/// Whether `provider` speaks the Messages API (stored entries are `{role, block}`).
pub fn is_messages(provider: Provider) -> bool {
    provider == Provider::Claude
}

/// The history a turn starts from (`TurnInput.history`): on OpenAI and ChatGPT the
/// Responses `input` ([`replay_input`]); on Claude the stored entries from the latest
/// compaction on ([`claude::entries`]), which the loop renders into `messages` for each
/// step ([`claude::render`]).
pub fn history(provider: Provider, items: &[ReplayItem<'_>]) -> Vec<Value> {
    if is_messages(provider) {
        claude::entries(items)
    } else {
        replay_input(items)
    }
}

/// The calls the model made that have no result yet (`function_call`s without a
/// `function_call_output`; Claude `tool_use`s without a `tool_result`). Never the
/// provider's own web search or fetch calls.
pub fn unanswered(provider: Provider, items: &[ReplayItem<'_>]) -> Vec<(String, ChatItem)> {
    let calls = if is_messages(provider) {
        claude::unanswered_calls(items)
    } else {
        unanswered_calls(items)
    };
    calls
        .into_iter()
        .map(|(id, item)| (id, item.clone()))
        .collect()
}

/// The stored result of a call: a `function_call_output`, or a Claude `tool_result`
/// (`is_error: !ok`).
pub fn call_output(provider: Provider, call_id: &str, output: &str, ok: bool) -> Value {
    if is_messages(provider) {
        claude::tool_result(call_id, output, ok, &[])
    } else {
        function_call_output(call_id, output)
    }
}

/// The stored result of a call cut short by Stop or a restart:
/// `{"error":"cancelled by the user"}` in the provider's shape.
pub fn cancelled_output(provider: Provider, call_id: &str) -> Value {
    if is_messages(provider) {
        claude::cancelled_call_output(call_id)
    } else {
        cancelled_call_output(call_id)
    }
}

/// The output text of a stored call result made by [`call_output`] or
/// [`cancelled_output`] (either shape).
pub fn output_text(value: &Value) -> String {
    match value["output"].as_str() {
        Some(output) => output.to_string(),
        None => claude::result_text(claude::block(value)),
    }
}

/// How an `Action` is stored and replayed: a developer message on OpenAI, a user-role
/// note starting with "Margin: " on Claude (D6).
pub fn action(provider: Provider, text: &str) -> Value {
    if is_messages(provider) {
        claude::action_note(text)
    } else {
        action_input(text)
    }
}

/// What a user message is stored and replayed as: the context Margin adds, then the
/// owner's words (one user message with two `input_text` parts on OpenAI, two user
/// `text` blocks on Claude). Built once when the message is stored.
pub fn user_message(provider: Provider, text: &str, ctx: &MessageContext) -> Vec<Value> {
    if is_messages(provider) {
        claude::user_message(&crate::prompt::context_text(ctx), text)
    } else {
        crate::user_message_input(text, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shapes_follow_the_provider() {
        for p in [Provider::OpenAi, Provider::ChatGpt] {
            assert_eq!(
                call_output(p, "c", "ok", true),
                function_call_output("c", "ok")
            );
            assert_eq!(cancelled_output(p, "c"), cancelled_call_output("c"));
            assert_eq!(action(p, "Stopped."), action_input("Stopped."));
        }
        let c = Provider::Claude;
        assert_eq!(
            call_output(c, "c", "bad", false),
            json!({"role": "user", "block": {"type": "tool_result", "tool_use_id": "c", "content": "bad", "is_error": true}})
        );
        assert_eq!(
            action(c, "Stopped."),
            json!({"role": "user", "block": {"type": "text", "text": "Margin: Stopped."}})
        );
        assert_eq!(
            output_text(&cancelled_output(c, "c")),
            r#"{"error":"cancelled by the user"}"#
        );
        assert_eq!(
            output_text(&cancelled_output(Provider::OpenAi, "c")),
            r#"{"error":"cancelled by the user"}"#
        );
    }
}
