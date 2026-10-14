//! Device tools: folder reads and app control, run on a device over its events socket.

use std::sync::Arc;
use std::time::Duration;

use margin_shared::{DeviceToolCall, DeviceToolOutput, Id};

use crate::{BoxFuture, Tool};

/// One device tool call for the [`DeviceBridge`].
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceCall {
    pub chat_id: Id,
    pub call_id: String,
    pub call: DeviceToolCall,
    /// How long the device has (`FOLDER_TOOL_TIMEOUT`, `SEARCH_TEXT_TIMEOUT` or
    /// `APP_CONTROL_TIMEOUT`).
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeviceError {
    /// The device has no events socket open.
    #[error("the device isn't connected")]
    Offline,
    /// No answer before the deadline (a `ToolCancel` was sent).
    #[error("the device didn't answer in time")]
    TimedOut,
    /// The device answered with an error; told to the model as is.
    #[error("{0}")]
    Failed(String),
}

/// Sends tool requests to devices. The service's `DeviceHub` implements it over the
/// events sockets.
pub trait DeviceBridge: Send + Sync + 'static {
    fn is_online(&self, device_id: &str) -> bool;

    /// Sends `DirectedFrame::ToolRequest` to `device_id` and waits for its
    /// `DeviceFrame::ToolResult`, at most `call.timeout`. Dropping the future (Stop)
    /// sends `DirectedFrame::ToolCancel`, as does a timeout. A socket that closes
    /// while the call waits is `DeviceError::Offline`.
    fn call<'a>(
        &'a self,
        device_id: &'a str,
        call: DeviceCall,
    ) -> BoxFuture<'a, Result<DeviceToolOutput, DeviceError>>;
}

/// A bridge to no devices: every device is offline. For tests and for running tools
/// that never reach a device.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoDevices;

impl DeviceBridge for NoDevices {
    fn is_online(&self, _device_id: &str) -> bool {
        false
    }

    fn call<'a>(
        &'a self,
        _device_id: &'a str,
        _call: DeviceCall,
    ) -> BoxFuture<'a, Result<DeviceToolOutput, DeviceError>> {
        Box::pin(async { Err(DeviceError::Offline) })
    }
}

/// The device tools, all `ToolKind::Device`:
/// - folders (phase 4, to the device that registered the workspace, which must be one
///   of `ToolContext.workspaces`): `list_files`, `find_files`, `search_text`,
///   `read_file`;
/// - app control (phase 3, to `ToolContext.origin_device`): `navigate`,
///   `open_citation`, `show_passage`, `toggle_sidebar`.
///
/// Offline or timed-out devices give an error output naming the device and what to do,
/// e.g. "iPhone isn't connected to Margin right now (the app must be open to read the
/// folder)."
pub fn device_tools() -> Vec<Arc<dyn Tool>> {
    unimplemented!("margin-agent: device tools (phases 3 and 4, agent owner)")
}

/// What the model reads for a device's answer: a compact text rendering (one entry or
/// match per line, file text with line numbers).
pub fn device_output_text(output: &DeviceToolOutput) -> String {
    let _ = output;
    unimplemented!("margin-agent: device_output_text (phase 3, agent owner)")
}
