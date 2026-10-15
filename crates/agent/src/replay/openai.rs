//! Replay on the Responses API (OpenAI, ChatGPT): a chat's stored items, verbatim, as `input`.

use std::collections::HashSet;

use margin_shared::ChatItem;
use serde_json::{Value, json};

/// A stored item with the model items stored beside it (`ItemRecord.model_items`).
#[derive(Debug, Clone, Copy)]
pub struct ReplayItem<'a> {
    pub item: &'a ChatItem,
    pub model_items: &'a [Value],
}

/// How the developer message that replaces condensed history starts (the fallback for
/// base URLs without server-side compaction). An `Action` item whose first model item
/// is such a message holds the whole condensed history; replay starts there.
pub const CONDENSED_HISTORY_PREFIX: &str =
    "The earlier part of this chat was condensed to fit the context window. Summary:";

/// The input that continues the chat: every item's `model_items` verbatim, items in
/// `seq` order (user messages with their context, model output whose
/// `output_item.done` arrived, function call outputs, `Action`s as developer messages
/// from [`action_input`]). Cards, errors and model output that never completed have no
/// model items. A `function_call` with no `function_call_output` anywhere gets
/// [`cancelled_call_output`] right after it, so the input is always valid. A developer
/// or user message stored between a step's calls and their outputs (an `Action`
/// recorded while the turn waited on a confirmation, or a call's pictures from
/// [`tool_pictures_input`] stored with its output) is replayed after those outputs;
/// the step's own output (more calls, reasoning, a message) keeps its order.
///
/// Condensed history: the input starts at the latest `compaction` item (server-side
/// compaction carries everything before it) or the latest condensed-history `Action`
/// (see [`CONDENSED_HISTORY_PREFIX`]); what came before is not sent. A `reasoning` item
/// that isn't followed by the output it led to (a message or function call), as when a
/// response failed right after it, is left out: the API rejects it alone.
///
/// The system prompt is not included (the loop puts it first).
pub fn replay_input(items: &[ReplayItem<'_>]) -> Vec<Value> {
    let flat = from_last_compaction(items);
    let answered: HashSet<&str> = flat
        .iter()
        .filter(|v| item_type(v) == Some("function_call_output"))
        .filter_map(|v| v["call_id"].as_str())
        .collect();
    let mut seen_calls = HashSet::new();
    let mut out = Vec::with_capacity(flat.len());
    // Calls replayed whose output is still to come, and the input messages stored
    // meanwhile (an `Action` recorded while the turn waited on a confirmation): they go
    // after the outputs, so no input comes between a step's calls and their outputs.
    let mut open: HashSet<&str> = HashSet::new();
    let mut deferred: Vec<Value> = Vec::new();
    for (i, value) in flat.iter().enumerate() {
        match item_type(value) {
            Some("reasoning") if !leads_to_output(&flat[i + 1..]) => continue,
            Some("function_call") => {
                let call_id = value["call_id"].as_str().unwrap_or_default();
                if !seen_calls.insert(call_id) {
                    continue;
                }
                out.push((*value).clone());
                if answered.contains(call_id) {
                    open.insert(call_id);
                } else {
                    out.push(cancelled_call_output(call_id));
                }
            }
            Some("function_call_output") => {
                out.push((*value).clone());
                open.remove(value["call_id"].as_str().unwrap_or_default());
                if open.is_empty() {
                    out.append(&mut deferred);
                }
            }
            _ if !open.is_empty() && is_input_message(value) => deferred.push((*value).clone()),
            _ => out.push((*value).clone()),
        }
    }
    out.append(&mut deferred);
    out
}

/// The `function_call` model items (as `(call_id, item)`) with no
/// `function_call_output`. The service uses it after Stop and at recovery to write a
/// `ToolResult` for each.
pub fn unanswered_calls<'a>(items: &[ReplayItem<'a>]) -> Vec<(String, &'a ChatItem)> {
    let answered: HashSet<&str> = items
        .iter()
        .flat_map(|r| r.model_items.iter())
        .filter(|v| item_type(v) == Some("function_call_output"))
        .filter_map(|v| v["call_id"].as_str())
        .collect();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for r in sorted(items) {
        for value in r.model_items {
            if item_type(value) != Some("function_call") {
                continue;
            }
            let call_id = value["call_id"].as_str().unwrap_or_default();
            if !answered.contains(call_id) && seen.insert(call_id) {
                out.push((call_id.to_string(), r.item));
            }
        }
    }
    out
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

/// How the user message carrying a tool call's pictures starts; the call_id follows.
pub const TOOL_PICTURES_PREFIX: &str = "Pictures from the tool call ";
/// What stands in for a picture condensing took out.
pub(crate) const PICTURE_REMOVED: &str = "[picture removed]";

/// The input item that carries a tool call's pictures ([`ToolOutput::images`]), stored
/// right after its `function_call_output` in the same `ItemRecord.model_items`:
/// `{"role":"user","content":[{"type":"input_text","text":"Pictures from the tool call
/// {call_id} ({summary}), in order."},{"type":"input_image","image_url":"data:…",
/// "detail":"auto"},…]}`. `None` without pictures. Replay puts it after the step's
/// outputs, like any input message stored between a step's calls and their outputs.
///
/// [`ToolOutput::images`]: crate::ToolOutput::images
pub fn tool_pictures_input(
    call_id: &str,
    summary: &str,
    images: &[margin_ai::Image],
) -> Option<Value> {
    if images.is_empty() {
        return None;
    }
    let mut content = vec![json!({
        "type": "input_text",
        "text": format!("{TOOL_PICTURES_PREFIX}{call_id} ({summary}), in order."),
    })];
    content.extend(images.iter().map(
        |image| json!({"type": "input_image", "image_url": image.data_url(), "detail": "auto"}),
    ));
    Some(json!({"role": "user", "content": content}))
}

/// The call_id of a message made by [`tool_pictures_input`]; `None` for anything else.
/// (An owner's message never matches: it starts with the context part Margin adds.)
pub(crate) fn tool_pictures_call_id(value: &Value) -> Option<&str> {
    if value["role"] != "user" {
        return None;
    }
    let rest = value["content"][0]["text"]
        .as_str()?
        .strip_prefix(TOOL_PICTURES_PREFIX)?;
    rest.split(' ').next().filter(|id| !id.is_empty())
}

/// The output for a call that never got one: `{"error":"cancelled by the user"}`.
pub fn cancelled_call_output(call_id: &str) -> Value {
    function_call_output(call_id, &crate::error_output("cancelled by the user"))
}

/// The developer message that starts a condensed history.
pub(crate) fn condensed_history_input(summary: &str) -> Value {
    action_input(&format!("{CONDENSED_HISTORY_PREFIX}\n\n{summary}"))
}

/// Whether `value` is a developer message made by [`condensed_history_input`].
pub(crate) fn is_condensed_history(value: &Value) -> bool {
    value["role"] == "developer"
        && value["content"][0]["text"]
            .as_str()
            .is_some_and(|t| t.starts_with(CONDENSED_HISTORY_PREFIX))
}

/// The index in `history` the model input starts at: the latest `compaction` item or
/// condensed-history message, else 0.
pub(crate) fn compaction_start(history: &[Value]) -> usize {
    history
        .iter()
        .rposition(|v| item_type(v) == Some("compaction") || is_condensed_history(v))
        .unwrap_or(0)
}

pub(crate) fn item_type(value: &Value) -> Option<&str> {
    value["type"].as_str()
}

fn sorted<'s, 'a>(items: &'s [ReplayItem<'a>]) -> Vec<&'s ReplayItem<'a>> {
    let mut sorted: Vec<_> = items.iter().collect();
    sorted.sort_by_key(|r| r.item.seq);
    sorted
}

