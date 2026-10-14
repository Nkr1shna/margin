//! Device tools: syncing a folder and app control, run on a device over its events
//! socket. A folder's files are read on the service (they are sources there); the only
//! folder call that reaches a device is `sync_folder`.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use margin_shared::{
    DeviceRef, DeviceToolCall, DeviceToolOutput, Id, LessonView, MAX_LISTED_ENTRIES, NavTarget,
    Workspace,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{
    APP_CONTROL_TIMEOUT, BoxFuture, FOLDER_TOOL_TIMEOUT, Tool, ToolContext, ToolKind, ToolOutput,
    Typed, TypedTool,
};

/// One device tool call for the [`DeviceBridge`].
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceCall {
    pub chat_id: Id,
    pub call_id: String,
    pub call: DeviceToolCall,
    /// How long the device has (`FOLDER_TOOL_TIMEOUT` or `APP_CONTROL_TIMEOUT`).
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

/// The folder tools: offered only while the chat has a folder attached. `list_files`
/// and `find_files` are the service's (they read what it stores); `sync_folder` is the
/// one device tool here. `search`, `read_passage`, `document_outline` and
/// `view_picture` reach folders too but aren't gated: they read sources as well.
pub const FOLDER_TOOLS: [&str; 3] = ["list_files", "find_files", "sync_folder"];
/// The app-control tools: offered only while the device that sent the message is
/// connected.
pub const APP_CONTROL_TOOLS: [&str; 4] = [
    "navigate",
    "open_citation",
    "show_passage",
    "toggle_sidebar",
];

/// Extra time the loop allows past a device call's deadline before giving up on the
/// bridge itself (which should have answered `TimedOut` by then).
const BRIDGE_GRACE: Duration = Duration::from_secs(5);

/// The device tools, all `ToolKind::Device`:
/// - folders (phase 4, to the device that holds the workspace, which must be one of
///   `ToolContext.workspaces`): `sync_folder`;
/// - app control (phase 3, to `ToolContext.origin_device`): `navigate`,
///   `open_citation`, `show_passage`, `toggle_sidebar`.
///
/// Offline or timed-out devices give an error output naming the device and what to do,
/// e.g. "MacBook Pro isn't connected to Margin right now (the app must be open to send
/// the folder's files)."
pub fn device_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(Typed(SyncFolder)),
        Arc::new(Typed(Navigate)),
        Arc::new(Typed(OpenCitation)),
        Arc::new(Typed(ShowPassage)),
        Arc::new(Typed(ToggleSidebar)),
    ]
}

