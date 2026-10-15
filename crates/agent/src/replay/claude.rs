//! Replay on the Messages API (Claude), D4: a stored entry is
//! `{"role": "assistant"|"user", "block": {…one content block verbatim…}}`; replay
//! regroups the blocks into messages.
//!
//! The rendering is a pure function of the stored items: the same items always render
//! the same prefix (what preserved thinking checks), so nothing sent is ever edited.
//!
//! What stores which (D4): a user message is two user `text` blocks (the context Margin
//! adds, then the owner's words); model output is one assistant block per stored entry
//! (`thinking`/`redacted_thinking` with their signatures, `text` with its citations,
//! `tool_use`, `server_tool_use`, `web_search_tool_result`/`web_fetch_tool_result`,
//! `compaction`), verbatim; a tool result is a user `tool_result` block (its pictures as
//! `image` blocks inside it); an `Action` is a user `text` block starting with
//! [`ACTION_NOTE_PREFIX`].

use std::collections::{HashMap, HashSet};

use margin_shared::ChatItem;
use serde_json::{Value, json};

use super::ReplayItem;
use super::openai::CONDENSED_HISTORY_PREFIX;

/// How an `Action` is stored on Claude: a user-role text block starting with this
/// (D6); the prompt tells the model such a note is the app's record, not the owner's
/// words.
pub const ACTION_NOTE_PREFIX: &str = "Margin: ";

/// A stored entry: one content block with its role.
pub fn entry(role: &str, block: Value) -> Value {
    json!({"role": role, "block": block})
}

/// An entry's role ("assistant" when a bare block was stored).
pub fn role(entry: &Value) -> &str {
    entry["role"].as_str().unwrap_or("assistant")
}

/// An entry's content block (the entry itself when a bare block was stored).
pub fn block(entry: &Value) -> &Value {
    match entry.get("block") {
        Some(block) => block,
        None => entry,
    }
}

/// An entry's block type.
pub fn block_type(entry: &Value) -> Option<&str> {
    block(entry)["type"].as_str()
}

