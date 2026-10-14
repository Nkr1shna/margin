//! The turn runner: starts `margin_agent::run_turn` as a background task per chat,
//! stores what it produces (`ServiceSink`), and handles stop, continue, confirmations,
//! the pending message and recovery. See `docs/agent-contract.md`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use margin_agent::{BoxFuture, ItemRecord, SinkError, TurnSink};
use margin_shared::{
    Chat, ChatItem, ChatItemKind, Decision, DeviceRef, Id, ItemStatus, PendingMessage, Usage,
};
use tokio::sync::{Semaphore, oneshot};
use tokio::task::AbortHandle;

use crate::error::AppResult;
use crate::state::AppState;

/// Running turns, one per chat at most. Lives in `App`.
pub struct Turns {
    pub running: Mutex<HashMap<Id, RunningTurn>>,
    /// item id → the streaming text of an `InProgress` item so far, kept by
    /// `ServiceSink::delta` (for delta offsets) and overlaid by `GET /api/chats/{id}`.
    pub live_text: Mutex<HashMap<Id, String>>,
    /// `margin_agent::MAX_CONCURRENT_TURNS` permits; a turn holds one while it runs.
    pub slots: Arc<Semaphore>,
}

impl Turns {
    pub fn new() -> Self {
        Self {
            running: Mutex::new(HashMap::new()),
            live_text: Mutex::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(margin_agent::MAX_CONCURRENT_TURNS)),
        }
    }
}

impl Default for Turns {
    fn default() -> Self {
        Self::new()
    }
}

/// A turn's task.
pub struct RunningTurn {
    pub turn_id: Id,
    pub abort: AbortHandle,
    /// Set while the turn waits on a confirmation card: (card item id, answer).
    pub decision: Option<(Id, oneshot::Sender<Decision>)>,
}

/// How a turn starts.
pub enum Start {
    /// A new user message: stores the `UserMessage` item (with
    /// `margin_agent::user_message_input`), sets `Running`, spawns the task.
    Message {
        text: String,
        source_ids: Vec<Id>,
        device: DeviceRef,
    },
    /// Continue an `Interrupted` or `Failed` turn: same `turn_id`, history from the
    /// stored items.
    Continue,
}

/// Starts a turn of `chat` (which must not be busy). Returns the chat (now `Running`)
/// and the `UserMessage` item for `Start::Message`. Call with `write_lock` held.
pub async fn start_locked(
    state: &AppState,
    chat: &mut Chat,
    start: Start,
) -> AppResult<Option<ChatItem>> {
    let _ = (state, chat, start);
    unimplemented!("service: turns::start_locked (phase 1, service owner)")
}

/// The task body: waits for a slot, builds `TurnInput` (settings: agent model, reasoning
/// effort; history from `margin_agent::replay_input`), runs the turn, then sets
/// `Idle` (finished) or `Failed`, and sends the pending message, if any, as the next
/// turn.
pub async fn run(state: AppState, chat_id: Id, turn_id: Id, origin: DeviceRef) {
    let _ = (state, chat_id, turn_id, origin);
    unimplemented!("service: turns::run (phase 1, service owner)")
}

/// Stop: aborts the task, finishes the turn's `InProgress` items as `Cancelled`, gives
/// every unanswered call a `ToolResult` ("Cancelled", `cancelled_call_output`), marks a
/// pending confirmation `Denied { by_stop: true }`, adds an `Action` "Stopped by the
/// owner.", sets `Idle`, then sends the pending message, if any. 409 "not_running" if
/// the chat isn't busy.
pub async fn stop(state: &AppState, chat_id: &str, by_device: &str) -> AppResult<Chat> {
    let _ = (state, chat_id, by_device);
    unimplemented!("service: turns::stop (phase 1, service owner)")
}

/// Continue an `Interrupted` or `Failed` turn. 409 "cannot_continue" otherwise.
pub async fn resume(state: &AppState, chat_id: &str) -> AppResult<Chat> {
    let _ = (state, chat_id);
    unimplemented!("service: turns::resume (phase 1, service owner)")
}

/// Answers the confirmation card `item_id`: stores the decision on the card, sets the
/// chat `Running`, wakes the waiting turn. 409 "not_waiting" if that card isn't the one
/// the turn waits on.
pub async fn decide(
    state: &AppState,
    chat_id: &str,
    item_id: &str,
    approve: bool,
    by_device: &str,
) -> AppResult<Chat> {
    let _ = (state, chat_id, item_id, approve, by_device);
    unimplemented!("service: turns::decide (phase 2, service owner)")
}

/// Stores a message sent while the chat is busy as `Chat.pending`. 409
/// "already_pending" if there is one.
pub async fn queue_locked(
    state: &AppState,
    chat: &mut Chat,
    pending: PendingMessage,
) -> AppResult<()> {
    let _ = (state, chat, pending);
    unimplemented!("service: turns::queue_locked (phase 1, service owner)")
}

/// Startup: chats left `Running` or `WaitingForUser` become `Interrupted`; their
/// `InProgress` items become `Cancelled`; unanswered calls get a `ToolResult` whose
/// output says the service stopped before the call finished; a pending confirmation
/// becomes `Decision::Expired` with a `ToolResult` saying the service restarted before
/// the owner answered; an `Action` "The service stopped while answering." is added.
/// Pending messages are kept. Called from `server::recover`.
pub async fn recover(state: &AppState) -> anyhow::Result<()> {
    let _ = state;
    unimplemented!("service: turns::recover (phase 1, service owner)")
}

/// `margin_agent::TurnSink` for one turn of one chat.
pub struct ServiceSink {
    pub state: AppState,
    pub chat_id: Id,
    pub turn_id: Id,
}

impl TurnSink for ServiceSink {
    fn add_item(
        &self,
        status: ItemStatus,
        kind: ChatItemKind,
        record: ItemRecord,
    ) -> BoxFuture<'_, Result<ChatItem, SinkError>> {
        let _ = (status, kind, record);
        unimplemented!("service: ServiceSink::add_item (phase 1, service owner)")
    }

    fn delta(&self, item: &ChatItem, text: &str) {
        let _ = (item, text);
        unimplemented!("service: ServiceSink::delta (phase 1, service owner)")
    }

    fn update_item<'a>(
        &'a self,
        item: &'a ChatItem,
        record: ItemRecord,
    ) -> BoxFuture<'a, Result<(), SinkError>> {
        let _ = (item, record);
        unimplemented!("service: ServiceSink::update_item (phase 1, service owner)")
    }

    fn add_usage<'a>(&'a self, usage: &'a Usage) -> BoxFuture<'a, Result<(), SinkError>> {
        let _ = usage;
        unimplemented!("service: ServiceSink::add_usage (phase 1, service owner)")
    }

    fn wait_for_decision<'a>(
        &'a self,
        card: &'a ChatItem,
    ) -> BoxFuture<'a, Result<Decision, SinkError>> {
        let _ = card;
        unimplemented!("service: ServiceSink::wait_for_decision (phase 2, service owner)")
    }
}