/// What the model reads for a device's answer: a compact text rendering (one entry,
/// match or file per line, file text with line numbers). `Files`, `Matches` and
/// `FileText` answer the old device folder calls, which no tool sends any more; they are
/// still rendered so a stray answer reads sensibly.
pub fn device_output_text(output: &DeviceToolOutput) -> String {
    match output {
        DeviceToolOutput::Files { entries, truncated } => {
            let mut lines: Vec<String> = entries
                .iter()
                .map(|e| {
                    if e.is_dir {
                        format!("{}/", e.path.trim_end_matches('/'))
                    } else {
                        let mut about = size_text(e.bytes);
                        if let Some(modified) = e.modified {
                            about.push_str(&format!(", modified {}", modified.format("%Y-%m-%d")));
                        }
                        if e.binary {
                            about.push_str(", not readable as text");
                        }
                        format!("{}  ({about})", e.path)
                    }
                })
                .collect();
            if lines.is_empty() {
                lines.push("No entries.".into());
            }
            if *truncated {
                lines.push(format!(
                    "[More than {} entries; only these are listed. Narrow the path or the pattern.]",
                    entries.len()
                ));
            }
            lines.join("\n")
        }
        DeviceToolOutput::Matches { matches, truncated } => {
            let mut lines: Vec<String> = matches
                .iter()
                .map(|m| format!("{}:{}: {}", m.path, m.line, m.text))
                .collect();
            if lines.is_empty() {
                lines.push("No matches.".into());
            }
            if *truncated {
                lines.push(
                    "[The search stopped early (too many matches, or too much to read). Narrow the path, query or pattern.]"
                        .into(),
                );
            }
            lines.join("\n")
        }
        DeviceToolOutput::FileText {
            path,
            start_line,
            end_line,
            total_lines,
            text,
        } => {
            let mut out = format!("{path}, lines {start_line}–{end_line} of {total_lines}:\n");
            let width = end_line.to_string().len();
            if text.is_empty() {
                out.push_str("(empty)");
            } else {
                let numbered: Vec<String> = text
                    .lines()
                    .enumerate()
                    .map(|(i, line)| format!("{:>width$}  {line}", *start_line as usize + i))
                    .collect();
                out.push_str(&numbered.join("\n"));
            }
            if end_line < total_lines {
                out.push_str(&format!(
                    "\n[Lines {}–{total_lines} not shown; read on from line {}.]",
                    end_line + 1,
                    end_line + 1
                ));
            }
            out
        }
        DeviceToolOutput::Done { message } => message.clone(),
        DeviceToolOutput::Manifest { files } => {
            if files.is_empty() {
                return "No files.".into();
            }
            let mut lines = vec![match files.len() {
                1 => "1 file:".to_string(),
                n => format!("{n} files:"),
            }];
            lines.extend(files.iter().take(MAX_LISTED_ENTRIES).map(|f| {
                let kind = serde_json::to_value(f.kind)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default();
                let hash: String = f.stamp.sha256.chars().take(12).collect();
                format!(
                    "{}  ({}, {kind}, sha256 {hash})",
                    f.path,
                    size_text(f.stamp.bytes)
                )
            }));
            if files.len() > MAX_LISTED_ENTRIES {
                lines.push(format!(
                    "[{} more files not listed.]",
                    files.len() - MAX_LISTED_ENTRIES
                ));
            }
            lines.join("\n")
        }
    }
}

/// The tool row's line for a device's answer.
fn output_summary(output: &DeviceToolOutput) -> String {
    match output {
        DeviceToolOutput::Files { entries, truncated } => {
            let more = if *truncated { "+" } else { "" };
            match entries.len() {
                1 => "1 entry".into(),
                n => format!("{n}{more} entries"),
            }
        }
        DeviceToolOutput::Matches { matches, truncated } => {
            let files: BTreeSet<&str> = matches.iter().map(|m| m.path.as_str()).collect();
            let more = if *truncated { "+" } else { "" };
            let noun = if matches.len() == 1 {
                "match"
            } else {
                "matches"
            };
            let in_files = match files.len() {
                0 => String::new(),
                1 => " in 1 file".into(),
                n => format!(" in {n} files"),
            };
            format!("{}{more} {noun}{in_files}", matches.len())
        }
        DeviceToolOutput::FileText {
            start_line,
            end_line,
            total_lines,
            ..
        } => format!("Lines {start_line}–{end_line} of {total_lines}"),
        DeviceToolOutput::Done { message } => message.clone(),
        DeviceToolOutput::Manifest { files } => match files.len() {
            1 => "1 file".into(),
            n => format!("{n} files"),
        },
    }
}

fn size_text(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.1} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.1} GB", b / (KB * KB * KB))
    }
}

/// What a device call is for, which decides how an offline device is explained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    Folder,
    AppControl,
}

/// The error the model gets when `device` has no socket open.
fn offline_message(device: &str, purpose: Purpose) -> String {
    match purpose {
        Purpose::Folder => format!(
            "{device} isn't connected to Margin right now (the app must be open to send the folder's files)."
        ),
        Purpose::AppControl => format!(
            "{device} isn't connected to Margin right now (the app must be open to show something there)."
        ),
    }
}