/// The `messages` that continue the chat: items in `seq` order from the latest
/// `compaction` block (or condensed-history note), consecutive blocks of one role as
/// one message, the safety net for an unanswered `tool_use`, an `Action` note between
/// a step's `tool_use`s and their results rendered after the results, a trailing
/// `server_tool_use` without its result and a trailing lone `thinking` block left out.
/// See [`render`].
pub fn messages(items: &[ReplayItem<'_>]) -> Vec<Value> {
    render(&entries(items), false)
}

/// The stored entries of `items` in `seq` order, from the latest `compaction` block or
/// condensed-history note on: the loop's history on Claude (it renders them with
/// [`render`] for each step and appends what each step stores).
pub fn entries(items: &[ReplayItem<'_>]) -> Vec<Value> {
    let mut sorted: Vec<&ReplayItem<'_>> = items.iter().collect();
    sorted.sort_by_key(|r| r.item.seq);
    let flat: Vec<Value> = sorted
        .into_iter()
        .flat_map(|r| r.model_items.iter().cloned())
        .collect();
    let start = compaction_start(&flat);
    flat[start..].to_vec()
}

/// The index the model input starts at: the latest `compaction` block or
/// condensed-history note, else 0.
pub fn compaction_start(entries: &[Value]) -> usize {
    entries
        .iter()
        .rposition(|e| block_type(e) == Some("compaction") || is_condensed_history(e))
        .unwrap_or(0)
}

/// Renders stored entries as Messages API `messages`. Pure and deterministic:
///
/// - starts at the latest `compaction` block or condensed-history note;
/// - consecutive blocks of one role are one message (adjacent same-role messages merge);
/// - in a user message the `tool_result` blocks come first, then anything else stored
///   meanwhile (an `Action` note recorded while the turn waited on a confirmation);
/// - a `tool_use` with no `tool_result` anywhere gets a `tool_result { is_error: true,
///   content: {"error":"cancelled by the user"} }` at the start of the next user message
///   (one is added when none follows); a repeated `tool_use` id is sent once;
/// - a `server_tool_use` without its result is sent only while it is pending: in the
///   last assistant message with nothing after it but tool results (the server runs it
///   in the next call), or, with `resume` (after `pause_turn`), at the very end;
///   otherwise (a turn stopped while it ran) it is left out;
/// - `thinking`/`redacted_thinking` blocks with nothing after them in their message are
///   left out (a response that failed right after them), except in the last message
///   when `resume`.
pub fn render(entries: &[Value], resume: bool) -> Vec<Value> {
    let es = &entries[compaction_start(entries)..];
    let answered: HashSet<&str> = es
        .iter()
        .filter(|e| role(e) == "user" && block_type(e) == Some("tool_result"))
        .filter_map(|e| block(e)["tool_use_id"].as_str())
        .collect();
    let resolved: HashSet<&str> = es
        .iter()
        .filter(|e| is_server_result(e))
        .filter_map(|e| block(e)["tool_use_id"].as_str())
        .collect();
    let last_assistant = es.iter().rposition(|e| role(e) == "assistant");
    // The start of the last assistant message, and whether only tool results follow it.
    let (last_run_start, pending_ok) = match last_assistant {
        Some(last) => {
            let start = es[..last]
                .iter()
                .rposition(|e| role(e) != "assistant")
                .map_or(0, |i| i + 1);
            let after = &es[last + 1..];
            let only_results = !after.is_empty()
                && after
                    .iter()
                    .all(|e| role(e) == "user" && block_type(e) == Some("tool_result"));
            (start, only_results || (resume && after.is_empty()))
        }
        None => (es.len(), false),
    };

    // Runs of one role, filtered.
    let mut runs: Vec<(String, Vec<Value>)> = Vec::new();
    let mut seen_calls: HashSet<&str> = HashSet::new();
    let mut seen_results: HashSet<&str> = HashSet::new();
    for (i, e) in es.iter().enumerate() {
        let b = block(e);
        match block_type(e) {
            Some("tool_use") => {
                let id = b["id"].as_str().unwrap_or_default();
                if !seen_calls.insert(id) {
                    continue;
                }
            }
            Some("server_tool_use") => {
                let id = b["id"].as_str().unwrap_or_default();
                let pending = i >= last_run_start && pending_ok;
                if !resolved.contains(id) && !pending {
                    continue;
                }
            }
            Some("tool_result") => {
                let id = b["tool_use_id"].as_str().unwrap_or_default();
                if !seen_results.insert(id) {
                    continue;
                }
            }
            _ => {}
        }
        let r = role(e);
        match runs.last_mut() {
            Some((role, blocks)) if role == r => blocks.push(b.clone()),
            _ => runs.push((r.to_string(), vec![b.clone()])),
        }
    }

    // Thinking that led to nothing, and the messages left empty by it.
    let last = runs.len().saturating_sub(1);
    for (i, (role, blocks)) in runs.iter_mut().enumerate() {
        if role != "assistant" || (resume && i == last) {
            continue;
        }
        while blocks.last().is_some_and(is_thinking) {
            blocks.pop();
        }
    }
    let mut merged: Vec<(String, Vec<Value>)> = Vec::new();
    for (role, blocks) in runs {
        if blocks.is_empty() {
            continue;
        }
        match merged.last_mut() {
            Some((r, b)) if *r == role => b.extend(blocks),
            _ => merged.push((role, blocks)),
        }
    }

    // The safety net, and tool results first in each user message.
    let mut out: Vec<Value> = Vec::with_capacity(merged.len() + 1);
    let mut owed: Vec<Value> = Vec::new();
    for (role, blocks) in merged {
        if role == "assistant" {
            if !owed.is_empty() {
                out.push(json!({"role": "user", "content": std::mem::take(&mut owed)}));
            }
            owed = blocks
                .iter()
                .filter(|b| b["type"] == "tool_use")
                .filter_map(|b| b["id"].as_str())
                .filter(|id| !answered.contains(id))
                .map(|id| block(&cancelled_call_output(id)).clone())
                .collect();
            out.push(json!({"role": "assistant", "content": blocks}));
        } else {
            let (results, rest): (Vec<Value>, Vec<Value>) =
                blocks.into_iter().partition(|b| b["type"] == "tool_result");
            let mut content = std::mem::take(&mut owed);
            content.extend(results);
            content.extend(rest);
            out.push(json!({"role": "user", "content": content}));
        }
    }
    if !owed.is_empty() {
        out.push(json!({"role": "user", "content": owed}));
    }
    out
}

/// `tool_use` ids with no `tool_result`, and the item each belongs to. A web search or
/// fetch (`server_tool_use`) is the provider's, never in here.
pub fn unanswered_calls<'a>(items: &[ReplayItem<'a>]) -> Vec<(String, &'a ChatItem)> {
    let answered: HashSet<&str> = items
        .iter()
        .flat_map(|r| r.model_items.iter())
        .filter(|e| role(e) == "user" && block_type(e) == Some("tool_result"))
        .filter_map(|e| block(e)["tool_use_id"].as_str())
        .collect();
    let mut sorted: Vec<&ReplayItem<'a>> = items.iter().collect();
    sorted.sort_by_key(|r| r.item.seq);
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for r in sorted {
        for e in r.model_items {
            if block_type(e) != Some("tool_use") {
                continue;
            }
            let id = block(e)["id"].as_str().unwrap_or_default();
            if !answered.contains(id) && seen.insert(id) {
                out.push((id.to_string(), r.item));
            }
        }
    }
    out
}

