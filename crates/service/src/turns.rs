//! The turn runner: starts `margin_agent::run_turn` as a background task per chat,
//! stores what it produces (`ServiceSink`), and handles stop, continue, confirmations,
//! the pending message and recovery. See `docs/agent-contract.md`.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use futures_util::FutureExt;
use margin_agent::{
    BoxFuture, ItemRecord, MessageContext, ReplayItem, RetryPolicy, SinkError, TurnDeps, TurnInput,
    TurnOutcome, TurnSink,
};
use margin_shared::{
    Card, Chat, ChatItem, ChatItemKind, ChatState, Decision, DeviceRef, Id, ItemStatus,
    PendingMessage, Source, Usage,
};
use serde_json::Value;
use tokio::sync::{Semaphore, oneshot};
use tokio::task::AbortHandle;

use crate::chats::{self, StoredChatItem};
use crate::error::{AppError, AppResult};
use crate::state::{AppState, DeviceRow};
use crate::{agent_tools, db, workspaces};

/// The `Action` added when the owner stops a turn.
pub const STOPPED: &str = "Stopped by the owner.";
/// The `Action` added to a turn the service stopped in the middle of.
pub const STOPPED_BY_RESTART: &str = "The service stopped while answering.";
/// What the model is told about a call that was running when the service stopped.
pub const CALL_INTERRUPTED: &str = "The service stopped before this call finished; check whether it took effect before trying again.";
/// What the model is told about a confirmation nobody answered before a restart.
pub const CONFIRMATION_EXPIRED: &str = "The service restarted before the owner answered.";

/// Running turns, one per chat at most. Lives in `App`.
pub struct Turns {
    pub running: Mutex<HashMap<Id, RunningTurn>>,
    /// item id → the streaming text of an `InProgress` item so far, kept by
    /// `ServiceSink::delta` (for delta offsets) and overlaid by `GET /api/chats/{id}`.
    pub live_text: Mutex<HashMap<Id, String>>,
    /// `margin_agent::MAX_CONCURRENT_TURNS` permits; a turn holds one while it runs.
    pub slots: Arc<Semaphore>,
    /// chat id → the last `seq` given to one of its items (loaded from the store on
    /// first use; see `chats::next_seq_locked`).
    pub seqs: Mutex<HashMap<Id, u64>>,
    /// How model calls are retried (tests use `RetryPolicy::immediate`).
    pub retry: Mutex<RetryPolicy>,
}

impl Turns {
    pub fn new() -> Self {
        Self {
            running: Mutex::new(HashMap::new()),
            live_text: Mutex::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(margin_agent::MAX_CONCURRENT_TURNS)),
            seqs: Mutex::new(HashMap::new()),
            retry: Mutex::new(RetryPolicy::default()),
        }
    }

    /// Ends the chat's turn task, if one runs. Its items are left as they are.
    pub fn abort(&self, chat_id: &str) {
        if let Some(turn) = self.running.lock().unwrap().remove(chat_id) {
            turn.abort.abort();
        }
    }

    /// Forgets what is kept in memory for a deleted chat.
    pub fn forget_chat(&self, chat_id: &str) {
        self.seqs.lock().unwrap().remove(chat_id);
    }

    /// Whether `turn_id` is the chat's running turn.
    fn is_current(&self, chat_id: &str, turn_id: &str) -> bool {
        self.running
            .lock()
            .unwrap()
            .get(chat_id)
            .is_some_and(|t| t.turn_id == turn_id)
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
    /// When this run of the turn started (kept for `ChatState::Running` while it waits
    /// on a confirmation).
    pub started_at: DateTime<Utc>,
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
    if chat.state.is_busy() {
        return Err(AppError::conflict(
            "busy",
            "This chat is already answering.",
        ));
    }
    let now = Utc::now();
    match start {
        Start::Message {
            text,
            source_ids,
            device,
        } => {
            // Files deleted since the message was written (a pending one) are left out.
            let mut attached = Vec::new();
            for id in source_ids {
                if state.store.get::<Source>(db::SOURCE, &id).await?.is_some() {
                    attached.push(id);
                }
            }
            for id in &attached {
                if !chat.source_ids.contains(id) {
                    chat.source_ids.push(id.clone());
                }
            }
            let turn_id = crate::state::new_id();
            let ctx = message_context(state, chat, &device, now).await?;
            let model_items = margin_agent::user_message_input(&text, &ctx);
            let title = chats::title_from(&text);
            let item = chats::new_item_locked(
                state,
                &chat.id,
                &turn_id,
                ItemStatus::Done,
                ChatItemKind::UserMessage {
                    text,
                    source_ids: attached,
                    device_id: device.device_id.clone(),
                    device_name: device.name.clone(),
                },
            )
            .await?;
            let stored = StoredChatItem {
                item,
                model_items,
                full_output: None,
            };
            chats::save_item_locked(state, &stored).await?;
            if chat.title.is_empty() {
                chat.title = title;
            }
            chat.state = ChatState::Running {
                turn_id: turn_id.clone(),
                device_id: device.device_id.clone(),
                started_at: now,
            };
            chats::save_chat_locked(state, chat).await?;
            spawn_locked(state, &chat.id, &turn_id, device, now);
            Ok(Some(stored.item))
        }
        Start::Continue => {
            let turn_id = match &chat.state {
                ChatState::Interrupted { turn_id } | ChatState::Failed { turn_id, .. } => {
                    turn_id.clone()
                }
                _ => return Err(cannot_continue()),
            };
            let device = turn_origin(state, &chat.id, &turn_id).await?;
            chat.state = ChatState::Running {
                turn_id: turn_id.clone(),
                device_id: device.device_id.clone(),
                started_at: now,
            };
            chats::save_chat_locked(state, chat).await?;
            spawn_locked(state, &chat.id, &turn_id, device, now);
            Ok(None)
        }
    }
}

