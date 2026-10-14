//! Device tools: folder reads and app control, run on a device over its events socket.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use margin_shared::{
    DeviceRef, DeviceToolCall, DeviceToolOutput, Id, LessonView, NavTarget, READ_FILE_MAX_LINES,
    Workspace,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{
    APP_CONTROL_TIMEOUT, BoxFuture, FOLDER_TOOL_TIMEOUT, SEARCH_TEXT_TIMEOUT, Tool, ToolContext,
    ToolKind, ToolOutput, Typed, TypedTool,
};

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

/// The folder tools: offered only while the chat has a folder attached.
pub const FOLDER_TOOLS: [&str; 4] = ["list_files", "find_files", "search_text", "read_file"];
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
    vec![
        Arc::new(Typed(ListFiles)),
        Arc::new(Typed(FindFiles)),
        Arc::new(Typed(SearchText)),
        Arc::new(Typed(ReadFile)),
        Arc::new(Typed(Navigate)),
        Arc::new(Typed(OpenCitation)),
        Arc::new(Typed(ShowPassage)),
        Arc::new(Typed(ToggleSidebar)),
    ]
}

/// What the model reads for a device's answer: a compact text rendering (one entry or
/// match per line, file text with line numbers).
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
            files
                .iter()
                .map(|f| format!("{}  ({})", f.path, size_text(f.stamp.bytes)))
                .collect::<Vec<_>>()
                .join("\n")
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
            "{device} isn't connected to Margin right now (the app must be open to read the folder)."
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

/// The attached workspace `id`, or the error the model gets.
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

fn quoted_path(path: &str) -> String {
    if path.trim_matches(['/', '.']).is_empty() {
        "the folder".into()
    } else {
        format!("“{path}”")
    }
}

// ---------------------------------------------------------------------------
// Folder tools
// ---------------------------------------------------------------------------

struct ListFiles;

#[derive(Deserialize, JsonSchema)]
struct ListFilesArgs {
    /// The attached folder's workspace_id.
    workspace_id: String,
    /// A folder inside it, relative to its root, "/"-separated; "" for the root.
    path: String,
}

impl TypedTool for ListFiles {
    type Args = ListFilesArgs;
    const NAME: &'static str = "list_files";
    const DESCRIPTION: &'static str = "List the files and folders directly inside a folder of an attached folder (not recursive). Hidden files, build folders and files that may hold secrets are never listed. Read-only.";
    const KIND: ToolKind = ToolKind::Device;

    fn label(&self, args: &ListFilesArgs) -> String {
        format!("Listing {}", quoted_path(&args.path))
    }

    fn device(&self, ctx: &ToolContext, args: &ListFilesArgs) -> Option<DeviceRef> {
        workspace(ctx, &args.workspace_id)
            .ok()
            .map(workspace_device)
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: ListFilesArgs) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let call = DeviceToolCall::ListFiles {
                workspace_id: args.workspace_id.clone(),
                path: args.path,
            };
            folder_call(ctx, &args.workspace_id, call, FOLDER_TOOL_TIMEOUT).await
        })
    }
}

struct FindFiles;

#[derive(Deserialize, JsonSchema)]
struct FindFilesArgs {
    /// The attached folder's workspace_id.
    workspace_id: String,
    /// Where to look, relative to the folder's root; "" for the whole folder.
    path: String,
    /// A glob matched against paths relative to `path`, case-insensitive: `*` within a
    /// name, `**` across folders, `?` one character. E.g. "**/*.md".
    pattern: String,
}

impl TypedTool for FindFiles {
    type Args = FindFilesArgs;
    const NAME: &'static str = "find_files";
    const DESCRIPTION: &'static str = "Find files anywhere under a folder of an attached folder whose path matches a glob pattern. Read-only.";
    const KIND: ToolKind = ToolKind::Device;

    fn label(&self, args: &FindFilesArgs) -> String {
        format!("Finding “{}” in {}", args.pattern, quoted_path(&args.path))
    }

    fn device(&self, ctx: &ToolContext, args: &FindFilesArgs) -> Option<DeviceRef> {
        workspace(ctx, &args.workspace_id)
            .ok()
            .map(workspace_device)
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: FindFilesArgs) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let call = DeviceToolCall::FindFiles {
                workspace_id: args.workspace_id.clone(),
                path: args.path,
                pattern: args.pattern,
            };
            folder_call(ctx, &args.workspace_id, call, FOLDER_TOOL_TIMEOUT).await
        })
    }
}

