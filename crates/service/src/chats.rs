//! Chats with the agent: storage (tables `chat` and `chat_item`) and the chat routes.
//! Turns themselves run in `turns`. See `docs/agent-contract.md`.
//!
//! Every change is stored under `write_lock` and then broadcast: `ChatChanged` for the
//! chat record, `ChatItemChanged` for items, `ChatDeleted` on delete.

use axum::Extension;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use margin_shared::{
    Chat, ChatDetail, ChatItem, ChatPatch, ConfirmRequest, Id, MessageSent, NewChat, NewMessage,
    ToolOutputText,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::CurrentDevice;
use crate::error::{ApiJson, AppResult};
use crate::state::AppState;

/// Longest message accepted, in bytes.
pub const MAX_MESSAGE_BYTES: usize = 32 * 1024;

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

/// The chat, or 404 "not_found".
pub async fn chat(state: &AppState, id: &str) -> AppResult<Chat> {
    let _ = (state, id);
    unimplemented!("service: chats::chat (phase 1, service owner)")
}

/// A chat's items, ordered by `seq`.
pub async fn items(state: &AppState, chat_id: &str) -> anyhow::Result<Vec<StoredChatItem>> {
    let _ = (state, chat_id);
    unimplemented!("service: chats::items (phase 1, service owner)")
}

/// Stores the chat (bumping `updated_at`) and emits `ChatChanged`. Call with
/// `write_lock` held.
pub async fn save_chat_locked(state: &AppState, chat: &mut Chat) -> anyhow::Result<()> {
    let _ = (state, chat);
    unimplemented!("service: chats::save_chat_locked (phase 1, service owner)")
}

/// Stores the item and emits `ChatItemChanged` (the wire item, without the stored
/// extras). Call with `write_lock` held.
pub async fn save_item_locked(state: &AppState, item: &StoredChatItem) -> anyhow::Result<()> {
    let _ = (state, item);
    unimplemented!("service: chats::save_item_locked (phase 1, service owner)")
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// `GET /api/chats`: newest `updated_at` first.
pub async fn list_chats(State(state): State<AppState>) -> AppResult<Json<Vec<Chat>>> {
    let _ = state;
    unimplemented!("service: chats::list_chats (phase 1, service owner)")
}

/// `POST /api/chats`: creates the chat (`ChatChanged`); with a message, also stores it
/// and starts the first turn as `post_message` does.
pub async fn create_chat(
    State(state): State<AppState>,
    Extension(CurrentDevice(device)): Extension<CurrentDevice>,
    ApiJson(req): ApiJson<NewChat>,
) -> AppResult<Json<ChatDetail>> {
    let _ = (state, device, req);
    unimplemented!("service: chats::create_chat (phase 1, service owner)")
}

/// `GET /api/chats/{id}`.
pub async fn get_chat(
    State(state): State<AppState>,
    Path(id): Path<Id>,
) -> AppResult<Json<ChatDetail>> {
    let _ = (state, id);
    unimplemented!("service: chats::get_chat (phase 1, service owner)")
}

/// `PATCH /api/chats/{id}`: rename (trimmed, not empty, at most 200 characters), replace
/// attached folders (each must exist) or files.
pub async fn patch_chat(
    State(state): State<AppState>,
    Path(id): Path<Id>,
    ApiJson(patch): ApiJson<ChatPatch>,
) -> AppResult<Json<Chat>> {
    let _ = (state, id, patch);
    unimplemented!("service: chats::patch_chat (phase 1, service owner)")
}

/// `DELETE /api/chats/{id}`: stops a busy turn (without sending the pending message),
/// deletes the items and the sources attached only to this chat, emits `ChatDeleted`.
/// Jobs the chat started (lessons, videos) carry on.
pub async fn delete_chat(
    State(state): State<AppState>,
    Path(id): Path<Id>,
) -> AppResult<StatusCode> {
    let _ = (state, id);
    unimplemented!("service: chats::delete_chat (phase 1, service owner)")
}

/// `POST /api/chats/{id}/messages`: see `routes::chat_messages`.
pub async fn post_message(
    State(state): State<AppState>,
    Extension(CurrentDevice(device)): Extension<CurrentDevice>,
    Path(id): Path<Id>,
    ApiJson(msg): ApiJson<NewMessage>,
) -> AppResult<Json<MessageSent>> {
    let _ = (state, device, id, msg);
    unimplemented!("service: chats::post_message (phase 1, service owner)")
}

/// `DELETE /api/chats/{id}/pending`.
pub async fn cancel_pending(
    State(state): State<AppState>,
    Path(id): Path<Id>,
) -> AppResult<Json<Chat>> {
    let _ = (state, id);
    unimplemented!("service: chats::cancel_pending (phase 1, service owner)")
}

/// `POST /api/chats/{id}/stop`: `turns::stop`.
pub async fn stop(
    State(state): State<AppState>,
    Extension(CurrentDevice(device)): Extension<CurrentDevice>,
    Path(id): Path<Id>,
) -> AppResult<Json<Chat>> {
    let _ = (state, device, id);
    unimplemented!("service: chats::stop (phase 1, service owner)")
}

/// `POST /api/chats/{id}/continue`: `turns::resume`.
pub async fn continue_turn(
    State(state): State<AppState>,
    Path(id): Path<Id>,
) -> AppResult<Json<Chat>> {
    let _ = (state, id);
    unimplemented!("service: chats::continue_turn (phase 1, service owner)")
}

/// `POST /api/chats/{id}/confirm`: `turns::decide` (phase 2).
pub async fn confirm(
    State(state): State<AppState>,
    Extension(CurrentDevice(device)): Extension<CurrentDevice>,
    Path(id): Path<Id>,
    ApiJson(req): ApiJson<ConfirmRequest>,
) -> AppResult<Json<Chat>> {
    let _ = (state, device, id, req);
    unimplemented!("service: chats::confirm (phase 2, service owner)")
}

/// `GET /api/chats/{id}/items/{item_id}/output`: 404 unless it is a `ToolResult` of
/// this chat.
pub async fn item_output(
    State(state): State<AppState>,
    Path((chat_id, item_id)): Path<(Id, Id)>,
) -> AppResult<Json<ToolOutputText>> {
    let _ = (state, chat_id, item_id);
    unimplemented!("service: chats::item_output (phase 1, service owner)")
}