fn cannot_continue() -> AppError {
    AppError::conflict(
        "cannot_continue",
        "There's nothing to continue in this chat.",
    )
}

/// What a message is sent with: the date, the sender, the chat's folders (with whether
/// their devices are online) and its files.
async fn message_context(
    state: &AppState,
    chat: &Chat,
    device: &DeviceRef,
    now: DateTime<Utc>,
) -> anyhow::Result<MessageContext> {
    let workspaces = workspaces::by_ids(state, &chat.workspace_ids)
        .await?
        .into_iter()
        .map(|w| {
            let online = state.devices.is_online(&w.device_id);
            (w, online)
        })
        .collect();
    let mut sources = Vec::new();
    for id in &chat.source_ids {
        if let Some(s) = state.store.get::<Source>(db::SOURCE, id).await? {
            sources.push(s);
        }
    }
    Ok(MessageContext {
        now,
        device: device.clone(),
        workspaces,
        sources,
    })
}

/// The device whose message started `turn_id`.
async fn turn_origin(state: &AppState, chat_id: &str, turn_id: &str) -> anyhow::Result<DeviceRef> {
    let items = chats::items(state, chat_id).await?;
    let found = items.iter().find_map(|r| match &r.item.kind {
        ChatItemKind::UserMessage {
            device_id,
            device_name,
            ..
        } if r.item.turn_id == turn_id => Some(DeviceRef {
            device_id: device_id.clone(),
            name: device_name.clone(),
        }),
        _ => None,
    });
    Ok(match found {
        Some(d) => d,
        None => DeviceRef {
            device_id: String::new(),
            name: "Unknown device".into(),
        },
    })
}

/// The device as a chat names it now; a revoked device keeps its id.
async fn device_ref(state: &AppState, device_id: &str) -> anyhow::Result<DeviceRef> {
    let row: Option<DeviceRow> = state.store.get(db::DEVICE, device_id).await?;
    Ok(DeviceRef {
        device_id: device_id.to_string(),
        name: row.map_or_else(|| "A removed device".into(), |r| r.name),
    })
}

/// Spawns `run` and records it as the chat's running turn. Call with `write_lock` held,
/// so the task can't touch the chat before it is recorded.
fn spawn_locked(
    state: &AppState,
    chat_id: &str,
    turn_id: &str,
    origin: DeviceRef,
    started_at: DateTime<Utc>,
) {
    let task = tokio::spawn(run(
        state.clone(),
        chat_id.to_string(),
        turn_id.to_string(),
        origin,
    ));
    state.turns.running.lock().unwrap().insert(
        chat_id.to_string(),
        RunningTurn {
            turn_id: turn_id.to_string(),
            abort: task.abort_handle(),
            decision: None,
            started_at,
        },
    );
}

/// How a run ended before the loop could say so itself.
struct Early {
    message: String,
    retryable: bool,
}