struct SearchText;

#[derive(Deserialize, JsonSchema)]
struct SearchTextArgs {
    /// The attached folder's workspace_id.
    workspace_id: String,
    /// Plain text to look for, case-insensitive (not a regular expression).
    query: String,
    /// Where to search, relative to the folder's root; "" for the whole folder.
    path: String,
    /// Only search files whose path matches this glob (e.g. "**/*.rs"); null for all
    /// text files.
    pattern: Option<String>,
}

impl TypedTool for SearchText {
    type Args = SearchTextArgs;
    const NAME: &'static str = "search_text";
    const DESCRIPTION: &'static str = "Search the text files under a folder of an attached folder for lines containing some text. Returns path:line: text per match. Read-only.";
    const KIND: ToolKind = ToolKind::Device;

    fn label(&self, args: &SearchTextArgs) -> String {
        format!("Searching {} for “{}”", quoted_path(&args.path), args.query)
    }

    fn device(&self, ctx: &ToolContext, args: &SearchTextArgs) -> Option<DeviceRef> {
        workspace(ctx, &args.workspace_id)
            .ok()
            .map(workspace_device)
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: SearchTextArgs) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let call = DeviceToolCall::SearchText {
                workspace_id: args.workspace_id.clone(),
                path: args.path,
                query: args.query,
                pattern: args.pattern,
            };
            folder_call(ctx, &args.workspace_id, call, SEARCH_TEXT_TIMEOUT).await
        })
    }
}

struct ReadFile;

#[derive(Deserialize, JsonSchema)]
struct ReadFileArgs {
    /// The attached folder's workspace_id.
    workspace_id: String,
    /// The file, relative to the folder's root.
    path: String,
    /// First line to read, from 1.
    start_line: u32,
    /// How many lines, at most 400.
    max_lines: u32,
}

impl TypedTool for ReadFile {
    type Args = ReadFileArgs;
    const NAME: &'static str = "read_file";
    const DESCRIPTION: &'static str = "Read lines of a text file in an attached folder, numbered. At most 400 lines (64 KB) per call; read long files in parts. Read-only.";
    const KIND: ToolKind = ToolKind::Device;

    fn label(&self, args: &ReadFileArgs) -> String {
        format!("Reading “{}”", args.path)
    }

    fn device(&self, ctx: &ToolContext, args: &ReadFileArgs) -> Option<DeviceRef> {
        workspace(ctx, &args.workspace_id)
            .ok()
            .map(workspace_device)
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: ReadFileArgs) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let call = DeviceToolCall::ReadFile {
                workspace_id: args.workspace_id.clone(),
                path: args.path,
                start_line: args.start_line.max(1),
                max_lines: args.max_lines.clamp(1, READ_FILE_MAX_LINES),
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
    use margin_shared::{FileEntry, Platform, TextMatch};
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
                name: "margin".into(),
                device_id: "mac".into(),
                device_name: "MacBook Pro".into(),
                platform: Platform::Macos,
                location: "~/Developer/margin".into(),
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
        assert_eq!(tools.len(), 8);
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
    }

    #[tokio::test]
    async fn folder_tools_go_to_the_workspace_device_with_their_deadline() {
        let device = fake(Ok(DeviceToolOutput::Matches {
            matches: vec![
                TextMatch {
                    path: "src/a.rs".into(),
                    line: 3,
                    text: "fn main() {}".into(),
                    page: None,
                },
                TextMatch {
                    path: "src/b.rs".into(),
                    line: 9,
                    text: "main();".into(),
                    page: None,
                },
            ],
            truncated: false,
        }));
        let ctx = ctx(device.clone());
        let args = json!({"workspace_id": "w1", "query": "main", "path": "", "pattern": null});
        let search = tool("search_text");
        assert_eq!(search.label(&args), "Searching the folder for “main”");
        assert_eq!(search.device(&ctx, &args).unwrap().name, "MacBook Pro");
        let out = search.run(&ctx, args).await;
        assert!(out.ok);
        assert_eq!(out.summary, "2 matches in 2 files");
        assert_eq!(out.output, "src/a.rs:3: fn main() {}\nsrc/b.rs:9: main();");
        let calls = device.calls.lock().unwrap();
        assert_eq!(calls[0].0, "mac");
        assert_eq!(calls[0].1.timeout, SEARCH_TEXT_TIMEOUT);
        assert_eq!(calls[0].1.call_id, "call_1");
        assert_eq!(calls[0].1.chat_id, "c1");
    }