/// The error the model gets when `device` didn't answer before the deadline.
fn timed_out_message(device: &str, timeout: Duration) -> String {
    format!(
        "{device} didn't answer within {} seconds; the app may have been closed or put in the background. Try again later or with a narrower request.",
        timeout.as_secs()
    )
}

/// Sends `call` to `device` and turns its answer (or silence) into a tool output.
async fn dispatch(
    ctx: &ToolContext,
    device: &DeviceRef,
    purpose: Purpose,
    call: DeviceToolCall,
    timeout: Duration,
) -> ToolOutput {
    let request = DeviceCall {
        chat_id: ctx.chat_id.clone(),
        call_id: ctx.call_id.clone(),
        call,
        timeout,
    };
    let answer = tokio::time::timeout(
        timeout + BRIDGE_GRACE,
        ctx.devices.call(&device.device_id, request),
    )
    .await;
    match answer {
        Ok(Ok(output)) => ToolOutput::ok(device_output_text(&output), output_summary(&output)),
        Ok(Err(DeviceError::Offline)) => ToolOutput::error(offline_message(&device.name, purpose)),
        Ok(Err(DeviceError::TimedOut)) | Err(_) => {
            ToolOutput::error(timed_out_message(&device.name, timeout))
        }
        Ok(Err(DeviceError::Failed(message))) => ToolOutput::error(message),
    }
}

/// The attached workspace `id`, or the error the model gets. The service's folder tools
/// (`list_files`, `find_files`) refuse with the same words.
fn workspace<'a>(ctx: &'a ToolContext, id: &str) -> Result<&'a Workspace, String> {
    if let Some(ws) = ctx.workspaces.iter().find(|w| w.id == id) {
        return Ok(ws);
    }
    if ctx.workspaces.is_empty() {
        return Err(
            "No folder is attached to this chat. The owner can attach one with Attach folder."
                .into(),
        );
    }
    let attached: Vec<String> = ctx
        .workspaces
        .iter()
        .map(|w| format!("“{}” ({})", w.name, w.id))
        .collect();
    Err(format!(
        "No folder attached to this chat has the workspace_id “{id}”. Attached: {}.",
        attached.join(", ")
    ))
}

fn workspace_device(ws: &Workspace) -> DeviceRef {
    DeviceRef {
        device_id: ws.device_id.clone(),
        name: ws.device_name.clone(),
    }
}

/// Runs a folder call on the device holding `workspace_id`.
async fn folder_call(
    ctx: &ToolContext,
    workspace_id: &str,
    call: DeviceToolCall,
    timeout: Duration,
) -> ToolOutput {
    match workspace(ctx, workspace_id) {
        Ok(ws) => dispatch(ctx, &workspace_device(ws), Purpose::Folder, call, timeout).await,
        Err(message) => ToolOutput::error(message),
    }
}

/// Runs an app-control call on the device whose message started the turn.
async fn app_call(ctx: &ToolContext, call: DeviceToolCall) -> ToolOutput {
    dispatch(
        ctx,
        &ctx.origin_device,
        Purpose::AppControl,
        call,
        APP_CONTROL_TIMEOUT,
    )
    .await
}

// ---------------------------------------------------------------------------
// Folder tools
// ---------------------------------------------------------------------------

struct SyncFolder;

#[derive(Deserialize, JsonSchema)]
struct SyncFolderArgs {
    /// The attached folder's workspace_id.
    workspace_id: String,
}

impl TypedTool for SyncFolder {
    type Args = SyncFolderArgs;
    const NAME: &'static str = "sync_folder";
    const DESCRIPTION: &'static str = "Ask the device holding an attached folder to send what changed in it (new, changed and deleted files). Only when the owner says the folder changed: files already sent are searchable without it, and the device syncs on its own every 15 minutes while Margin is open there.";
    const KIND: ToolKind = ToolKind::Device;

    fn label(&self, _args: &SyncFolderArgs) -> String {
        "Syncing the folder".into()
    }

