//! Chats with the agent: storage (tables `chat` and `chat_item`) and the chat routes.
//! Turns themselves run in `turns`. See `docs/agent-contract.md`.
//!
//! Every change is stored under `write_lock` and then broadcast to the chat's audience
//! (its `access`, L1 D8): `ChatChanged` for the chat record (not for `updated_at`
//! alone), `ChatItemChanged` for items, `ChatDeleted` on delete.
//!
//! A chat is private to the person who made it (L1 D6). Anyone in its audience may read
//! it and stop or continue its turn; only its owner sends messages to it, confirms its
//! tool calls, renames or deletes it (403 `not_owner`). Outside the audience it is 404.

use std::collections::HashSet;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::Utc;
use margin_shared::{
    CHAT_TITLE_MAX, Card, Chat, ChatDetail, ChatItem, ChatItemKind, ChatPatch, ChatState,
    ConfirmRequest, DeviceRef, Event, Id, MessageSent, NewChat, NewMessage, PendingMessage, Source,
    TitleSource, ToolOutputText, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{ApiJson, AppError, AppResult};
use crate::principal::{CurrentDevice, CurrentPrincipal, Principal};
use crate::state::{
    AppState, Emitted, Owned, Ownership, can_read, check_owner, list_visible, new_id, readable,
};
use crate::turns::{self, Start};
use crate::{db, ingest, sources, workspaces};
use margin_shared::context::LibraryKind;
use margin_shared::sync::Locked;

/// Longest message accepted, in bytes.
pub const MAX_MESSAGE_BYTES: usize = 32 * 1024;
/// Longest chat title a rename keeps, in characters.
pub const MAX_TITLE_CHARS: usize = 200;

// ---------------------------------------------------------------------------
// Stored rows
// ---------------------------------------------------------------------------

/// A chat item as stored: the wire item plus what only the service keeps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredChatItem {
    #[serde(flatten)]
    pub item: ChatItem,
    /// The Responses API items it stands for, verbatim (`margin_agent::ItemRecord`).
    #[serde(default)]
    pub model_items: Vec<Value>,
    /// A tool result's full output (served by `item_output`, never pushed).
    #[serde(default)]
    pub full_output: Option<String>,
}

// Chats are stored as the wire `Chat` in table `chat`; items as `StoredChatItem` in
// `chat_item`, keyed by item id, with `chat_id` for `list_where`.

// ---------------------------------------------------------------------------
// Loading and saving
// ---------------------------------------------------------------------------

/// The chat, when `p` may read it; else 404 "not_found".
pub async fn chat(state: &AppState, p: &Principal, id: &str) -> AppResult<Chat> {
    readable(p, state.store.get(db::CHAT, id).await?, "chat")
}

/// The chat whoever asks (turns, which hold its id already), or 404 "not_found".
pub async fn chat_row(state: &AppState, id: &str) -> AppResult<Chat> {
    state
        .store
        .get(db::CHAT, id)
        .await?
        .ok_or_else(|| AppError::not_found("chat"))
}

/// Who hears about `chat_id`'s items: the chat's audience, kept in memory by
/// `save_chat_locked` (read from the store on first use). Empty (nobody) once the chat
/// is gone.
pub async fn chat_audience(state: &AppState, chat_id: &str) -> anyhow::Result<Vec<String>> {
    if let Some(a) = state.turns.audiences.locked().get(chat_id) {
        return Ok(a.clone());
    }
    let Some(chat) = state.store.get::<Chat>(db::CHAT, chat_id).await? else {
        return Ok(Vec::new());
    };
    let audience = chat.audience();
    state
        .turns
        .audiences
        .locked()
        .insert(chat_id.to_string(), audience.clone());
    Ok(audience)
}

/// The cached audience of `chat_id` without a store read (empty when unknown). For the
/// streaming deltas, which follow an item `save_item_locked` already saved.
pub fn cached_audience(state: &AppState, chat_id: &str) -> Vec<String> {
    state
        .turns
        .audiences
        .locked()
        .get(chat_id)
        .cloned()
        .unwrap_or_default()
}