    #[tokio::test]
    async fn read_file_clamps_its_lines() {
        let device = fake(Ok(DeviceToolOutput::FileText {
            path: "README.md".into(),
            start_line: 1,
            end_line: 2,
            total_lines: 10,
            text: "# Margin\nA study app.".into(),
        }));
        let ctx = ctx(device.clone());
        let out = tool("read_file")
            .run(
                &ctx,
                json!({"workspace_id": "w1", "path": "README.md", "start_line": 0, "max_lines": 5000}),
            )
            .await;
        assert!(out.ok);
        assert_eq!(
            out.output,
            "README.md, lines 1–2 of 10:\n1  # Margin\n2  A study app.\n[Lines 3–10 not shown; read on from line 3.]"
        );
        let calls = device.calls.lock().unwrap();
        assert_eq!(calls[0].1.timeout, FOLDER_TOOL_TIMEOUT);
        assert_eq!(
            calls[0].1.call,
            DeviceToolCall::ReadFile {
                workspace_id: "w1".into(),
                path: "README.md".into(),
                start_line: 1,
                max_lines: READ_FILE_MAX_LINES,
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
        let out = tool("list_files")
            .run(&ctx, json!({"workspace_id": "w1", "path": "src"}))
            .await;
        assert!(!out.ok);
        assert_eq!(
            out.summary,
            "MacBook Pro isn't connected to Margin right now (the app must be open to read the folder)."
        );
        let parsed: Value = serde_json::from_str(&out.output).unwrap();
        assert_eq!(parsed["error"], out.summary);
        let out = tool("toggle_sidebar")
            .run(&ctx, json!({"visible": null}))
            .await;
        assert!(out.summary.starts_with("iPhone isn't connected"));
    }

    #[tokio::test]
    async fn timeouts_and_device_errors_go_back_to_the_model() {
        let ctx_timed = ctx(fake(Err(DeviceError::TimedOut)));
        let out = tool("find_files")
            .run(
                &ctx_timed,
                json!({"workspace_id": "w1", "path": "", "pattern": "*.md"}),
            )
            .await;
        assert!(!out.ok);
        assert!(
            out.summary
                .starts_with("MacBook Pro didn't answer within 60 seconds")
        );

        let ctx_err = ctx(fake(Err(DeviceError::Failed(
            "Margin no longer has access to this folder; attach it again on the Mac.".into(),
        ))));
        let out = tool("list_files")
            .run(&ctx_err, json!({"workspace_id": "w1", "path": ""}))
            .await;
        assert_eq!(
            out.summary,
            "Margin no longer has access to this folder; attach it again on the Mac."
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_bridge_still_times_out() {
        let ctx = ctx(Arc::new(SilentDevice));
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
        let out = tool("list_files")
            .run(&c, json!({"workspace_id": "w9", "path": ""}))
            .await;
        assert!(!out.ok);
        assert!(out.summary.contains("“margin” (w1)"), "{}", out.summary);
        c.workspaces.clear();
        let out = tool("list_files")
            .run(&c, json!({"workspace_id": "w1", "path": ""}))
            .await;
        assert!(out.summary.starts_with("No folder is attached"));
        assert!(device.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn file_listings_read_compactly() {
        let text = device_output_text(&DeviceToolOutput::Files {
            entries: vec![
                FileEntry {
                    path: "src".into(),
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
                    kind: None,
                },
                FileEntry {
                    path: "logo.png".into(),
                    is_dir: false,
                    bytes: 12,
                    modified: None,
                    binary: true,
                    kind: None,
                },
            ],
            truncated: true,
        });
        assert_eq!(
            text,
            "src/\nREADME.md  (2.1 KB, modified 2025-10-09)\nlogo.png  (12 B, not readable as text)\n[More than 3 entries; only these are listed. Narrow the path or the pattern.]"
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