/// The task body: waits for a slot, builds `TurnInput` (settings: agent model, reasoning
/// effort; history from `margin_agent::replay_input`), runs the turn, then sets
/// `Idle` (finished) or `Failed`, and sends the pending message, if any, as the next
/// turn.
pub async fn run(state: AppState, chat_id: Id, turn_id: Id, origin: DeviceRef) {
    let slot = state.turns.slots.clone().acquire_owned().await;
    let result = AssertUnwindSafe(run_turn(&state, &chat_id, &turn_id, origin))
        .catch_unwind()
        .await;
    drop(slot);
    let (outcome, early) = match result {
        Ok(Ok(outcome)) => (outcome, None),
        Ok(Err(early)) => (
            TurnOutcome::Failed {
                error: early.message.clone(),
                retryable: early.retryable,
            },
            Some(early),
        ),
        Err(_) => {
            tracing::error!(chat = %chat_id, turn = %turn_id, "the agent loop panicked");
            let early = Early {
                message: "Something went wrong on the service while answering. Check its log."
                    .into(),
                retryable: true,
            };
            (
                TurnOutcome::Failed {
                    error: early.message.clone(),
                    retryable: true,
                },
                Some(early),
            )
        }
    };
    if let Err(e) = finish(&state, &chat_id, &turn_id, outcome, early).await {
        tracing::error!(chat = %chat_id, turn = %turn_id, "couldn't finish the turn: {e:#}");
    }
}

/// Everything but the bookkeeping: config, history, tools, `margin_agent::run_turn`.
async fn run_turn(
    state: &AppState,
    chat_id: &str,
    turn_id: &str,
    origin: DeviceRef,
) -> Result<TurnOutcome, Early> {
    // A missing key or model ends the turn like a failed model call.
    let ai = state.ai_config().await.map_err(|e| Early {
        message: e.message,
        retryable: true,
    })?;
    let internal = |e: anyhow::Error| {
        tracing::error!(chat = %chat_id, "couldn't start the turn: {e:#}");
        Early {
            message: "The service couldn't load this chat. Check its log.".into(),
            retryable: true,
        }
    };
    let settings = state.settings_row().await.map_err(internal)?;
    let chat: Option<Chat> = state.store.get(db::CHAT, chat_id).await.map_err(internal)?;
    let Some(chat) = chat else {
        return Err(Early {
            message: "This chat was deleted.".into(),
            retryable: false,
        });
    };
    let rows = chats::items(state, chat_id).await.map_err(internal)?;
    let replay: Vec<ReplayItem<'_>> = rows
        .iter()
        .map(|r| ReplayItem {
            item: &r.item,
            model_items: &r.model_items,
        })
        .collect();
    let history = margin_agent::replay_input(&replay);
    let workspaces = workspaces::by_ids(state, &chat.workspace_ids)
        .await
        .map_err(internal)?;
    let tools = agent_tools::registry(state);
    let model = state.generator.agent_model(&ai);
    let sink = ServiceSink {
        state: state.clone(),
        chat_id: chat_id.to_string(),
        turn_id: turn_id.to_string(),
    };
    let retry = *state.turns.retry.lock().unwrap();
    let deps = TurnDeps {
        model: model.as_ref(),
        tools: &tools,
        sink: &sink,
        devices: state.devices.clone(),
        retry,
    };
    let input = TurnInput {
        chat_id: chat_id.to_string(),
        turn_id: turn_id.to_string(),
        origin_device: origin,
        model: settings.agent_model.clone(),
        reasoning_effort: settings.reasoning_effort,
        history,
        workspaces,
    };
    tracing::info!(chat = %chat_id, turn = %turn_id, "turn started");
    let outcome = margin_agent::run_turn(deps, input).await;
    tracing::info!(chat = %chat_id, turn = %turn_id, ?outcome, "turn ended");
    Ok(outcome)
}