/// A chat's items, ordered by `seq`.
pub async fn items(state: &AppState, chat_id: &str) -> anyhow::Result<Vec<StoredChatItem>> {
    let mut rows: Vec<StoredChatItem> = state
        .store
        .list_where(db::CHAT_ITEM, "chat_id", chat_id)
        .await?;
    rows.sort_by_key(|r| r.item.seq);
    Ok(rows)
}

/// The documents a chat's Passages cards quote, once each (a citation from before
/// documents existed names its source, whose document has the same id): what the agent
/// has read in the chat and may repeat (L2 D4.3, `audience::note_refusal`).
pub async fn quoted_documents(state: &AppState, chat_id: &str) -> anyhow::Result<Vec<Id>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for row in items(state, chat_id).await? {
        let ChatItemKind::Card {
            card: Card::Passages(card),
            ..
        } = &row.item.kind
        else {
            continue;
        };
        for c in &card.passages {
            let id = c.document_id.as_ref().unwrap_or(&c.source_id);
            if seen.insert(id.clone()) {
                out.push(id.clone());
            }
        }
    }
    Ok(out)
}

/// Stores the chat (bumping `updated_at`) and emits `ChatChanged` when something else
/// changed (state, title, pending message, usage, attachments, waiting): a new item only
/// bumps `updated_at`, which is saved silently. Call with `write_lock` held.
pub async fn save_chat_locked(state: &AppState, chat: &mut Chat) -> anyhow::Result<()> {
    let before: Option<Chat> = state.store.get(db::CHAT, &chat.id).await?;
    chat.updated_at = Utc::now();
    state.store.put(db::CHAT, &chat.id, &*chat).await?;
    let changed = before.is_none_or(|mut b| {
        b.updated_at = chat.updated_at;
        b != *chat
    });
    state
        .turns
        .audiences
        .locked()
        .insert(chat.id.clone(), chat.audience());
    if changed {
        state.emit_owned(&*chat, Event::ChatChanged { chat: chat.clone() });
    }
    Ok(())
}

/// Stores the item and emits `ChatItemChanged` (the wire item, without the stored
/// extras) to the chat's audience (`chat_audience`). Call with `write_lock` held.
pub async fn save_item_locked(state: &AppState, item: &StoredChatItem) -> anyhow::Result<()> {
    state.store.put(db::CHAT_ITEM, &item.item.id, item).await?;
    let audience = chat_audience(state, &item.item.chat_id).await?;
    state.emit_all([Emitted::paths(
        &audience,
        Event::ChatItemChanged {
            item: public_item(state, &item.item),
        },
    )]);
    Ok(())
}

/// The wire item: pictures of a passages card (stored as media file names, like a
/// lesson's citations) become signed URLs.
pub fn public_item(state: &AppState, item: &ChatItem) -> ChatItem {
    let mut item = item.clone();
    if let ChatItemKind::Card {
        card: Card::Passages(card),
        ..
    } = &mut item.kind
    {
        for c in &mut card.passages {
            c.image_url = c
                .image_url
                .take()
                .map(|file| state.source_media_url(&c.source_id, &file));
        }
    }
    item
}

/// The next `seq` for an item of `chat_id`. Call with `write_lock` held: the counter is
/// read from the store once per chat and then kept in memory.
pub async fn next_seq_locked(state: &AppState, chat_id: &str) -> anyhow::Result<u64> {
    let known = state.turns.seqs.locked().get(chat_id).copied();
    let last = match known {
        Some(last) => last,
        None => items(state, chat_id)
            .await?
            .last()
            .map(|r| r.item.seq)
            .unwrap_or(0),
    };
    let next = last + 1;
    state.turns.seqs.locked().insert(chat_id.to_string(), next);
    Ok(next)
}