/// The stored `tool_result` of a call: `{"role":"user","block":{"type":"tool_result",
/// "tool_use_id":…,"content":…,"is_error":…}}`. `content` is `output` alone, or, with
/// pictures, a `text` block with `output` followed by one base64 `image` block each.
pub fn tool_result(call_id: &str, output: &str, ok: bool, images: &[margin_ai::Image]) -> Value {
    let content = if images.is_empty() {
        Value::String(output.to_string())
    } else {
        let mut parts = vec![json!({"type": "text", "text": output})];
        parts.extend(images.iter().map(image_block));
        Value::Array(parts)
    };
    entry(
        "user",
        json!({"type": "tool_result", "tool_use_id": call_id, "content": content, "is_error": !ok}),
    )
}

/// A picture as an `image` content block.
pub fn image_block(image: &margin_ai::Image) -> Value {
    let url = image.data_url();
    let data = url.split_once(',').map_or("", |(_, data)| data);
    json!({"type": "image", "source": {"type": "base64", "media_type": image.media_type, "data": data}})
}

/// The stored `tool_result` of a call cut short by Stop or a restart:
/// `{"role":"user","block":{"type":"tool_result","tool_use_id":…,"is_error":true,…}}`.
pub fn cancelled_call_output(call_id: &str) -> Value {
    tool_result(
        call_id,
        &crate::error_output("cancelled by the user"),
        false,
        &[],
    )
}

/// The stored user-role note an `Action` is replayed as (`ACTION_NOTE_PREFIX` + text).
pub fn action_note(text: &str) -> Value {
    entry(
        "user",
        json!({"type": "text", "text": format!("{ACTION_NOTE_PREFIX}{text}")}),
    )
}

/// The entries a user message is stored as: the context Margin adds, then the owner's
/// words, as two user `text` blocks.
pub fn user_message(context: &str, text: &str) -> Vec<Value> {
    vec![
        entry("user", json!({"type": "text", "text": context})),
        entry("user", json!({"type": "text", "text": text})),
    ]
}

