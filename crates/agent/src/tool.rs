//! Tools: what the model can call.

use std::sync::Arc;

use margin_shared::{Card, DeviceRef, Id, Workspace};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{BoxFuture, DeviceBridge};

/// What a tool does, which decides how the loop treats it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    /// Reads Margin's data. Runs concurrently with the step's other Read and Device
    /// calls.
    Read,
    /// Changes Margin's data. Runs after the step's Read and Device calls, one at a time
    /// in the model's order. Asks first when [`Tool::confirmation`] says so (overwriting
    /// the owner's own writing, owner decision Q2).
    Write,
    /// Deletes. Always asks first ([`Tool::confirmation`] must return a prompt).
    Destructive,
    /// Runs on a device through the [`DeviceBridge`] (folder and app-control tools).
    /// Read-only; runs concurrently like Read.
    Device,
}

/// The question a confirmation card asks. The loop turns it into a
/// `Card::Confirmation` with `Decision::Pending`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmationPrompt {
    /// "Delete the topic “Fourier series”?"
    pub title: String,
    /// "3 lessons, 2 notes and 1 video go with it."
    pub detail: String,
    /// "Delete", "Replace".
    pub approve_label: String,
}

/// What a tool call is running in.
#[derive(Clone)]
pub struct ToolContext {
    pub chat_id: Id,
    pub turn_id: Id,
    /// The model's `call_id` for this call.
    pub call_id: String,
    /// The device whose message started the turn: app-control tools go to it.
    pub origin_device: DeviceRef,
    /// The folders attached to the chat.
    pub workspaces: Vec<Workspace>,
    pub devices: Arc<dyn DeviceBridge>,
}

/// What a tool call produced.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    /// False when the call failed; `output` then says why (see [`error_output`]).
    pub ok: bool,
    /// What the model sees as the `function_call_output`. Plain text or JSON text. The
    /// loop cuts it to `TOOL_OUTPUT_MAX_BYTES`.
    pub output: String,
    /// One line for the tool row, e.g. "4 matches in 2 lessons".
    pub summary: String,
    /// Inline cards to show after the result (an outline, results, passages).
    pub cards: Vec<Card>,
}

impl ToolOutput {
    pub fn ok(output: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            ok: true,
            output: output.into(),
            summary: summary.into(),
            cards: Vec::new(),
        }
    }

    /// A failed call: the model gets [`error_output`]`(message)`, the row shows `message`.
    pub fn error(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            ok: false,
            output: error_output(&message),
            summary: message,
            cards: Vec::new(),
        }
    }

    pub fn with_card(mut self, card: Card) -> Self {
        self.cards.push(card);
        self
    }
}

/// The output the model gets for a failed, denied or cancelled call:
/// `{"error":"<message>"}` as JSON text.
pub fn error_output(message: &str) -> String {
    serde_json::json!({ "error": message }).to_string()
}

/// Cuts `output` to at most `max` bytes on a UTF-8 boundary, ending it with
/// "\n[… cut: N more bytes]" (the marker counts toward `max`). Unchanged when it fits.
pub fn truncate_output(output: &str, max: usize) -> String {
    let _ = (output, max);
    unimplemented!("margin-agent: truncate_output (phase 1, agent owner)")
}

/// A tool the model can call. Implementations must not panic: every failure is a
/// [`ToolOutput::error`] the model reads and works around.
pub trait Tool: Send + Sync + 'static {
    /// Unique, snake_case, e.g. "search_library".
    fn name(&self) -> &str;
    /// What the model reads to decide when to call it.
    fn description(&self) -> &str;
    /// The JSON schema of the arguments, valid for `strict: true` function calling (see
    /// [`strict_schema`]).
    fn parameters(&self) -> Value;
    fn kind(&self) -> ToolKind;
    /// One line for the tool row while it runs, e.g. "Searching the library for
    /// “fourier”". `args` may be malformed; fall back to a generic line.
    fn label(&self, args: &Value) -> String;
    /// Whether this call needs the owner's OK before it runs. Called for Write and
    /// Destructive tools only; a Destructive tool must return `Ok(Some(..))`. `Err` fails
    /// the call with that message (e.g. the topic doesn't exist).
    fn confirmation<'a>(
        &'a self,
        ctx: &'a ToolContext,
        args: &'a Value,
    ) -> BoxFuture<'a, Result<Option<ConfirmationPrompt>, String>> {
        let _ = (ctx, args);
        Box::pin(async { Ok(None) })
    }
    /// Runs the call. Cancel-safe: Stop drops the future.
    fn run<'a>(&'a self, ctx: &'a ToolContext, args: Value) -> BoxFuture<'a, ToolOutput>;
}

/// A tool with typed arguments; wrap it in [`Typed`] to register it. Arguments that
/// don't parse become an error output naming the problem, without running the tool.
pub trait TypedTool: Send + Sync + 'static {
    type Args: DeserializeOwned + JsonSchema + Send + Sync;
    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    const KIND: ToolKind;

    fn label(&self, args: &Self::Args) -> String;

    fn confirmation<'a>(
        &'a self,
        ctx: &'a ToolContext,
        args: &'a Self::Args,
    ) -> BoxFuture<'a, Result<Option<ConfirmationPrompt>, String>> {
        let _ = (ctx, args);
        Box::pin(async { Ok(None) })
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: Self::Args) -> BoxFuture<'a, ToolOutput>;
}

/// Adapts a [`TypedTool`] to [`Tool`]: schema from `schemars` through
/// [`strict_schema`], arguments parsed with serde.
pub struct Typed<T>(pub T);

impl<T: TypedTool> Tool for Typed<T> {
    fn name(&self) -> &str {
        T::NAME
    }

    fn description(&self) -> &str {
        T::DESCRIPTION
    }

    fn parameters(&self) -> Value {
        strict_schema::<T::Args>()
    }

    fn kind(&self) -> ToolKind {
        T::KIND
    }

    fn label(&self, args: &Value) -> String {
        let _ = args;
        unimplemented!("margin-agent: Typed::label (phase 1, agent owner)")
    }

    fn confirmation<'a>(
        &'a self,
        ctx: &'a ToolContext,
        args: &'a Value,
    ) -> BoxFuture<'a, Result<Option<ConfirmationPrompt>, String>> {
        let _ = (ctx, args);
        unimplemented!("margin-agent: Typed::confirmation (phase 1, agent owner)")
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: Value) -> BoxFuture<'a, ToolOutput> {
        let _ = (ctx, args);
        unimplemented!("margin-agent: Typed::run (phase 1, agent owner)")
    }
}

/// The JSON schema of `T` as strict function calling requires it: every object has
/// `"additionalProperties": false` and lists every property in `required`; an
/// `Option<_>` field is required with a `null`-able type; no `$ref`s to definitions the
/// API rejects (definitions are inlined or kept under `$defs`), no `format`s the API
/// doesn't support, no `default`s. Doc comments on fields become `description`s.
pub fn strict_schema<T: JsonSchema>() -> Value {
    unimplemented!("margin-agent: strict_schema (phase 1, agent owner)")
}