/// A new item of `chat_id` in `turn_id` with the next `seq`. Call with `write_lock`
/// held; nothing is stored yet.
pub async fn new_item_locked(
    state: &AppState,
    chat_id: &str,
    turn_id: &str,
    status: margin_shared::ItemStatus,
    kind: ChatItemKind,
) -> anyhow::Result<ChatItem> {
    let now = Utc::now();
    Ok(ChatItem {
        id: new_id(),
        chat_id: chat_id.to_string(),
        turn_id: turn_id.to_string(),
        seq: next_seq_locked(state, chat_id).await?,
        status,
        created_at: now,
        updated_at: now,
        kind,
    })
}

/// A chat's title from its first message: the first line, at most `CHAT_TITLE_MAX`
/// characters (a cut one ends with "…").
pub fn title_from(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or("").trim();
    if line.chars().count() <= CHAT_TITLE_MAX {
        return line.to_string();
    }
    let mut title: String = line.chars().take(CHAT_TITLE_MAX - 1).collect();
    title = title.trim_end().to_string();
    title.push('…');
    title
}

/// Trims the text and checks it and its sources (which must exist and be `p`'s to
/// read; duplicates are dropped).
async fn check_message(state: &AppState, p: &Principal, msg: NewMessage) -> AppResult<NewMessage> {
    let text = msg.text.trim().to_string();
    if text.is_empty() {
        return Err(AppError::bad_request("Write a message first."));
    }
    if text.len() > MAX_MESSAGE_BYTES {
        return Err(AppError::bad_request(format!(
            "That message is too long: at most {} KB.",
            MAX_MESSAGE_BYTES / 1024
        )));
    }
    let source_ids = check_sources(state, p, msg.source_ids).await?;
    Ok(NewMessage { text, source_ids })
}

/// Every source must exist and be readable by `p` (one that isn't doesn't exist for
/// them). Duplicates are dropped.
async fn check_sources(state: &AppState, p: &Principal, mut ids: Vec<Id>) -> AppResult<Vec<Id>> {
    let mut seen = HashSet::new();
    ids.retain(|id| seen.insert(id.clone()));
    for id in &ids {
        let source = state.store.get::<Source>(db::SOURCE, id).await?;
        let Some(source) = source.filter(|s| can_read(p, s)) else {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "unknown_source",
                "One of the files isn't on the service any more. Add it again.",
            ));
        };
        if source.workspace_id.is_some() {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "unknown_source",
                "That file belongs to a folder; attach the folder instead.",
            ));
        }
    }
    Ok(ids)
}

/// Every folder must still be attached and readable by `p`. Duplicates are dropped.
async fn check_workspaces(state: &AppState, p: &Principal, mut ids: Vec<Id>) -> AppResult<Vec<Id>> {
    let mut seen = HashSet::new();
    ids.retain(|id| seen.insert(id.clone()));
    let found = workspaces::visible_by_ids(state, p, &ids).await?;
    if found.len() != ids.len() {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "unknown_workspace",
            "One of the folders isn't attached any more. Attach it again.",
        ));
    }
    Ok(ids)
}

/// Sends `msg` from `device` on `chat`: starts a turn when the chat isn't busy, else
/// queues it as the pending message. Call with `write_lock` held.
async fn send_locked(
    state: &AppState,
    chat: &mut Chat,
    msg: NewMessage,
    device: DeviceRef,
) -> AppResult<Option<ChatItem>> {
    if chat.state.is_busy() {
        turns::queue_locked(
            state,
            chat,
            PendingMessage {
                text: msg.text,
                source_ids: msg.source_ids,
                device_id: device.device_id,
                created_at: Utc::now(),
            },
        )
        .await?;
        return Ok(None);
    }
    if chat.pending.is_some() {
        // An interrupted or failed turn keeps its pending message; it goes first.
        return Err(AppError::conflict(
            "already_pending",
            "A message is already waiting to be sent. Continue the answer, or cancel the waiting message first.",
        ));
    }
    turns::start_locked(
        state,
        chat,
        Start::Message {
            text: msg.text,
            source_ids: msg.source_ids,
            device,
        },
    )
    .await
}