    fn context_label(&self, ctx: &ToolContext, args: &SyncFolderArgs) -> String {
        match workspace(ctx, &args.workspace_id) {
            Ok(ws) => format!("Syncing “{}”", ws.name),
            Err(_) => self.label(args),
        }
    }

    fn device(&self, ctx: &ToolContext, args: &SyncFolderArgs) -> Option<DeviceRef> {
        workspace(ctx, &args.workspace_id)
            .ok()
            .map(workspace_device)
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: SyncFolderArgs) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let call = DeviceToolCall::SyncFolder {
                workspace_id: args.workspace_id.clone(),
            };
            folder_call(ctx, &args.workspace_id, call, FOLDER_TOOL_TIMEOUT).await
        })
    }
}

// ---------------------------------------------------------------------------
// App control
// ---------------------------------------------------------------------------

/// `NavTarget` with a schema.
#[derive(Deserialize, JsonSchema)]
#[serde(tag = "screen", rename_all = "snake_case")]
enum Screen {
    /// A lesson, in one of its views (null for the one last used).
    Lesson {
        lesson_id: String,
        view: Option<View>,
    },
    Topic {
        topic_id: String,
    },
    Note {
        note_id: String,
    },
    AllNotes,
    Chat {
        chat_id: String,
    },
    Settings,
}

#[derive(Deserialize, JsonSchema, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum View {
    Read,
    Notes,
    Video,
}

impl From<Screen> for NavTarget {
    fn from(screen: Screen) -> Self {
        match screen {
            Screen::Lesson { lesson_id, view } => NavTarget::Lesson {
                lesson_id,
                view: view.map(|v| match v {
                    View::Read => LessonView::Read,
                    View::Notes => LessonView::Notes,
                    View::Video => LessonView::Video,
                }),
            },
            Screen::Topic { topic_id } => NavTarget::Topic { topic_id },
            Screen::Note { note_id } => NavTarget::Note { note_id },
            Screen::AllNotes => NavTarget::AllNotes,
            Screen::Chat { chat_id } => NavTarget::Chat { chat_id },
            Screen::Settings => NavTarget::Settings,
        }
    }
}

struct Navigate;

#[derive(Deserialize, JsonSchema)]
struct NavigateArgs {
    /// The screen to show.
    to: Screen,
}

impl TypedTool for Navigate {
    type Args = NavigateArgs;
    const NAME: &'static str = "navigate";
    const DESCRIPTION: &'static str = "Show a screen of the app (a lesson in a view, a topic, a note, all notes, a chat, Settings) on the device the owner is writing from. Only when the owner asks to see something.";
    const KIND: ToolKind = ToolKind::Device;

    fn label(&self, args: &NavigateArgs) -> String {
        match &args.to {
            Screen::Lesson { view, .. } => match view {
                Some(View::Video) => "Opening the lesson's video".into(),
                Some(View::Notes) => "Opening the lesson's notes".into(),
                _ => "Opening the lesson".into(),
            },
            Screen::Topic { .. } => "Opening the topic".into(),
            Screen::Note { .. } => "Opening the note".into(),
            Screen::AllNotes => "Opening all notes".into(),
            Screen::Chat { .. } => "Opening the chat".into(),
            Screen::Settings => "Opening Settings".into(),
        }
    }

    fn device(&self, ctx: &ToolContext, _args: &NavigateArgs) -> Option<DeviceRef> {
        Some(ctx.origin_device.clone())
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: NavigateArgs) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let call = DeviceToolCall::Navigate { to: args.to.into() };
            app_call(ctx, call).await
        })
    }
}

struct OpenCitation;

#[derive(Deserialize, JsonSchema)]
struct OpenCitationArgs {
    lesson_id: String,
    /// The citation's number in the lesson (its [^n] footnote).
    n: u32,
}