/// The note that starts a condensed history (an `Action` note whose text starts with
/// [`CONDENSED_HISTORY_PREFIX`]).
pub fn condensed_history(summary: &str) -> Value {
    action_note(&format!("{CONDENSED_HISTORY_PREFIX}\n\n{summary}"))
}

/// Whether `entry` is a note made by [`condensed_history`].
pub fn is_condensed_history(entry: &Value) -> bool {
    role(entry) == "user"
        && block_type(entry) == Some("text")
        && block(entry)["text"].as_str().is_some_and(|t| {
            t.strip_prefix(ACTION_NOTE_PREFIX)
                .is_some_and(|t| t.starts_with(CONDENSED_HISTORY_PREFIX))
        })
}

/// Whether `entry` is an `Action` note (Margin's record, not the owner's words).
pub fn is_action_note(entry: &Value) -> bool {
    role(entry) == "user"
        && block_type(entry) == Some("text")
        && block(entry)["text"]
            .as_str()
            .is_some_and(|t| t.starts_with(ACTION_NOTE_PREFIX))
}

/// A `tool_result` block's output text (its text parts joined), and whether it holds
/// pictures.
pub fn result_text(block: &Value) -> String {
    match &block["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The outputs already given, by `tool_use_id`.
pub(crate) fn answered(entries: &[Value]) -> HashMap<String, String> {
    entries
        .iter()
        .filter(|e| role(e) == "user" && block_type(e) == Some("tool_result"))
        .filter_map(|e| {
            let b = block(e);
            Some((b["tool_use_id"].as_str()?.to_string(), result_text(b)))
        })
        .collect()
}

fn is_thinking(block: &Value) -> bool {
    matches!(
        block["type"].as_str(),
        Some("thinking" | "redacted_thinking")
    )
}

/// Whether `entry` is a hosted tool's result (`web_search_tool_result`, …).
fn is_server_result(entry: &Value) -> bool {
    block_type(entry).is_some_and(|t| t.ends_with("_tool_result") && t != "tool_result")
}

/// Whether the entry is a thinking block (the condensed tail leaves them out: the
/// summary replaces the prefix they were bound to).
pub(crate) fn is_thinking_entry(entry: &Value) -> bool {
    is_thinking(block(entry))
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn a(block: Value) -> Value {
        entry("assistant", block)
    }

    fn text(t: &str) -> Value {
        json!({"type": "text", "text": t})
    }

    fn thinking(t: &str) -> Value {
        json!({"type": "thinking", "thinking": t, "signature": format!("sig-{t}")})
    }

    fn tool_use(id: &str) -> Value {
        json!({"type": "tool_use", "id": id, "name": "list_topics", "input": {}})
    }

    fn server_use(id: &str) -> Value {
        json!({"type": "server_tool_use", "id": id, "name": "web_search", "input": {"query": "q"}})
    }

    fn search_result(id: &str) -> Value {
        json!({"type": "web_search_tool_result", "tool_use_id": id, "content": [
            {"type": "web_search_result", "url": "https://e.example", "title": "E", "encrypted_content": "enc", "page_age": null}
        ]})
    }

    fn with_items(stored: &[Vec<Value>]) -> Vec<Value> {
        let items: Vec<ChatItem> = (1..=stored.len() as u64).map(item).collect();
        let replay: Vec<ReplayItem> = items
            .iter()
            .zip(stored)
            .map(|(item, m)| ReplayItem {
                item,
                model_items: m,
            })
            .rev()
            .collect();
        messages(&replay)
    }

    #[test]
    fn blocks_of_one_role_are_one_message() {
        let stored = vec![
            user_message("ctx", "hi"),
            vec![a(thinking("t1"))],
            vec![a(tool_use("tu_1"))],
            vec![tool_result("tu_1", "[]", true, &[])],
            vec![a(text("None yet."))],
        ];
        let m = with_items(&stored);
        assert_eq!(
            m,
            vec![
                json!({"role": "user", "content": [text("ctx"), text("hi")]}),
                json!({"role": "assistant", "content": [thinking("t1"), tool_use("tu_1")]}),
                json!({"role": "user", "content": [block(&tool_result("tu_1", "[]", true, &[]))]}),
                json!({"role": "assistant", "content": [text("None yet.")]}),
            ]
        );
    }

    #[test]
    fn a_note_between_calls_and_results_follows_the_results() {
        let note = action_note("Created the course “Optics”.");
        let stored = vec![
            user_message("ctx", "go"),
            vec![a(tool_use("tu_1"))],
            vec![a(tool_use("tu_2"))],
            vec![note.clone()],
            vec![tool_result("tu_1", "ok", true, &[])],
            vec![tool_result("tu_2", "ok", true, &[])],
            vec![a(text("Done."))],
        ];
        let m = with_items(&stored);
        assert_eq!(m.len(), 4);
        let user = m[2]["content"].as_array().unwrap();
        assert_eq!(user[0]["tool_use_id"], "tu_1");
        assert_eq!(user[1]["tool_use_id"], "tu_2");
        assert_eq!(user[2], *block(&note));
        assert!(
            user[2]["text"]
                .as_str()
                .unwrap()
                .starts_with(ACTION_NOTE_PREFIX)
        );
    }

    #[test]
    fn unanswered_calls_get_a_cancelled_result() {
        let stored = vec![
            user_message("ctx", "go"),
            vec![a(tool_use("tu_1"))],
            vec![a(tool_use("tu_2"))],
            vec![tool_result("tu_2", "ok", true, &[])],
            vec![action_note("Stopped by the owner.")],
        ];
        let m = with_items(&stored);
        let user = m[2]["content"].as_array().unwrap();
        assert_eq!(user[0], *block(&cancelled_call_output("tu_1")));
        assert_eq!(user[0]["is_error"], true);
        assert_eq!(user[0]["content"], r#"{"error":"cancelled by the user"}"#);
        assert_eq!(user[1]["tool_use_id"], "tu_2");
        assert_eq!(user[2]["text"], "Margin: Stopped by the owner.");
        // A trailing unanswered call gets its own message.
        let m = with_items(&stored[..3]);
        assert_eq!(m.len(), 3);
        assert_eq!(m[2]["content"].as_array().unwrap().len(), 2);

        let items: Vec<ChatItem> = (1..=3).map(item).collect();
        let replay: Vec<ReplayItem> = items
            .iter()
            .zip(&stored)
            .map(|(item, m)| ReplayItem {
                item,
                model_items: m,
            })
            .collect();
        let unanswered = unanswered_calls(&replay);
        assert_eq!(unanswered.len(), 2);
        assert_eq!(
            (unanswered[0].0.as_str(), unanswered[0].1.id.as_str()),
            ("tu_1", "i2")
        );
    }

    #[test]
    fn a_trailing_server_call_and_lone_thinking_are_left_out() {
        // Stopped while the search ran.
        let stored = vec![
            user_message("ctx", "look it up"),
            vec![a(thinking("t1"))],
            vec![a(server_use("srv_1"))],
        ];
        let m = with_items(&stored);
        assert_eq!(m.len(), 1, "{m:?}");
        // Stopped, then a note and a new message: still left out.
        let mut more = stored.clone();
        more.push(vec![action_note("Stopped by the owner.")]);
        more.push(user_message("ctx", "never mind"));
        let m = with_items(&more);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0]["content"].as_array().unwrap().len(), 5);
        // A paused response is sent back as it came.
        let entries: Vec<Value> = stored.concat();
        let m = render(&entries, true);
        assert_eq!(
            m[1]["content"],
            json!([thinking("t1"), server_use("srv_1")])
        );
        // Unresolved server calls of earlier assistant messages never come back.
        assert!(!unanswered_calls_ids(&more).contains(&"srv_1".to_string()));
    }

    fn unanswered_calls_ids(stored: &[Vec<Value>]) -> Vec<String> {
        let items: Vec<ChatItem> = (1..=stored.len() as u64).map(item).collect();
        let replay: Vec<ReplayItem> = items
            .iter()
            .zip(stored)
            .map(|(item, m)| ReplayItem {
                item,
                model_items: m,
            })
            .collect();
        unanswered_calls(&replay)
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    #[test]
    fn a_pending_server_call_is_sent_with_the_client_results() {
        let stored = vec![
            user_message("ctx", "search and list"),
            vec![a(server_use("srv_1"))],
            vec![a(tool_use("tu_1"))],
            vec![tool_result("tu_1", "[]", true, &[])],
        ];
        let m = with_items(&stored);
        assert_eq!(
            m[1]["content"],
            json!([server_use("srv_1"), tool_use("tu_1")])
        );
        assert_eq!(m[2]["content"][0]["tool_use_id"], "tu_1");
        // Its result arrives in the next response; then it stays for good.
        let mut done = stored.clone();
        done.push(vec![a(search_result("srv_1"))]);
        done.push(vec![a(text("Found it."))]);
        done.push(user_message("ctx", "thanks"));
        let m = with_items(&done);
        assert_eq!(
            m[1]["content"],
            json!([server_use("srv_1"), tool_use("tu_1")])
        );
        assert_eq!(
            m[3]["content"],
            json!([search_result("srv_1"), text("Found it.")])
        );
        assert!(unanswered_calls_ids(&done).is_empty());
    }

    #[test]
    fn replay_starts_at_the_latest_compaction_or_condensed_note() {
        let compaction = a(json!({"type": "compaction", "content": "summary"}));
        let stored = vec![
            user_message("ctx", "old"),
            vec![a(text("old answer"))],
            vec![compaction.clone()],
            vec![a(text("after"))],
        ];
        let m = with_items(&stored);
        assert_eq!(
            m,
            vec![json!({"role": "assistant", "content": [block(&compaction), text("after")]})]
        );
        let mut condensed = stored.clone();
        condensed.push(
            [
                vec![condensed_history("they talked")],
                user_message("ctx", "kept"),
            ]
            .concat(),
        );
        let m = with_items(&condensed);
        assert_eq!(m.len(), 1);
        let content = m[0]["content"].as_array().unwrap();
        assert!(
            content[0]["text"]
                .as_str()
                .unwrap()
                .starts_with("Margin: The earlier part of this chat was condensed")
        );
        assert_eq!(content[2], text("kept"));
        assert!(is_condensed_history(&condensed_history("x")));
        assert!(!is_condensed_history(&action_note("Stopped.")));
    }

    #[test]
    fn tool_results_carry_their_pictures() {
        let jpeg = margin_ai::Image {
            media_type: "image/jpeg".into(),
            data: vec![0xff, 0xd8, 0xff],
        };
        let r = tool_result("tu_1", "Page 3", true, &[jpeg]);
        assert_eq!(
            *block(&r),
            json!({"type": "tool_result", "tool_use_id": "tu_1", "is_error": false, "content": [
                {"type": "text", "text": "Page 3"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "/9j/"}},
            ]})
        );
        assert_eq!(result_text(block(&r)), "Page 3");
        assert_eq!(role(&r), "user");
    }

    #[test]
    fn rendering_is_deterministic_and_prefix_stable() {
        let mut entries = user_message("ctx", "go");
        let first = render(&entries, false);
        entries.extend([a(thinking("t1")), a(tool_use("tu_1"))]);
        entries.push(tool_result("tu_1", "ok", true, &[]));
        let second = render(&entries, false);
        assert_eq!(second[..first.len()], first[..]);
        entries.push(a(text("Answer.")));
        entries.extend(user_message("ctx", "more"));
        let third = render(&entries, false);
        assert_eq!(third[..second.len()], second[..]);
        assert_eq!(render(&entries, false), third);
    }
}