/// Deletes the sources in `ids` that belong to no topic and to no chat but `chat_id`.
/// Call with `write_lock` held; returns the events to emit.
async fn delete_chat_only_sources_locked(
    state: &AppState,
    chat_id: &str,
    ids: &[Id],
) -> anyhow::Result<Vec<Emitted>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let chats: Vec<Chat> = state.store.list(db::CHAT).await?;
    let elsewhere: HashSet<&str> = chats
        .iter()
        .filter(|c| c.id != chat_id)
        .flat_map(|c| c.source_ids.iter().map(String::as_str))
        .collect();
    let mut events = Vec::new();
    for id in ids {
        let Some(source) = state.store.get::<Source>(db::SOURCE, id).await? else {
            continue;
        };
        if source.topic_id.is_none() && !elsewhere.contains(id.as_str()) {
            events.push(sources::delete_locked(state, id).await?);
        }
    }
    Ok(events)
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// `GET /api/chats`: the chats the caller may read, newest `updated_at` first.
pub async fn list_chats(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
) -> AppResult<Json<Vec<Chat>>> {
    let mut chats: Vec<Chat> = list_visible(&state.store, db::CHAT, &p).await?;
    chats.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    Ok(Json(chats))
}

/// `POST /api/chats`: creates the chat, private to the caller (`ChatChanged`); with a
/// message, also stores it and starts the first turn as `post_message` does.
pub async fn create_chat(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    CurrentDevice(device): CurrentDevice,
    ApiJson(req): ApiJson<NewChat>,
) -> AppResult<Json<ChatDetail>> {
    let workspace_ids = check_workspaces(&state, &p, req.workspace_ids).await?;
    let message = match req.message {
        Some(m) => Some(check_message(&state, &p, m).await?),
        None => None,
    };
    // The provider asked for, else the person's default, else Settings' default (D9);
    // the chat stays on it for good.
    let provider = match req.provider.or(p.person.settings.default_provider) {
        Some(provider) => provider,
        None => state.default_provider().await?,
    };
    let own = Ownership::private(&p);
    let _w = state.write_lock.lock().await;
    let now = Utc::now();
    let mut chat = Chat {
        access: own.access,
        deny: own.deny,
        owner: Some(own.owner),
        id: new_id(),
        title: String::new(),
        title_by: TitleSource::FirstLine,
        state: ChatState::Idle,
        workspace_ids,
        source_ids: Vec::new(),
        pending: None,
        usage: Usage::default(),
        waiting: None,
        provider,
        created_at: now,
        updated_at: now,
    };
    save_chat_locked(&state, &mut chat).await?;
    let mut items = Vec::new();
    if let Some(msg) = message {
        let sender = DeviceRef {
            device_id: device.id.clone(),
            name: device.name.clone(),
        };
        items.extend(send_locked(&state, &mut chat, msg, sender).await?);
    }
    Ok(Json(ChatDetail { chat, items }))
}

/// `GET /api/chats/{id}`.
pub async fn get_chat(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    Path(id): Path<Id>,
) -> AppResult<Json<ChatDetail>> {
    let chat = chat(&state, &p, &id).await?;
    let mut items: Vec<ChatItem> = items(&state, &id)
        .await?
        .into_iter()
        .map(|r| public_item(&state, &r.item))
        .collect();
    // In-progress text lives in memory until the item is done.
    let live = state.turns.live_text.locked();
    for item in &mut items {
        if item.status != margin_shared::ItemStatus::InProgress {
            continue;
        }
        if let Some(text) = live.get(&item.id) {
            match &mut item.kind {
                ChatItemKind::AssistantText { text: t, .. }
                | ChatItemKind::Reasoning { summary: t }
                | ChatItemKind::ToolCall { arguments: t, .. } => *t = text.clone(),
                _ => {}
            }
        }
    }
    drop(live);
    Ok(Json(ChatDetail { chat, items }))
}