/// Ends a run: the Error item if the loop never got to add one, `Idle` or `Failed`,
/// then the pending message.
async fn finish(
    state: &AppState,
    chat_id: &str,
    turn_id: &str,
    outcome: TurnOutcome,
    early: Option<Early>,
) -> anyhow::Result<()> {
    let _w = state.write_lock.lock().await;
    // Stopped or deleted meanwhile: whoever did that finished the turn.
    if !state.turns.is_current(chat_id, turn_id) {
        return Ok(());
    }
    state.turns.running.lock().unwrap().remove(chat_id);
    let Some(mut chat) = state.store.get::<Chat>(db::CHAT, chat_id).await? else {
        return Ok(());
    };
    if let Some(early) = early {
        let item = chats::new_item_locked(
            state,
            chat_id,
            turn_id,
            ItemStatus::Done,
            ChatItemKind::Error {
                message: early.message,
                retryable: early.retryable,
            },
        )
        .await?;
        chats::save_item_locked(
            state,
            &StoredChatItem {
                item,
                model_items: Vec::new(),
                full_output: None,
            },
        )
        .await?;
    }
    chat.state = match outcome {
        TurnOutcome::Finished { .. } => ChatState::Idle,
        TurnOutcome::Failed { error, .. } => ChatState::Failed {
            turn_id: turn_id.to_string(),
            error,
        },
    };
    chats::save_chat_locked(state, &mut chat).await?;
    send_pending_locked(state, &mut chat).await;
    Ok(())
}

/// Starts the chat's pending message, if any, as a new turn (`pending = None` goes out
/// with `Running`). Call with `write_lock` held.
async fn send_pending_locked(state: &AppState, chat: &mut Chat) {
    let Some(pending) = chat.pending.take() else {
        return;
    };
    let started = async {
        let device = device_ref(state, &pending.device_id).await?;
        start_locked(
            state,
            chat,
            Start::Message {
                text: pending.text.clone(),
                source_ids: pending.source_ids.clone(),
                device,
            },
        )
        .await
    };
    if let Err(e) = started.await {
        tracing::error!(chat = %chat.id, "couldn't send the pending message: {}", e.message);
        // Keep it, so the owner sees it and can cancel it.
        chat.pending = Some(pending);
        if let Err(e) = chats::save_chat_locked(state, chat).await {
            tracing::error!(chat = %chat.id, "couldn't keep the pending message: {e:#}");
        }
    }
}

/// Stop: aborts the task, finishes the turn's `InProgress` items as `Cancelled`, gives
/// every unanswered call a `ToolResult` ("Cancelled", `cancelled_call_output`), marks a
/// pending confirmation `Denied { by_stop: true }`, adds an `Action` "Stopped by the
/// owner.", sets `Idle`, then sends the pending message, if any. 409 "not_running" if
/// the chat isn't busy.
pub async fn stop(state: &AppState, chat_id: &str, by_device: &str) -> AppResult<Chat> {
    let _w = state.write_lock.lock().await;
    let mut chat = chats::chat(state, chat_id).await?;
    let Some(turn_id) = chat
        .state
        .is_busy()
        .then(|| chat.state.turn_id().map(str::to_string))
        .flatten()
    else {
        return Err(AppError::conflict(
            "not_running",
            "Nothing is being answered in this chat.",
        ));
    };
    state.turns.abort(chat_id);
    wind_up_locked(
        state,
        chat_id,
        &turn_id,
        Ending::Stop {
            by_device: by_device.to_string(),
        },
    )
    .await?;
    chat.state = ChatState::Idle;
    chats::save_chat_locked(state, &mut chat).await?;
    send_pending_locked(state, &mut chat).await;
    Ok(chat)
}

/// Continue an `Interrupted` or `Failed` turn. 409 "cannot_continue" otherwise.
pub async fn resume(state: &AppState, chat_id: &str) -> AppResult<Chat> {
    let _w = state.write_lock.lock().await;
    let mut chat = chats::chat(state, chat_id).await?;
    if !chat.state.can_continue() {
        return Err(cannot_continue());
    }
    start_locked(state, &mut chat, Start::Continue).await?;
    Ok(chat)
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
    let _w = state.write_lock.lock().await;
    let mut chat = chats::chat(state, chat_id).await?;
    let not_waiting = || {
        AppError::conflict(
            "not_waiting",
            "That question was already answered, or the answer stopped.",
        )
    };
    let (turn_id, device_id) = match &chat.state {
        ChatState::WaitingForUser {
            turn_id,
            device_id,
            item_id: waiting,
        } if waiting == item_id => (turn_id.clone(), device_id.clone()),
        _ => return Err(not_waiting()),
    };
    let (answer, started_at) = {
        let mut running = state.turns.running.lock().unwrap();
        let Some(turn) = running.get_mut(chat_id).filter(|t| t.turn_id == turn_id) else {
            return Err(not_waiting());
        };
        match turn.decision.take() {
            Some((card, answer)) if card == item_id => (answer, turn.started_at),
            other => {
                turn.decision = other;
                return Err(not_waiting());
            }
        }
    };
    let decision = if approve {
        Decision::Approved {
            device_id: by_device.to_string(),
        }
    } else {
        Decision::Denied {
            device_id: by_device.to_string(),
            by_stop: false,
        }
    };
    if let Some(mut row) = state
        .store
        .get::<StoredChatItem>(db::CHAT_ITEM, item_id)
        .await?
        && set_decision(&mut row.item, decision.clone())
    {
        row.item.updated_at = Utc::now();
        chats::save_item_locked(state, &row).await?;
    }
    chat.state = ChatState::Running {
        turn_id,
        device_id,
        started_at,
    };
    chats::save_chat_locked(state, &mut chat).await?;
    let _ = answer.send(decision);
    Ok(chat)
}

