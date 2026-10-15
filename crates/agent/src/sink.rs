//! How the loop hands its items to the service.

use chrono::{DateTime, Utc};
use margin_ai::ModelWindow;
use margin_shared::{ChatItem, ChatItemKind, Decision, ItemStatus, Usage};
use serde_json::Value;

use crate::BoxFuture;

/// A storage failure; the loop ends the turn as failed with this message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SinkError(pub String);

/// What the service stores with an item besides the wire `ChatItem`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ItemRecord {
    /// The provider's wire items this chat item stands for, verbatim, replayed on later
    /// calls (`replay::history`). On OpenAI and ChatGPT, Responses items: the
    /// `output_item.done` item for model output (message, reasoning, function_call,
    /// web_search_call, compaction), the `function_call_output` for a tool result, the
    /// `user_message_input` items for a user message. On Claude, content blocks with
    /// their role (`{"role":…,"block":…}`, `replay::claude`). `None` leaves what was
    /// stored unchanged.
    pub model_items: Option<Vec<Value>>,
    /// A tool result's whole output as the model saw it (served by
    /// `routes::chat_item_output`). `None` leaves it unchanged.
    pub full_output: Option<String>,
}

/// The service's side of a running turn. Every method stores before it broadcasts, so
/// a device that reloads the chat after an event sees what the event said.
pub trait TurnSink: Send + Sync {
    /// Appends an item to the chat: assigns `id`, the next `seq`, `turn_id` and times,
    /// stores it with `record`, emits `Event::ChatItemChanged`, bumps
    /// `Chat.updated_at` (emitting `Event::ChatChanged`). Returns the stored item.
    fn add_item(
        &self,
        status: ItemStatus,
        kind: ChatItemKind,
        record: ItemRecord,
    ) -> BoxFuture<'_, Result<ChatItem, SinkError>>;

    /// Text appended to an `InProgress` item (assistant text, reasoning summary, tool
    /// arguments): emits `Event::ChatItemDelta` with `offset` = the length of that text
    /// before `text`. The sink keeps each in-progress item's text in memory (so
    /// `GET routes::chat` can return it live) and may save it now and then; it is not
    /// stored per delta. `update_item` carries the final value.
    fn delta(&self, item: &ChatItem, text: &str);

    /// Stores `item` (its new status and kind) and `record`, and emits
    /// `Event::ChatItemChanged`.
    fn update_item<'a>(
        &'a self,
        item: &'a ChatItem,
        record: ItemRecord,
    ) -> BoxFuture<'a, Result<(), SinkError>>;

    /// Adds one model call's usage to `Chat.usage` and emits `Event::ChatChanged`.
    fn add_usage<'a>(&'a self, usage: &'a Usage) -> BoxFuture<'a, Result<(), SinkError>>;

    /// Parks the turn on the confirmation card `card`: sets the chat
    /// `ChatState::WaitingForUser` (emitting `Event::ChatChanged`) and resolves with
    /// the owner's decision once `routes::chat_confirm` answers. The service stores the
    /// decision on the card (emitting `Event::ChatItemChanged`) and sets the chat back
    /// to `Running` before resolving. Stop never resolves it: the service aborts the
    /// turn and finishes the card itself (`Denied { by_stop: true }`).
    fn wait_for_decision<'a>(
        &'a self,
        card: &'a ChatItem,
    ) -> BoxFuture<'a, Result<Decision, SinkError>>;

    /// The loop is about to sleep until `until` before retrying a model call (`reason`
    /// says why, for the chat, e.g. "The model API asked Margin to slow down."): the
    /// service sets `Chat.waiting` and emits `Event::ChatChanged`. Called before every
    /// retry sleep, including a `Retry-After` far beyond `RETRY_MAX_DELAY` (honoured as
    /// asked). An error ends the turn like any storage failure.
    fn retrying<'a>(
        &'a self,
        until: DateTime<Utc>,
        reason: &'a str,
    ) -> BoxFuture<'a, Result<(), SinkError>> {
        let _ = (until, reason);
        Box::pin(async { Ok(()) })
    }

    /// The retry sleep is over: the service clears `Chat.waiting` and emits
    /// `Event::ChatChanged`. The service also clears it when the turn ends.
    fn clear_waiting(&self) -> BoxFuture<'_, Result<(), SinkError>> {
        Box::pin(async { Ok(()) })
    }

    /// The chat model's context window (`ModelChoice.window`, D13). The loop asks the
    /// server to compact at `min(COMPACT_THRESHOLD_TOKENS, 0.6 × window.input)`, and
    /// condenses here past the same threshold when the server can't. `None`: the
    /// constant alone.
    fn model_window(&self) -> Option<ModelWindow> {
        None
    }

    /// Whether to ask the server for compaction: false once this provider and base URL
    /// rejected it (the service remembers that per `(provider, base_url)`, D3).
    fn server_compaction(&self) -> bool {
        true
    }

    /// The model API rejected server-side compaction (OpenAI: an unknown
    /// `context_management`; Claude: a 400 naming the beta); the loop condenses here
    /// for the rest of the run. The service remembers it for the provider and base URL.
    fn compaction_rejected(&self) {}

    /// One agent step's model call ended (`Completed` or `Incomplete`): its step number
    /// (from 1, per run), the model that answered (empty when the provider didn't say),
    /// the stop reason as the provider sent it, and its usage (also given to
    /// [`TurnSink::add_usage`]). For the trace row (D14).
    fn model_called(&self, step: u32, model: &str, stop_reason: &str, usage: &Usage) {
        let _ = (step, model, stop_reason, usage);
    }
}
