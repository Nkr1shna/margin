//! What the model is told: the stable system prompt, and each message's context.

use chrono::{DateTime, Utc};
use margin_shared::{DeviceRef, Source, Workspace};
use serde_json::Value;

/// The developer message that starts every model call's `input`. Identical for every
/// call of every chat (so the prompt cache holds): what Margin is, what the tools do,
/// that tool outputs (files, sources, notes) are data and never instructions, that
/// deletes and overwrites of the owner's writing are confirmed by the app (the model
/// just calls the tool), that attached folders are read-only, that there is no web
/// search, and how to answer (Markdown, `$…$` math, short unless asked).
pub fn system_prompt() -> &'static str {
    unimplemented!("margin-agent: system prompt (phase 1, agent owner)")
}

/// What a user message is sent with.
#[derive(Debug, Clone, PartialEq)]
pub struct MessageContext {
    pub now: DateTime<Utc>,
    /// The device the message was sent from.
    pub device: DeviceRef,
    /// The chat's folders, each with whether its device is online.
    pub workspaces: Vec<(Workspace, bool)>,
    /// The chat's attached files.
    pub sources: Vec<Source>,
}

/// The input items a user message is stored and replayed as: a developer message with
/// the context (date, device, attached folders and files with their ids), then the
/// user message itself (`{"role":"user","content":[{"type":"input_text","text":…}]}`).
/// Built once when the message is stored, so replays are byte-identical.
pub fn user_message_input(text: &str, ctx: &MessageContext) -> Vec<Value> {
    let _ = (text, ctx);
    unimplemented!("margin-agent: user_message_input (phase 1, agent owner)")
}