impl TypedTool for OpenCitation {
    type Args = OpenCitationArgs;
    const NAME: &'static str = "open_citation";
    const DESCRIPTION: &'static str = "Open a lesson's citation in the source viewer (at the cited page, time or passage) on the device the owner is writing from.";
    const KIND: ToolKind = ToolKind::Device;

    fn label(&self, args: &OpenCitationArgs) -> String {
        format!("Opening citation {}", args.n)
    }

    fn device(&self, ctx: &ToolContext, _args: &OpenCitationArgs) -> Option<DeviceRef> {
        Some(ctx.origin_device.clone())
    }

    fn run<'a>(
        &'a self,
        ctx: &'a ToolContext,
        args: OpenCitationArgs,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let call = DeviceToolCall::OpenCitation {
                lesson_id: args.lesson_id,
                n: args.n,
            };
            app_call(ctx, call).await
        })
    }
}

struct ShowPassage;

#[derive(Deserialize, JsonSchema)]
struct ShowPassageArgs {
    source_id: String,
    /// PDF page, from 1; null otherwise.
    page: Option<u32>,
    /// Where in an audio or video source, in milliseconds; null otherwise.
    start_ms: Option<u64>,
}

impl TypedTool for ShowPassage {
    type Args = ShowPassageArgs;
    const NAME: &'static str = "show_passage";
    const DESCRIPTION: &'static str = "Open a source in the source viewer at a page or time, on the device the owner is writing from.";
    const KIND: ToolKind = ToolKind::Device;

    fn label(&self, args: &ShowPassageArgs) -> String {
        match (args.page, args.start_ms) {
            (Some(page), _) => format!("Showing page {page}"),
            (None, Some(ms)) => {
                let s = ms / 1000;
                format!("Showing {}:{:02}", s / 60, s % 60)
            }
            (None, None) => "Showing the source".into(),
        }
    }

    fn device(&self, ctx: &ToolContext, _args: &ShowPassageArgs) -> Option<DeviceRef> {
        Some(ctx.origin_device.clone())
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: ShowPassageArgs) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let call = DeviceToolCall::ShowPassage {
                source_id: args.source_id,
                page: args.page,
                start_ms: args.start_ms,
            };
            app_call(ctx, call).await
        })
    }
}

struct ToggleSidebar;

#[derive(Deserialize, JsonSchema)]
struct ToggleSidebarArgs {
    /// true shows the sidebar, false hides it, null toggles it.
    visible: Option<bool>,
}

impl TypedTool for ToggleSidebar {
    type Args = ToggleSidebarArgs;
    const NAME: &'static str = "toggle_sidebar";
    const DESCRIPTION: &'static str =
        "Show or hide the app's sidebar on the device the owner is writing from.";
    const KIND: ToolKind = ToolKind::Device;

    fn label(&self, args: &ToggleSidebarArgs) -> String {
        match args.visible {
            Some(true) => "Showing the sidebar".into(),
            Some(false) => "Hiding the sidebar".into(),
            None => "Toggling the sidebar".into(),
        }
    }

    fn device(&self, ctx: &ToolContext, _args: &ToggleSidebarArgs) -> Option<DeviceRef> {
        Some(ctx.origin_device.clone())
    }

    fn run<'a>(
        &'a self,
        ctx: &'a ToolContext,
        args: ToggleSidebarArgs,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let call = DeviceToolCall::ToggleSidebar {
                visible: args.visible,
            };
            app_call(ctx, call).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check_strict_schema;
    use chrono::{DateTime, Utc};
    use margin_shared::{FileEntry, FileStamp, FolderFileType, ManifestFile, Platform};
    use serde_json::{Value, json};
    use std::sync::Mutex;

    fn at() -> DateTime<Utc> {
        DateTime::from_timestamp(1_760_000_000, 0).unwrap()
    }

    /// Answers every call with `answer`, recording what it was asked.
    struct FakeDevice {
        answer: Result<DeviceToolOutput, DeviceError>,
        calls: Mutex<Vec<(String, DeviceCall)>>,
    }