/// `PATCH /api/chats/{id}`: rename (trimmed, not empty, at most 200 characters; the
/// title is then the owner's and the model never replaces it), replace attached folders
/// (each must exist) or files.
pub async fn patch_chat(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    Path(id): Path<Id>,
    ApiJson(patch): ApiJson<ChatPatch>,
) -> AppResult<Json<Chat>> {
    // Who may see it and whether it's theirs, before anything else is checked.
    check_owner(&p, &chat(&state, &p, &id).await?)?;
    let title = match &patch.title {
        Some(t) => {
            let t = t.trim();
            if t.is_empty() {
                return Err(AppError::bad_request("Give the chat a name."));
            }
            Some(t.chars().take(MAX_TITLE_CHARS).collect::<String>())
        }
        None => None,
    };
    let workspace_ids = match patch.workspace_ids {
        Some(ids) => Some(check_workspaces(&state, &p, ids).await?),
        None => None,
    };
    let source_ids = match patch.source_ids {
        Some(ids) => Some(check_sources(&state, &p, ids).await?),
        None => None,
    };
    let (chat, events) = {
        let _w = state.write_lock.lock().await;
        let mut chat = chat(&state, &p, &id).await?;
        check_owner(&p, &chat)?;
        if let Some(t) = title {
            chat.title = t;
            chat.title_by = TitleSource::Owner;
        }
        if let Some(ids) = workspace_ids {
            chat.workspace_ids = ids;
        }
        let mut dropped = Vec::new();
        let mut added = Vec::new();
        if let Some(ids) = source_ids {
            dropped = chat
                .source_ids
                .iter()
                .filter(|s| !ids.contains(s))
                .cloned()
                .collect();
            added = ids
                .iter()
                .filter(|s| !chat.source_ids.contains(s))
                .cloned()
                .collect();
            chat.source_ids = ids;
        }
        save_chat_locked(&state, &mut chat).await?;
        let mut events = Vec::new();
        // What the chat searches follows its files.
        for source_id in &added {
            events.extend(ingest::add_scope_locked(&state, source_id, &id).await?);
        }
        for source_id in &dropped {
            events.extend(ingest::remove_scope_locked(&state, source_id, &id).await?);
        }
        // The chat no longer lists them, so they go unless a topic or another chat has them.
        events.extend(delete_chat_only_sources_locked(&state, &id, &dropped).await?);
        (chat, events)
    };
    state.emit_all(events);
    Ok(Json(chat))
}