/// Sets the decision of a confirmation card; false if `item` isn't one.
fn set_decision(item: &mut ChatItem, decision: Decision) -> bool {
    match &mut item.kind {
        ChatItemKind::Card {
            card: Card::Confirmation(card),
            ..
        } => {
            card.decision = decision;
            true
        }
        _ => false,
    }
}

/// Stores a message sent while the chat is busy as `Chat.pending`. 409
/// "already_pending" if there is one.
pub async fn queue_locked(
    state: &AppState,
    chat: &mut Chat,
    pending: PendingMessage,
) -> AppResult<()> {
    if chat.pending.is_some() {
        return Err(AppError::conflict(
            "already_pending",
            "A message is already waiting to be sent. Cancel it to send another.",
        ));
    }
    chat.pending = Some(pending);
    chats::save_chat_locked(state, chat).await?;
    Ok(())
}

/// Startup: chats left `Running` or `WaitingForUser` become `Interrupted`; their
/// `InProgress` items become `Cancelled`; unanswered calls get a `ToolResult` whose
/// output says the service stopped before the call finished; a pending confirmation
/// becomes `Decision::Expired` with a `ToolResult` saying the service restarted before
/// the owner answered; an `Action` "The service stopped while answering." is added.
/// Pending messages are kept. Called from `server::recover` (which holds `write_lock`).
pub async fn recover(state: &AppState) -> anyhow::Result<()> {
    let chats: Vec<Chat> = state.store.list(db::CHAT).await?;
    for mut chat in chats {
        if !chat.state.is_busy() {
            continue;
        }
        let Some(turn_id) = chat.state.turn_id().map(str::to_string) else {
            continue;
        };
        wind_up_locked(state, &chat.id, &turn_id, Ending::Restart).await?;
        chat.state = ChatState::Interrupted { turn_id };
        chats::save_chat_locked(state, &mut chat).await?;
        tracing::info!(chat = %chat.id, "marked an interrupted chat turn");
    }
    Ok(())
}

/// Why a busy turn is being wound up.
enum Ending {
    Stop { by_device: String },
    Restart,
}