fn from_last_compaction<'a>(items: &[ReplayItem<'a>]) -> Vec<&'a Value> {
    let flat: Vec<&'a Value> = sorted(items)
        .into_iter()
        .flat_map(|r| r.model_items.iter())
        .collect();
    let start = flat
        .iter()
        .rposition(|v| item_type(v) == Some("compaction") || is_condensed_history(v))
        .unwrap_or(0);
    flat[start..].to_vec()
}

/// Whether `value` is a developer or user message (input, not model output).
fn is_input_message(value: &Value) -> bool {
    matches!(value["role"].as_str(), Some("developer" | "user"))
}

/// Whether the items after a reasoning item continue the same output: more reasoning,
/// then an assistant message or a function call.
fn leads_to_output(rest: &[&Value]) -> bool {
    for value in rest {
        match item_type(value) {
            Some("reasoning") => continue,
            Some("function_call") => return true,
            Some("message") => return value["role"] == "assistant",
            _ => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scripted::{function_call_item, message_item, reasoning_item};
    use chrono::{DateTime, Utc};
    use margin_shared::{ChatItemKind, ItemStatus};

    fn at() -> DateTime<Utc> {
        DateTime::from_timestamp(1_760_000_000, 0).unwrap()
    }

    fn item(seq: u64) -> ChatItem {
        ChatItem {
            id: format!("i{seq}"),
            chat_id: "c".into(),
            turn_id: "t".into(),
            seq,
            status: ItemStatus::Done,
            created_at: at(),
            updated_at: at(),
            kind: ChatItemKind::Action {
                text: String::new(),
            },
        }
    }

    fn user(text: &str) -> Value {
        json!({"role": "user", "content": [{"type": "input_text", "text": text}]})
    }

    #[test]
    fn replays_model_items_in_seq_order() {
        let items: Vec<ChatItem> = (1..=5).map(item).collect();
        let stored = [
            vec![user("hi")],
            vec![reasoning_item("rs_1", "thinking")],
            vec![function_call_item("fc_1", "call_1", "list_topics", "{}")],
            vec![function_call_output("call_1", "[]")],
            vec![message_item("msg_1", "None yet.")],
        ];
        let mut replay: Vec<ReplayItem> = items
            .iter()
            .zip(&stored)
            .map(|(item, m)| ReplayItem {
                item,
                model_items: m,
            })
            .collect();
        replay.reverse(); // the order given doesn't matter, `seq` does
        let input = replay_input(&replay);
        let flat: Vec<Value> = stored.concat();
        assert_eq!(input, flat);
        assert!(unanswered_calls(&replay).is_empty());
    }

    #[test]
    fn unanswered_calls_get_a_cancelled_output() {
        let items: Vec<ChatItem> = (1..=4).map(item).collect();
        let stored = [
            vec![user("go")],
            vec![function_call_item("fc_1", "call_1", "a", "{}")],
            vec![function_call_item("fc_2", "call_2", "b", "{}")],
            vec![function_call_output("call_2", "ok")],
        ];
        let replay: Vec<ReplayItem> = items
            .iter()
            .zip(&stored)
            .map(|(item, m)| ReplayItem {
                item,
                model_items: m,
            })
            .collect();
        let input = replay_input(&replay);
        assert_eq!(input.len(), 5);
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[2], cancelled_call_output("call_1"));
        assert_eq!(input[2]["output"], r#"{"error":"cancelled by the user"}"#);
        let unanswered = unanswered_calls(&replay);
        assert_eq!(unanswered.len(), 1);
        assert_eq!(unanswered[0].0, "call_1");
        assert_eq!(unanswered[0].1.id, "i2");
    }

    #[test]
    fn an_action_between_calls_and_outputs_follows_the_outputs() {
        let items: Vec<ChatItem> = (1..=7).map(item).collect();
        let created = action_input("Created the course “Optics” with 3 lessons.");
        let stored = [
            vec![user("tidy up")],
            vec![function_call_item("fc_1", "call_1", "delete_note", "{}")],
            vec![function_call_item("fc_2", "call_2", "list_topics", "{}")],
            vec![created.clone()],
            vec![function_call_output("call_1", "ok")],
            vec![function_call_output("call_2", "[]")],
            vec![message_item("msg_1", "Done.")],
        ];
        let replay: Vec<ReplayItem> = items
            .iter()
            .zip(&stored)
            .map(|(item, m)| ReplayItem {
                item,
                model_items: m,
            })
            .collect();
        let input = replay_input(&replay);
        assert_eq!(
            input,
            vec![
                user("tidy up"),
                function_call_item("fc_1", "call_1", "delete_note", "{}"),
                function_call_item("fc_2", "call_2", "list_topics", "{}"),
                function_call_output("call_1", "ok"),
                function_call_output("call_2", "[]"),
                created,
                message_item("msg_1", "Done."),
            ]
        );
    }

    #[test]
    fn a_steps_own_output_between_its_calls_keeps_its_order() {
        let cases = [
            vec![
                vec![user("go")],
                vec![function_call_item("fc_1", "call_1", "a", "{}")],
                vec![reasoning_item("rs_2", "next")],
                vec![function_call_item("fc_2", "call_2", "b", "{}")],
                vec![function_call_output("call_1", "ok")],
                vec![function_call_output("call_2", "ok")],
            ],
            vec![
                vec![user("go")],
                vec![function_call_item("fc_1", "call_1", "a", "{}")],
                vec![message_item("msg_1", "Looking.")],
                vec![function_call_output("call_1", "ok")],
            ],
        ];
        for stored in cases {
            let items: Vec<ChatItem> = (1..=stored.len() as u64).map(item).collect();
            let replay: Vec<ReplayItem> = items
                .iter()
                .zip(&stored)
                .map(|(item, m)| ReplayItem {
                    item,
                    model_items: m,
                })
                .collect();
            assert_eq!(replay_input(&replay), stored.concat());
        }
    }

    #[test]
    fn a_lone_reasoning_item_is_left_out() {
        let items: Vec<ChatItem> = (1..=3).map(item).collect();
        let stored = [
            vec![user("go")],
            vec![reasoning_item("rs_1", "hm")],
            vec![action_input("Stopped by the owner.")],
        ];
        let replay: Vec<ReplayItem> = items
            .iter()
            .zip(&stored)
            .map(|(item, m)| ReplayItem {
                item,
                model_items: m,
            })
            .collect();
        let input = replay_input(&replay);
        assert_eq!(
            input,
            vec![user("go"), action_input("Stopped by the owner.")]
        );
    }

    #[test]
    fn replay_starts_at_the_latest_compaction() {
        let items: Vec<ChatItem> = (1..=6).map(item).collect();
        let compaction = json!({"type": "compaction", "id": "cmp_1", "encrypted_content": "x"});
        let stored = [
            vec![user("old")],
            vec![message_item("msg_1", "old answer")],
            vec![compaction.clone()],
            vec![message_item("msg_2", "after")],
            vec![condensed_history_input("they talked"), user("kept")],
            vec![message_item("msg_3", "latest")],
        ];
        let replay: Vec<ReplayItem> = items
            .iter()
            .zip(&stored)
            .map(|(item, m)| ReplayItem {
                item,
                model_items: m,
            })
            .collect();
        let input = replay_input(&replay);
        assert_eq!(input.len(), 3);
        assert!(is_condensed_history(&input[0]));
        assert_eq!(input[1], user("kept"));
        // Without the condensed item, the server compaction is the start.
        let input = replay_input(&replay[..4]);
        assert_eq!(input, vec![compaction, message_item("msg_2", "after")]);
    }
}