    impl DeviceBridge for FakeDevice {
        fn is_online(&self, _device_id: &str) -> bool {
            true
        }

        fn call<'a>(
            &'a self,
            device_id: &'a str,
            call: DeviceCall,
        ) -> BoxFuture<'a, Result<DeviceToolOutput, DeviceError>> {
            self.calls
                .lock()
                .unwrap()
                .push((device_id.to_string(), call));
            let answer = self.answer.clone();
            Box::pin(async move { answer })
        }
    }

    /// Never answers.
    struct SilentDevice;

    impl DeviceBridge for SilentDevice {
        fn is_online(&self, _device_id: &str) -> bool {
            true
        }

        fn call<'a>(
            &'a self,
            _device_id: &'a str,
            _call: DeviceCall,
        ) -> BoxFuture<'a, Result<DeviceToolOutput, DeviceError>> {
            Box::pin(std::future::pending())
        }
    }

    fn ctx(devices: Arc<dyn DeviceBridge>) -> ToolContext {
        ToolContext {
            chat_id: "c1".into(),
            turn_id: "t1".into(),
            call_id: "call_1".into(),
            origin_device: DeviceRef {
                device_id: "phone".into(),
                name: "iPhone".into(),
            },
            workspaces: vec![Workspace {
                id: "w1".into(),
                name: "notes".into(),
                device_id: "mac".into(),
                device_name: "MacBook Pro".into(),
                platform: Platform::Macos,
                location: "~/notes".into(),
                created_at: at(),
                sync: Default::default(),
            }],
            devices,
        }
    }

    fn tool(name: &str) -> Arc<dyn Tool> {
        device_tools()
            .into_iter()
            .find(|t| t.name() == name)
            .unwrap()
    }

    fn fake(answer: Result<DeviceToolOutput, DeviceError>) -> Arc<FakeDevice> {
        Arc::new(FakeDevice {
            answer,
            calls: Mutex::new(Vec::new()),
        })
    }

    #[test]
    fn every_device_tool_has_a_strict_schema() {
        let tools = device_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(
            names,
            [
                "sync_folder",
                "navigate",
                "open_citation",
                "show_passage",
                "toggle_sidebar"
            ]
        );
        for t in &tools {
            assert_eq!(t.kind(), ToolKind::Device);
            check_strict_schema(&t.parameters())
                .unwrap_or_else(|e| panic!("{}: {e}\n{}", t.name(), t.parameters()));
            assert!(
                FOLDER_TOOLS.contains(&t.name()) || APP_CONTROL_TOOLS.contains(&t.name()),
                "{}",
                t.name()
            );
        }
        // Folders are read on the service: `sync_folder` is the only device folder tool.
        let folder: Vec<&str> = names
            .iter()
            .copied()
            .filter(|n| FOLDER_TOOLS.contains(n))
            .collect();
        assert_eq!(folder, ["sync_folder"]);
    }

    #[test]
    fn sync_folders_schema_is_one_workspace_id() {
        let schema = tool("sync_folder").parameters();
        assert_eq!(
            schema,
            json!({
                "type": "object",
                "properties": {
                    "workspace_id": {
                        "type": "string",
                        "description": "The attached folder's workspace_id."
                    }
                },
                "required": ["workspace_id"],
                "additionalProperties": false
            })
        );
    }

    #[tokio::test]
    async fn sync_folder_goes_to_the_holding_device_and_says_what_it_did() {
        let message = "Synced “notes”: 3 files to send, 1 removed; sending now.";
        let device = fake(Ok(DeviceToolOutput::Done {
            message: message.into(),
        }));
        let ctx = ctx(device.clone());
        let args = json!({"workspace_id": "w1"});
        let sync = tool("sync_folder");
        assert_eq!(sync.label(&args), "Syncing the folder");
        assert_eq!(sync.context_label(&ctx, &args), "Syncing “notes”");
        assert_eq!(sync.device(&ctx, &args).unwrap().name, "MacBook Pro");
        let out = sync.run(&ctx, args).await;
        assert!(out.ok);
        assert_eq!(out.output, message);
        assert_eq!(out.summary, message);
        assert!(out.images.is_empty());
        let calls = device.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "mac");
        assert_eq!(calls[0].1.timeout, FOLDER_TOOL_TIMEOUT);
        assert_eq!(calls[0].1.call_id, "call_1");
        assert_eq!(calls[0].1.chat_id, "c1");
        assert_eq!(
            calls[0].1.call,
            DeviceToolCall::SyncFolder {
                workspace_id: "w1".into()
            }
        );
    }

    #[tokio::test]
    async fn app_control_goes_to_the_origin_device() {
        let device = fake(Ok(DeviceToolOutput::Done {
            message: "Opened “Fourier series · Lesson 2”.".into(),
        }));
        let ctx = ctx(device.clone());
        let args = json!({"to": {"screen": "lesson", "lesson_id": "l2", "view": "video"}});
        let nav = tool("navigate");
        assert_eq!(nav.label(&args), "Opening the lesson's video");
        assert_eq!(nav.context_label(&ctx, &args), "Opening the lesson's video");
        assert_eq!(nav.device(&ctx, &args).unwrap().device_id, "phone");
        let out = nav.run(&ctx, args).await;
        assert!(out.ok);
        assert_eq!(out.summary, "Opened “Fourier series · Lesson 2”.");
        let calls = device.calls.lock().unwrap();
        assert_eq!(calls[0].0, "phone");
        assert_eq!(calls[0].1.timeout, APP_CONTROL_TIMEOUT);
        assert_eq!(
            calls[0].1.call,
            DeviceToolCall::Navigate {
                to: NavTarget::Lesson {
                    lesson_id: "l2".into(),
                    view: Some(LessonView::Video)
                }
            }
        );
    }

    #[tokio::test]
    async fn offline_devices_are_named() {
        let ctx = ctx(Arc::new(NoDevices));
        let out = tool("sync_folder")
            .run(&ctx, json!({"workspace_id": "w1"}))
            .await;
        assert!(!out.ok);
        assert_eq!(
            out.summary,
            "MacBook Pro isn't connected to Margin right now (the app must be open to send the folder's files)."
        );
        let parsed: Value = serde_json::from_str(&out.output).unwrap();
        assert_eq!(parsed["error"], out.summary);
        let out = tool("toggle_sidebar")
            .run(&ctx, json!({"visible": null}))
            .await;
        assert_eq!(
            out.summary,
            "iPhone isn't connected to Margin right now (the app must be open to show something there)."
        );
    }

    #[tokio::test]
    async fn timeouts_and_device_errors_go_back_to_the_model() {
        let ctx_timed = ctx(fake(Err(DeviceError::TimedOut)));
        let out = tool("sync_folder")
            .run(&ctx_timed, json!({"workspace_id": "w1"}))
            .await;
        assert!(!out.ok);
        assert!(
            out.summary
                .starts_with("MacBook Pro didn't answer within 60 seconds"),
            "{}",
            out.summary
        );

        let ctx_err = ctx(fake(Err(DeviceError::Failed(
            "Margin no longer has access to the folder “notes”; attach it again on the Mac.".into(),
        ))));
        let out = tool("sync_folder")
            .run(&ctx_err, json!({"workspace_id": "w1"}))
            .await;
        assert!(!out.ok);
        assert_eq!(
            out.summary,
            "Margin no longer has access to the folder “notes”; attach it again on the Mac."
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_bridge_still_times_out() {
        let ctx = ctx(Arc::new(SilentDevice));
        let out = tool("sync_folder")
            .run(&ctx, json!({"workspace_id": "w1"}))
            .await;
        assert!(!out.ok);
        assert!(
            out.summary
                .starts_with("MacBook Pro didn't answer within 60 seconds")
        );
        let out = tool("open_citation")
            .run(&ctx, json!({"lesson_id": "l1", "n": 2}))
            .await;
        assert!(!out.ok);
        assert!(
            out.summary
                .starts_with("iPhone didn't answer within 20 seconds")
        );
    }

    #[tokio::test]
    async fn unknown_workspaces_are_refused_without_a_call() {
        let device = fake(Ok(DeviceToolOutput::Done {
            message: String::new(),
        }));
        let mut c = ctx(device.clone());
        let sync = tool("sync_folder");
        let out = sync.run(&c, json!({"workspace_id": "w9"})).await;
        assert!(!out.ok);
        assert_eq!(
            out.summary,
            "No folder attached to this chat has the workspace_id “w9”. Attached: “notes” (w1)."
        );
        assert_eq!(
            sync.context_label(&c, &json!({"workspace_id": "w9"})),
            "Syncing the folder"
        );
        assert!(sync.device(&c, &json!({"workspace_id": "w9"})).is_none());
        c.workspaces.clear();
        let out = sync.run(&c, json!({"workspace_id": "w1"})).await;
        assert_eq!(
            out.summary,
            "No folder is attached to this chat. The owner can attach one with Attach folder."
        );
        assert!(device.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn done_and_manifest_answers_read_compactly() {
        assert_eq!(
            device_output_text(&DeviceToolOutput::Done {
                message: "Synced “notes”: nothing changed.".into()
            }),
            "Synced “notes”: nothing changed."
        );
        let file = |path: &str, kind, bytes| ManifestFile {
            path: path.into(),
            kind,
            stamp: FileStamp {
                bytes,
                modified: Some(at()),
                sha256: "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08".into(),
            },
        };
        let manifest = DeviceToolOutput::Manifest {
            files: vec![
                file("intro.md", FolderFileType::Text, 120),
                file("ch1/paper.pdf", FolderFileType::Pdf, 2_130_000),
            ],
        };
        assert_eq!(
            device_output_text(&manifest),
            "2 files:\nintro.md  (120 B, text, sha256 9f86d081884c)\nch1/paper.pdf  (2.0 MB, pdf, sha256 9f86d081884c)"
        );
        assert_eq!(output_summary(&manifest), "2 files");
        assert_eq!(
            device_output_text(&DeviceToolOutput::Manifest { files: vec![] }),
            "No files."
        );
        // Cut at MAX_LISTED_ENTRIES.
        let many = DeviceToolOutput::Manifest {
            files: (0..MAX_LISTED_ENTRIES + 3)
                .map(|i| file(&format!("p{i}.png"), FolderFileType::Image, 10))
                .collect(),
        };
        let text = device_output_text(&many);
        assert_eq!(text.lines().count(), MAX_LISTED_ENTRIES + 2);
        assert!(text.ends_with("[3 more files not listed.]"), "{text}");
    }

    #[test]
    fn old_folder_answers_still_render() {
        let text = device_output_text(&DeviceToolOutput::Files {
            entries: vec![
                FileEntry {
                    path: "ch1".into(),
                    is_dir: true,
                    bytes: 0,
                    modified: None,
                    binary: false,
                    kind: None,
                },
                FileEntry {
                    path: "README.md".into(),
                    is_dir: false,
                    bytes: 2150,
                    modified: Some(at()),
                    binary: false,
                    kind: Some(FolderFileType::Text),
                },
            ],
            truncated: true,
        });
        assert_eq!(
            text,
            "ch1/\nREADME.md  (2.1 KB, modified 2025-10-09)\n[More than 2 entries; only these are listed. Narrow the path or the pattern.]"
        );
        assert_eq!(
            device_output_text(&DeviceToolOutput::Matches {
                matches: vec![],
                truncated: false
            }),
            "No matches."
        );
    }
}
