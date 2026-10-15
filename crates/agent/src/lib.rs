//! Margin's agent: the loop that answers a chat message by calling the model, running
//! tools and calling the model again until it answers, on the Responses API with
//! `store: false` and full replay.
//!
//! This crate knows nothing about storage, HTTP or the library. The service plugs in:
//! - a [`ModelClient`] (`margin_ai::ResponsesClient` in production, [`ScriptedModel`] in
//!   tests);
//! - a [`ToolRegistry`] of [`Tool`]s (the library, course and source tools live in the
//!   service, which owns the data; the device tools live here and forward through a
//!   [`DeviceBridge`]);
//! - a [`TurnSink`] that stores each item and broadcasts it.
//!
//! The lifecycle of a turn (which items and events, in which order) is specified in
//! `docs/agent-contract.md`; [`run_turn`] implements it.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

mod device;
mod model;
mod prompt;
mod registry;
pub mod replay;
pub mod scripted;
mod sink;
mod tool;
mod turn;

pub use device::{
    APP_CONTROL_TOOLS, DeviceBridge, DeviceCall, DeviceError, FOLDER_TOOLS, NoDevices,
    device_output_text, device_tools,
};
pub use model::ModelClient;
pub use prompt::{AGENT_PROMPT_VERSION, MessageContext, system_prompt, user_message_input};
pub use registry::ToolRegistry;
pub use replay::{
    CONDENSED_HISTORY_PREFIX, ReplayItem, TOOL_PICTURES_PREFIX, action_input,
    cancelled_call_output, function_call_output, replay_input, tool_pictures_input,
    unanswered_calls,
};
pub use scripted::{Recorded, Scripted, ScriptedModel};
pub use sink::{ItemRecord, SinkError, TurnSink};
pub use tool::{
    ConfirmationPrompt, Tool, ToolContext, ToolKind, ToolOutput, Typed, TypedTool,
    check_strict_schema, error_output, strict_schema, truncate_output,
};
pub use turn::{
    RetryPolicy, SUMMARY_INSTRUCTIONS, SUMMARY_PROMPT_VERSION, TurnDeps, TurnInput, TurnOutcome,
    run_turn,
};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// Most model calls in one run of [`run_turn`] (a turn, or one Continue of it). The last
/// allowed call is made with `tool_choice: "none"` so the model answers instead of
/// calling more tools.
pub const MAX_STEPS: u32 = 40;
/// Most bytes of one tool output the model sees; longer outputs are cut (see
/// [`truncate_output`]).
pub const TOOL_OUTPUT_MAX_BYTES: usize = 64 * 1024;
/// Most bytes of tool output in one turn. Once used up, further calls get an error
/// output saying so and the next model call is made with `tool_choice: "none"`.
pub const TURN_OUTPUT_MAX_BYTES: usize = 512 * 1024;
/// Model calls retried after a retryable error (429, 5xx, network) before the turn
/// fails.
pub const MODEL_RETRIES: u32 = 3;
/// First pause before a retry; doubles each time, unless the API sent `Retry-After`.
pub const RETRY_BASE_DELAY: Duration = Duration::from_secs(2);
/// Longest computed backoff before a retry. A `Retry-After` from the API is honoured as
/// asked, however long (owner decision); the chat shows it is waiting.
pub const RETRY_MAX_DELAY: Duration = Duration::from_secs(60);
/// Turns running at once across all chats; a further turn waits (its chat shows
/// Running) until one ends.
pub const MAX_CONCURRENT_TURNS: usize = 2;
/// How long a device has to answer `sync_folder` (walk the folder, send the manifest
/// and get the service's plan; the uploads go on afterwards).
pub const FOLDER_TOOL_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a device has to answer an app-control tool.
pub const APP_CONTROL_TIMEOUT: Duration = Duration::from_secs(20);
/// Server-side compaction threshold, in input tokens (phase 5).
pub const COMPACT_THRESHOLD_TOKENS: u32 = 200_000;