/// `DELETE /api/chats/{id}`: stops a busy turn (without sending the pending message),
/// deletes the items and the sources attached only to this chat, emits `ChatDeleted`.
/// Jobs the chat started (lessons, videos) carry on.
pub async fn delete_chat(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    Path(id): Path<Id>,
) -> AppResult<StatusCode> {
    let (chat, events) = {
        let _w = state.write_lock.lock().await;
        let chat = chat(&state, &p, &id).await?;
        check_owner(&p, &chat)?;
        // The items go too, so there is nothing to finish: just end the task.
        state.turns.abort(&id);
        {
            let rows = items(&state, &id).await?;
            let mut live = state.turns.live_text.locked();
            for r in rows {
                live.remove(&r.item.id);
            }
        }
        state
            .store
            .delete_where(db::CHAT_ITEM, "chat_id", &id)
            .await?;
        state.store.delete(db::CHAT, &id).await?;
        state.turns.forget_chat(&id);
        // Its transcript leaves the index; files that stay (a topic's or another chat's)
        // are no longer searched from it.
        ingest::delete_library_document_locked(&state, LibraryKind::Chat, &id).await?;
        let mut events = Vec::new();
        for source_id in &chat.source_ids {
            events.extend(ingest::remove_scope_locked(&state, source_id, &id).await?);
        }
        events.extend(delete_chat_only_sources_locked(&state, &id, &chat.source_ids).await?);
        (chat, events)
    };
    state.emit_all(events);
    state.emit_owned(&chat, Event::ChatDeleted { id });
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/chats/{id}/messages`: see `routes::chat_messages`.
pub async fn post_message(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    CurrentDevice(device): CurrentDevice,
    Path(id): Path<Id>,
    ApiJson(msg): ApiJson<NewMessage>,
) -> AppResult<Json<MessageSent>> {
    // The turn runs with the chat owner's access, so only the owner starts one.
    check_owner(&p, &chat(&state, &p, &id).await?)?;
    let msg = check_message(&state, &p, msg).await?;
    let _w = state.write_lock.lock().await;
    let mut chat = chat(&state, &p, &id).await?;
    check_owner(&p, &chat)?;
    let sender = DeviceRef {
        device_id: device.id.clone(),
        name: device.name.clone(),
    };
    let item = send_locked(&state, &mut chat, msg, sender).await?;
    Ok(Json(MessageSent { chat, item }))
}

/// `DELETE /api/chats/{id}/pending`.
pub async fn cancel_pending(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    Path(id): Path<Id>,
) -> AppResult<Json<Chat>> {
    let _w = state.write_lock.lock().await;
    let mut chat = chat(&state, &p, &id).await?;
    check_owner(&p, &chat)?;
    if chat.pending.take().is_some() {
        save_chat_locked(&state, &mut chat).await?;
    }
    Ok(Json(chat))
}

/// `POST /api/chats/{id}/stop`: `turns::stop`, by anyone in the chat's audience.
pub async fn stop(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    CurrentDevice(device): CurrentDevice,
    Path(id): Path<Id>,
) -> AppResult<Json<Chat>> {
    Ok(Json(turns::stop(&state, &p, &id, &device.id).await?))
}

/// `POST /api/chats/{id}/continue`: `turns::resume`, by anyone in the chat's audience.
pub async fn continue_turn(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    Path(id): Path<Id>,
) -> AppResult<Json<Chat>> {
    Ok(Json(turns::resume(&state, &p, &id).await?))
}

/// `POST /api/chats/{id}/confirm`: `turns::decide`, by the chat's owner only.
pub async fn confirm(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    CurrentDevice(device): CurrentDevice,
    Path(id): Path<Id>,
    ApiJson(req): ApiJson<ConfirmRequest>,
) -> AppResult<Json<Chat>> {
    Ok(Json(
        turns::decide(&state, &p, &id, &req.item_id, req.approve, &device.id).await?,
    ))
}

/// `GET /api/chats/{id}/items/{item_id}/output`: 404 unless it is a `ToolResult` of
/// this chat.
pub async fn item_output(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    Path((chat_id, item_id)): Path<(Id, Id)>,
) -> AppResult<Json<ToolOutputText>> {
    // A chat outside the caller's access has no items for them.
    chat(&state, &p, &chat_id)
        .await
        .map_err(|_| AppError::not_found("tool result"))?;
    let row: Option<StoredChatItem> = state.store.get(db::CHAT_ITEM, &item_id).await?;
    let Some(row) = row.filter(|r| {
        r.item.chat_id == chat_id && matches!(r.item.kind, ChatItemKind::ToolResult { .. })
    }) else {
        return Err(AppError::not_found("tool result"));
    };
    // Older rows may only have the output inside the replayed `function_call_output`.
    let text = row
        .full_output
        .or_else(|| {
            row.model_items
                .iter()
                .find_map(|m| m.get("output").and_then(Value::as_str).map(str::to_string))
        })
        .unwrap_or_default();
    Ok(Json(ToolOutputText { text }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_come_from_the_first_line() {
        assert_eq!(title_from("  Fourier series\nand more"), "Fourier series");
        let long = "x".repeat(CHAT_TITLE_MAX + 10);
        let t = title_from(&long);
        assert_eq!(t.chars().count(), CHAT_TITLE_MAX);
        assert!(t.ends_with('…'));
        assert_eq!(
            title_from(&"é".repeat(CHAT_TITLE_MAX)).chars().count(),
            CHAT_TITLE_MAX
        );
    }
}