/// Finishes what a stopped (or interrupted) turn left behind, in this order: its
/// `InProgress` items become `Cancelled` (keeping the text streamed so far); every
/// unanswered call gets a failed `ToolResult`; a pending confirmation is denied (Stop)
/// or expired (restart); then the `Action` saying what happened. Call with `write_lock`
/// held, after the task is gone.
async fn wind_up_locked(
    state: &AppState,
    chat_id: &str,
    turn_id: &str,
    ending: Ending,
) -> anyhow::Result<()> {
    let rows = chats::items(state, chat_id).await?;
    let now = Utc::now();

    for row in &rows {
        if row.item.turn_id != turn_id || row.item.status != ItemStatus::InProgress {
            continue;
        }
        let mut row = row.clone();
        let live = state.turns.live_text.lock().unwrap().remove(&row.item.id);
        if let Some(text) = live {
            set_streaming_text(&mut row.item.kind, text);
        }
        row.item.status = ItemStatus::Cancelled;
        row.item.updated_at = now;
        chats::save_item_locked(state, &row).await?;
    }

    // Calls waiting on a confirmation card that nobody answered.
    let pending_cards: Vec<&StoredChatItem> = rows
        .iter()
        .filter(|r| {
            matches!(
                &r.item.kind,
                ChatItemKind::Card { card: Card::Confirmation(c), .. } if c.decision == Decision::Pending
            )
        })
        .collect();
    let asked: Vec<&str> = pending_cards
        .iter()
        .filter_map(|r| match &r.item.kind {
            ChatItemKind::Card { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();

    let replay: Vec<ReplayItem<'_>> = rows
        .iter()
        .map(|r| ReplayItem {
            item: &r.item,
            model_items: &r.model_items,
        })
        .collect();
    for (call_id, call) in margin_agent::unanswered_calls(&replay) {
        let (model_item, summary) = match &ending {
            Ending::Stop { .. } => (margin_agent::cancelled_call_output(&call_id), "Cancelled"),
            Ending::Restart => {
                let why = if asked.contains(&call_id.as_str()) {
                    CONFIRMATION_EXPIRED
                } else {
                    CALL_INTERRUPTED
                };
                (
                    margin_agent::function_call_output(&call_id, &margin_agent::error_output(why)),
                    "Interrupted",
                )
            }
        };
        let output = model_item
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let item = chats::new_item_locked(
            state,
            chat_id,
            &call.turn_id,
            ItemStatus::Done,
            ChatItemKind::ToolResult {
                call_id: call_id.clone(),
                ok: false,
                summary: summary.into(),
                output_bytes: output.len() as u64,
            },
        )
        .await?;
        chats::save_item_locked(
            state,
            &StoredChatItem {
                item,
                model_items: vec![model_item],
                full_output: Some(output),
            },
        )
        .await?;
    }

    for card in pending_cards {
        let mut row = card.clone();
        let decision = match &ending {
            Ending::Stop { by_device } => Decision::Denied {
                device_id: by_device.clone(),
                by_stop: true,
            },
            Ending::Restart => Decision::Expired,
        };
        set_decision(&mut row.item, decision);
        row.item.updated_at = now;
        chats::save_item_locked(state, &row).await?;
    }

    let text = match ending {
        Ending::Stop { .. } => STOPPED,
        Ending::Restart => STOPPED_BY_RESTART,
    };
    let item = chats::new_item_locked(
        state,
        chat_id,
        turn_id,
        ItemStatus::Done,
        ChatItemKind::Action { text: text.into() },
    )
    .await?;
    chats::save_item_locked(
        state,
        &StoredChatItem {
            item,
            model_items: vec![margin_agent::action_input(text)],
            full_output: None,
        },
    )
    .await
}

/// Replaces the text deltas append to (`AssistantText.text`, `Reasoning.summary`,
/// `ToolCall.arguments`).
fn set_streaming_text(kind: &mut ChatItemKind, text: String) {
    match kind {
        ChatItemKind::AssistantText { text: t }
        | ChatItemKind::Reasoning { summary: t }
        | ChatItemKind::ToolCall { arguments: t, .. } => *t = text,
        _ => {}
    }
}

/// `margin_agent::TurnSink` for one turn of one chat.
pub struct ServiceSink {
    pub state: AppState,
    pub chat_id: Id,
    pub turn_id: Id,
}

impl ServiceSink {
    fn stopped(&self) -> SinkError {
        SinkError("This answer was stopped.".into())
    }

    fn storage(e: impl std::fmt::Display) -> SinkError {
        tracing::error!("couldn't store a chat item: {e:#}");
        SinkError("The service couldn't save the answer. Check its log.".into())
    }
}

impl TurnSink for ServiceSink {
    fn add_item(
        &self,
        status: ItemStatus,
        kind: ChatItemKind,
        record: ItemRecord,
    ) -> BoxFuture<'_, Result<ChatItem, SinkError>> {
        Box::pin(async move {
            let state = &self.state;
            let _w = state.write_lock.lock().await;
            if !state.turns.is_current(&self.chat_id, &self.turn_id) {
                return Err(self.stopped());
            }
            let Some(mut chat) = state
                .store
                .get::<Chat>(db::CHAT, &self.chat_id)
                .await
                .map_err(Self::storage)?
            else {
                return Err(self.stopped());
            };
            let item = chats::new_item_locked(state, &self.chat_id, &self.turn_id, status, kind)
                .await
                .map_err(Self::storage)?;
            if item.status == ItemStatus::InProgress
                && let Some(text) = item.kind.streaming_text()
            {
                state
                    .turns
                    .live_text
                    .lock()
                    .unwrap()
                    .insert(item.id.clone(), text.to_string());
            }
            let stored = StoredChatItem {
                item,
                model_items: record.model_items.unwrap_or_default(),
                full_output: record.full_output,
            };
            chats::save_item_locked(state, &stored)
                .await
                .map_err(Self::storage)?;
            chats::save_chat_locked(state, &mut chat)
                .await
                .map_err(Self::storage)?;
            Ok(stored.item)
        })
    }

    fn delta(&self, item: &ChatItem, text: &str) {
        if text.is_empty() {
            return;
        }
        let offset = {
            let mut live = self.state.turns.live_text.lock().unwrap();
            let so_far = live.entry(item.id.clone()).or_insert_with(|| {
                item.kind
                    .streaming_text()
                    .map(str::to_string)
                    .unwrap_or_default()
            });
            let offset = so_far.len() as u64;
            so_far.push_str(text);
            offset
        };
        self.state.emit(margin_shared::Event::ChatItemDelta {
            chat_id: self.chat_id.clone(),
            item_id: item.id.clone(),
            offset,
            text: text.to_string(),
        });
    }

    fn update_item<'a>(
        &'a self,
        item: &'a ChatItem,
        record: ItemRecord,
    ) -> BoxFuture<'a, Result<(), SinkError>> {
        Box::pin(async move {
            let state = &self.state;
            let _w = state.write_lock.lock().await;
            if !state.turns.is_current(&self.chat_id, &self.turn_id) {
                return Err(self.stopped());
            }
            let Some(mut row) = state
                .store
                .get::<StoredChatItem>(db::CHAT_ITEM, &item.id)
                .await
                .map_err(Self::storage)?
            else {
                return Err(self.stopped());
            };
            row.item = item.clone();
            row.item.updated_at = Utc::now();
            if let Some(model_items) = record.model_items {
                row.model_items = model_items;
            }
            if let Some(full_output) = record.full_output {
                row.full_output = Some(full_output);
            }
            {
                let mut live = state.turns.live_text.lock().unwrap();
                match (row.item.status, row.item.kind.streaming_text()) {
                    (ItemStatus::InProgress, Some(text)) => {
                        live.insert(row.item.id.clone(), text.to_string());
                    }
                    _ => {
                        live.remove(&row.item.id);
                    }
                }
            }
            chats::save_item_locked(state, &row)
                .await
                .map_err(Self::storage)
        })
    }

    fn add_usage<'a>(&'a self, usage: &'a Usage) -> BoxFuture<'a, Result<(), SinkError>> {
        Box::pin(async move {
            let state = &self.state;
            let _w = state.write_lock.lock().await;
            let Some(mut chat) = state
                .store
                .get::<Chat>(db::CHAT, &self.chat_id)
                .await
                .map_err(Self::storage)?
            else {
                return Err(self.stopped());
            };
            chat.usage.add(usage);
            chats::save_chat_locked(state, &mut chat)
                .await
                .map_err(Self::storage)
        })
    }

    fn wait_for_decision<'a>(
        &'a self,
        card: &'a ChatItem,
    ) -> BoxFuture<'a, Result<Decision, SinkError>> {
        Box::pin(async move {
            let state = &self.state;
            let answer = {
                let _w = state.write_lock.lock().await;
                let Some(mut chat) = state
                    .store
                    .get::<Chat>(db::CHAT, &self.chat_id)
                    .await
                    .map_err(Self::storage)?
                else {
                    return Err(self.stopped());
                };
                let device_id = match &chat.state {
                    ChatState::Running { device_id, .. }
                    | ChatState::WaitingForUser { device_id, .. } => device_id.clone(),
                    _ => return Err(self.stopped()),
                };
                let (tx, rx) = oneshot::channel();
                {
                    let mut running = state.turns.running.lock().unwrap();
                    let Some(turn) = running
                        .get_mut(&self.chat_id)
                        .filter(|t| t.turn_id == self.turn_id)
                    else {
                        return Err(self.stopped());
                    };
                    turn.decision = Some((card.id.clone(), tx));
                }
                chat.state = ChatState::WaitingForUser {
                    turn_id: self.turn_id.clone(),
                    device_id,
                    item_id: card.id.clone(),
                };
                chats::save_chat_locked(state, &mut chat)
                    .await
                    .map_err(Self::storage)?;
                rx
            };
            answer.await.map_err(|_| self.stopped())
        })
    }
}
