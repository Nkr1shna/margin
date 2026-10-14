//! The loop: one turn of a chat.

use std::sync::Arc;
use std::time::Duration;

use margin_shared::{DeviceRef, Id, ReasoningEffort, Workspace};
use serde_json::Value;

use crate::{
    DeviceBridge, MODEL_RETRIES, ModelClient, RETRY_BASE_DELAY, RETRY_MAX_DELAY, ToolRegistry,
    TurnSink,
};

/// What a turn runs with.
pub struct TurnDeps<'a> {
    pub model: &'a dyn ModelClient,
    pub tools: &'a ToolRegistry,
    pub sink: &'a dyn TurnSink,
    pub devices: Arc<dyn DeviceBridge>,
    pub retry: RetryPolicy,
}

/// How model calls are retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries after the first attempt.
    pub retries: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            retries: MODEL_RETRIES,
            base_delay: RETRY_BASE_DELAY,
            max_delay: RETRY_MAX_DELAY,
        }
    }
}

impl RetryPolicy {
    /// No pauses, for tests.
    pub fn immediate() -> Self {
        Self {
            retries: MODEL_RETRIES,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        }
    }
}

/// One turn (or one Continue of it).
#[derive(Clone)]
pub struct TurnInput {
    pub chat_id: Id,
    pub turn_id: Id,
    /// The device whose message started the turn.
    pub origin_device: DeviceRef,
    /// Settings' agent model; `None` uses the client's model.
    pub model: Option<String>,
    pub reasoning_effort: ReasoningEffort,
    /// `replay_input` of the chat's stored items, ending with this turn's user message
    /// (or with whatever the interrupted/failed run left).
    pub history: Vec<Value>,
    /// The chat's attached folders. Folder tools are allowed only when there are some.
    pub workspaces: Vec<Workspace>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    /// The model answered without calling more tools.
    Finished { steps: u32 },
    /// The turn couldn't go on (retries used up, a non-retryable model error, storage
    /// failure). The loop already added the `ChatItemKind::Error` item; the service
    /// sets `ChatState::Failed { error }`.
    Failed { error: String, retryable: bool },
}

/// Runs a turn to its end. See `docs/agent-contract.md` ("Lifecycle of a turn") for
/// the exact items and events. In short, each step:
/// 1. calls the model with `[system_prompt developer message] + history`, the
///    registry's `definitions()`, `parallel_tool_calls: true`, the reasoning effort with
///    summaries, `include: reasoning.encrypted_content`, `prompt_cache_key = chat_id`,
///    and a `tool_choice` (`allowed_tools` without folder tools when no workspace is
///    attached; `"none"` on step `MAX_STEPS` or once `TURN_OUTPUT_MAX_BYTES` is used);
/// 2. streams the response into items through the sink (deltas while streaming, the
///    verbatim `output_item.done` stored as each item's model item), adds usage;
/// 3. without function calls, finishes; otherwise runs them (Read/Device concurrently,
///    then Write/Destructive in order, parking on a confirmation card where needed),
///    stores each `ToolResult` (+ cards) with its `function_call_output`, appends the
///    outputs to the history and goes on.
///
/// Retryable model errors before any output item of the attempt are retried per
/// `deps.retry`; anything else fails the turn. Cancel-safe: Stop drops the future; the
/// service then finishes whatever is left `InProgress`.
pub async fn run_turn(deps: TurnDeps<'_>, input: TurnInput) -> TurnOutcome {
    let _ = (deps, input);
    unimplemented!("margin-agent: run_turn (phase 1, agent owner)")
}
