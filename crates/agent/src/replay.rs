//! Rebuilding a chat's model input from its stored items.

use margin_shared::ChatItem;
use serde_json::{Value, json};

/// A stored item with the model items stored beside it (`ItemRecord.model_items`).
#[derive(Debug, Clone, Copy)]
pub struct ReplayItem<'a> {
    pub item: &'a ChatItem,
    pub model_items: &'a [Value],
}

/// The `input` that continues the chat: every item's `model_items` verbatim, items in
/// `seq` order (user messages with their context, model output whose
/// `output_item.done` arrived, function call outputs, `Action`s as developer messages
/// from [`action_input`]). Cards, errors and model output that never completed have no
/// model items. A `function_call` with no `function_call_output` anywhere gets
/// [`cancelled_call_output`] right after it, so the input is always valid.
///
/// The system prompt is not included (the loop puts it first).
pub fn replay_input(items: &[ReplayItem<'_>]) -> Vec<Value> {
    let _ = items;
    unimplemented!("margin-agent: replay_input (phase 1, agent owner)")
}

/// The `function_call` model items (as `(call_id, item)`) with no
/// `function_call_output`. The service uses it after Stop and at recovery to write a
/// `ToolResult` for each.
pub fn unanswered_calls<'a>(items: &[ReplayItem<'a>]) -> Vec<(String, &'a ChatItem)> {
    let _ = items;
    unimplemented!("margin-agent: unanswered_calls (phase 1, agent owner)")
}

/// The developer message an `Action` item is stored (and replayed) as:
/// `{"role":"developer","content":[{"type":"input_text","text":…}]}`.
pub fn action_input(text: &str) -> Value {
    json!({"role": "developer", "content": [{"type": "input_text", "text": text}]})
}

/// `{"type":"function_call_output","call_id":…,"output":…}`.
pub fn function_call_output(call_id: &str, output: &str) -> Value {
    json!({"type": "function_call_output", "call_id": call_id, "output": output})
}

/// The output for a call that never got one: `{"error":"cancelled by the user"}`.
pub fn cancelled_call_output(call_id: &str) -> Value {
    function_call_output(call_id, &crate::error_output("cancelled by the user"))
}
