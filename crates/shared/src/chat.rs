//! Chats with the agent: the chat record, its ordered items and inline cards, workspaces
//! (folders a device lets the agent read), and the bodies of the chat routes.
//!
//! The agent loop runs on the service. Every item a turn produces is stored there and
//! broadcast (`Event::ChatItemChanged` / `Event::ChatItemDelta`), so every device shows
//! the same transcript live. `docs/agent-contract.md` describes the lifecycle of a turn.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{Citation, Id, Level, Provider, WebCitation, WebResult};

// ---------------------------------------------------------------------------
// Chats
// ---------------------------------------------------------------------------

/// A conversation with the agent. Listed in the sidebar's Chats section, newest
/// `updated_at` first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chat {
    pub id: Id,
    /// Set from the first message (its first line, at most `CHAT_TITLE_MAX` characters),
    /// then written by the model after the first answer, until the owner renames the
    /// chat. `title_by` says which.
    pub title: String,
    #[serde(default)]
    pub title_by: TitleSource,
    pub state: ChatState,
    /// Folders attached to the chat (`Workspace.id`), readable through the device that
    /// holds each one.
    #[serde(default)]
    pub workspace_ids: Vec<Id>,
    /// Files attached to the chat (`Source.id`). Sources attached only to a chat are kept
    /// while the chat exists (the 24-hour cleanup of unattached sources skips them) and
    /// deleted with it.
    #[serde(default)]
    pub source_ids: Vec<Id>,
    /// A message sent while a turn was running; it is sent as the next turn when the
    /// running one ends. At most one.
    #[serde(default)]
    pub pending: Option<PendingMessage>,
    /// Tokens spent by every turn of this chat, for the footer.
    #[serde(default)]
    pub usage: Usage,
    /// Set while the running turn waits before retrying a model call (the API asked it
    /// to slow down or failed); the chat shows it's waiting. Cleared when the retry
    /// starts or the turn ends.
    #[serde(default)]
    pub waiting: Option<Waiting>,
    /// The model provider the chat runs on: set at creation (`NewChat.provider`, else
    /// Settings' default) and never changed. Chats stored before providers are OpenAI.
    #[serde(default)]
    pub provider: Provider,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Longest generated chat title, in characters.
pub const CHAT_TITLE_MAX: usize = 60;

/// Who wrote `Chat.title`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TitleSource {
    /// The first line of the first message, until the model names the chat (the app
    /// shows it as a placeholder).
    #[default]
    FirstLine,
    /// Written by the model after the chat's first answer.
    Model,
    /// The owner renamed the chat (`ChatPatch.title`); never replaced.
    Owner,
}

/// A running turn waiting before it retries a model call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Waiting {
    /// When the retry starts: the server's `Retry-After` as given (uncapped), else the
    /// backoff delay.
    pub until: DateTime<Utc>,
    /// Why, in a short sentence for the chat, e.g. "The model API asked Margin to slow
    /// down."
    pub reason: String,
}

/// Where a chat's current (or last) turn is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ChatState {
    /// No turn is running. A new message starts one.
    Idle,
    /// A turn is running on the service. Any device can stop it.
    Running {
        turn_id: Id,
        /// The device whose message started the turn; app-control tools go to it.
        device_id: Id,
        started_at: DateTime<Utc>,
    },
    /// The turn is parked on a confirmation card (`Card::Confirmation`) that any
    /// device can answer (`routes::chat_confirm`). Stop counts as a denial.
    WaitingForUser {
        turn_id: Id,
        device_id: Id,
        /// The `ChatItem.id` of the confirmation card.
        item_id: Id,
    },
    /// The service stopped while the turn ran. `routes::chat_continue` resumes it.
    Interrupted { turn_id: Id },
    /// The turn ended on an error the loop could not get past (after retries). The
    /// last item is an `ChatItemKind::Error`; `routes::chat_continue` retries.
    Failed { turn_id: Id, error: String },
}

impl ChatState {
    /// Whether a turn is in progress (running or waiting on a confirmation): a message
    /// sent now becomes the pending message.
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            ChatState::Running { .. } | ChatState::WaitingForUser { .. }
        )
    }

    /// Whether `routes::chat_continue` applies.
    pub fn can_continue(&self) -> bool {
        matches!(
            self,
            ChatState::Interrupted { .. } | ChatState::Failed { .. }
        )
    }

    /// The turn this state is about, if any.
    pub fn turn_id(&self) -> Option<&str> {
        match self {
            ChatState::Idle => None,
            ChatState::Running { turn_id, .. }
            | ChatState::WaitingForUser { turn_id, .. }
            | ChatState::Interrupted { turn_id }
            | ChatState::Failed { turn_id, .. } => Some(turn_id),
        }
    }
}

/// A message waiting for the running turn to end.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingMessage {
    pub text: String,
    #[serde(default)]
    pub source_ids: Vec<Id>,
    /// The device that sent it; it becomes the turn's originating device.
    pub device_id: Id,
    pub created_at: DateTime<Utc>,
}

/// Tokens spent, as the provider reports them (Responses `response.usage`, Messages
/// `usage`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    /// The part of `input_tokens` served from the prompt cache.
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    /// The part of `output_tokens` spent on reasoning.
    pub reasoning_tokens: u64,
    /// Model calls made.
    pub model_calls: u64,
    /// Input tokens written to the prompt cache (Claude's `cache_creation_input_tokens`;
    /// 0 on OpenAI, which writes no explicit entries).
    #[serde(default)]
    pub cache_write_input_tokens: u64,
    /// Hosted web searches run (Claude bills them per 1 000).
    #[serde(default)]
    pub web_search_requests: u64,
    /// Hosted web page fetches run.
    #[serde(default)]
    pub web_fetch_requests: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
        self.output_tokens += other.output_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
        self.model_calls += other.model_calls;
        self.cache_write_input_tokens += other.cache_write_input_tokens;
        self.web_search_requests += other.web_search_requests;
        self.web_fetch_requests += other.web_fetch_requests;
    }
}

// ---------------------------------------------------------------------------
// Chat items
// ---------------------------------------------------------------------------

/// One entry of a chat's transcript. Items are ordered by `seq` (assigned by the
/// service, increasing within a chat, never reused).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatItem {
    pub id: Id,
    pub chat_id: Id,
    /// The turn that produced it: every item from one user message to the end of the
    /// answer shares the user message's `turn_id`.
    pub turn_id: Id,
    pub seq: u64,
    pub status: ItemStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: ChatItemKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    /// Text or arguments are still streaming in (`Event::ChatItemDelta`), or the tool
    /// call is running.
    InProgress,
    Done,
    /// Cut short by Stop, a failure or a service restart. Kept and shown as it was.
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChatItemKind {
    /// What the owner typed. Starts a turn.
    UserMessage {
        text: String,
        /// Files attached with this message (also added to `Chat.source_ids`).
        #[serde(default)]
        source_ids: Vec<Id>,
        /// The device it was sent from, and its name at the time.
        device_id: Id,
        device_name: String,
    },
    /// The agent's answer text (Markdown, `$…$` math as in lessons). Deltas append to
    /// `text`. `citations` are the web pages a hosted web tool cited (shown as a
    /// numbered "Sources" list under the answer); empty without web use.
    AssistantText {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        citations: Vec<WebCitation>,
    },
    /// A summary of the model's reasoning. Deltas append to `summary`. May stay empty
    /// (the app shows nothing for an empty summary).
    Reasoning { summary: String },
    /// The model called a tool. Deltas append to `arguments` while they stream; the
    /// item is `InProgress` until its `ToolResult` is written.
    ToolCall {
        /// The Responses API `call_id`; the matching `ToolResult` carries it too.
        call_id: String,
        /// Tool name, e.g. "search_library".
        name: String,
        /// The arguments as JSON text, as the model wrote them.
        arguments: String,
        /// One line for the tool row, e.g. "Searching the library for “fourier”".
        /// Empty until the arguments are complete.
        label: String,
        /// Set for device tools: the device the call is sent to.
        #[serde(default)]
        device: Option<DeviceRef>,
    },
    /// What a tool returned. The full output (what the model saw) is not pushed to
    /// devices: fetch it with `routes::chat_item_output` when the row is expanded.
    ToolResult {
        call_id: String,
        /// False when the tool failed, was denied or was cancelled; the model was told why.
        ok: bool,
        /// One line for the tool row, e.g. "4 matches in 2 lessons".
        summary: String,
        /// Size of the full output, in bytes.
        output_bytes: u64,
    },
    /// An inline card: an editable outline, a result, retrieved passages, or a
    /// confirmation. Cards follow the `ToolResult` of the call that made them.
    Card {
        /// The tool call the card belongs to.
        call_id: String,
        card: Card,
    },
    /// Something that happened outside the model's own output that the next model call
    /// should know, e.g. "Created the course “Fourier series” with 5 lessons." (when
    /// the owner pressed Create lessons) or "Stopped by the owner.". Shown as a quiet
    /// line; replayed to the model as a developer message.
    Action { text: String },
    /// The turn hit an error it could not get past. `retryable` errors show Retry
    /// (`routes::chat_continue`) while the chat is `ChatState::Failed`.
    Error {
        message: String,
        retryable: bool,
        /// What kind of error, for the app to offer a fix (e.g. "Open Settings" for
        /// `no_api_key`): one of the `error_code` constants. `None` on items stored by older builds.
        #[serde(default)]
        code: Option<String>,
    },
}

/// `ChatItemKind::Error.code` values.
pub mod error_code {
    /// No API key is stored in the service Mac's Keychain.
    pub const NO_API_KEY: &str = "no_api_key";
    /// No model is set in Settings.
    pub const NO_MODEL: &str = "no_model";
    /// 429 after every retry, or a `failed`/`error` stream event coded
    /// `rate_limit_exceeded` or `insufficient_quota`.
    pub const RATE_LIMITED: &str = "rate_limited";
    /// 5xx after every retry, or a `failed`/`error` stream event with any other code.
    pub const SERVER_ERROR: &str = "server_error";
    /// The model API could not be reached, or the connection dropped.
    pub const NETWORK: &str = "network";
    /// The conversation no longer fits the model's context window.
    pub const CONTEXT_TOO_LONG: &str = "context_too_long";
    /// The answer was cut off (`Incomplete`).
    pub const INCOMPLETE: &str = "incomplete";
    /// The model API refused the request (4xx other than 429, or a `failed`/`error`
    /// stream event coded as an invalid request, prompt or image).
    pub const BAD_REQUEST: &str = "bad_request";
    /// The service could not store the turn's items.
    pub const STORAGE: &str = "storage";
    /// The turn ended without finishing (a panic or an unexpected stop).
    pub const INTERRUPTED: &str = "interrupted";
    /// The provider refused the credential (OpenAI 401/403; Claude 401, 403, 402), or
    /// the ChatGPT sign-in ended. The app offers Open Settings → that provider.
    pub const AUTH_FAILED: &str = "auth_failed";
    /// The Claude organization's spend limit is reached.
    pub const SPEND_LIMIT_REACHED: &str = "spend_limit_reached";
    /// Margin's share of the owner's ChatGPT plan is used up.
    pub const PLAN_LIMIT_REACHED: &str = "plan_limit_reached";
    /// The owner's ChatGPT plan can't be used by apps.
    pub const PLAN_INELIGIBLE: &str = "plan_ineligible";

    /// Every code, for exhaustiveness checks.
    pub const ALL: [&str; 14] = [
        NO_API_KEY,
        NO_MODEL,
        RATE_LIMITED,
        SERVER_ERROR,
        NETWORK,
        CONTEXT_TOO_LONG,
        INCOMPLETE,
        BAD_REQUEST,
        STORAGE,
        INTERRUPTED,
        AUTH_FAILED,
        SPEND_LIMIT_REACHED,
        PLAN_LIMIT_REACHED,
        PLAN_INELIGIBLE,
    ];
}

/// What `ChatItemKind::apply_delta` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaOutcome {
    Applied,
    /// The text already covers this delta (it was loaded after the delta was sent).
    AlreadyHad,
    /// Text before this delta is missing: reload the chat (or wait for the item's next
    /// `ChatItemChanged`).
    Gap,
    /// This kind of item doesn't stream.
    NotStreaming,
}

impl ChatItemKind {
    /// The text deltas append to: `AssistantText.text`, `Reasoning.summary`,
    /// `ToolCall.arguments`.
    pub fn streaming_text(&self) -> Option<&str> {
        match self {
            ChatItemKind::AssistantText { text, .. } => Some(text),
            ChatItemKind::Reasoning { summary } => Some(summary),
            ChatItemKind::ToolCall { arguments, .. } => Some(arguments),
            _ => None,
        }
    }

    fn streaming_text_mut(&mut self) -> Option<&mut String> {
        match self {
            ChatItemKind::AssistantText { text, .. } => Some(text),
            ChatItemKind::Reasoning { summary } => Some(summary),
            ChatItemKind::ToolCall { arguments, .. } => Some(arguments),
            _ => None,
        }
    }

    /// Applies an `Event::ChatItemDelta` whose text starts at byte `offset`.
    pub fn apply_delta(&mut self, offset: u64, delta: &str) -> DeltaOutcome {
        let Some(text) = self.streaming_text_mut() else {
            return DeltaOutcome::NotStreaming;
        };
        let have = text.len() as u64;
        let end = offset + delta.len() as u64;
        if offset == have {
            text.push_str(delta);
            DeltaOutcome::Applied
        } else if offset > have {
            DeltaOutcome::Gap
        } else if end <= have {
            DeltaOutcome::AlreadyHad
        } else {
            // Partly new: append the part past what we have, if it splits on a char.
            match delta.get((have - offset) as usize..) {
                Some(rest) => {
                    text.push_str(rest);
                    DeltaOutcome::Applied
                }
                None => DeltaOutcome::Gap,
            }
        }
    }
}

/// A device as a chat names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRef {
    pub device_id: Id,
    pub name: String,
}

// ---------------------------------------------------------------------------
// Cards
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Card {
    /// A course outline proposed by `propose_course`: the Ask outline editor with chips
    /// and Create lessons. The proposal itself is a `Proposal` record (fetched with
    /// `routes::proposal`, kept current by `Event::ProposalChanged`).
    Outline(OutlineCard),
    /// Something a tool created or started: a topic, a lesson being written, a video
    /// being rendered, a note. The app renders its live state from the library.
    Result(ResultCard),
    /// Source passages `search_sources` found, each openable in the source viewer.
    Passages(PassagesCard),
    /// The agent wants to do something that needs the owner's OK.
    Confirmation(ConfirmationCard),
    /// What a hosted web search found: links that open in the system browser.
    WebResults(WebResultsCard),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebResultsCard {
    /// What was searched for.
    pub query: String,
    pub results: Vec<WebResult>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutlineCard {
    pub proposal_id: Id,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultCard {
    pub target: ResultTarget,
    /// Title when the card was made (the app prefers the live record's title).
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResultTarget {
    Topic {
        topic_id: Id,
    },
    Lesson {
        lesson_id: Id,
    },
    /// The video of a lesson (one per lesson).
    Video {
        lesson_id: Id,
    },
    Note {
        note_id: Id,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PassagesCard {
    /// What was searched for.
    pub query: String,
    /// The passages, numbered from 1 in order (`Citation.n`); opened like a lesson's
    /// citations.
    pub passages: Vec<Citation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfirmationCard {
    /// The tool that asks, e.g. "delete_topic".
    pub tool: String,
    /// The question, e.g. "Delete the topic “Fourier series”?".
    pub title: String,
    /// What would be lost or changed, e.g. "3 lessons, 2 notes and 1 video go with it."
    pub detail: String,
    /// The button that approves, e.g. "Delete" or "Replace".
    pub approve_label: String,
    pub decision: Decision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    /// Waiting for the owner.
    Pending,
    Approved {
        device_id: Id,
    },
    /// Denied with the Deny button, or by Stop (`by_stop`).
    Denied {
        device_id: Id,
        by_stop: bool,
    },
    /// The service restarted before anyone answered; the model was told so.
    Expired,
}

// ---------------------------------------------------------------------------
// Workspaces (attached folders)
// ---------------------------------------------------------------------------

/// A folder one device holds and attaches to chats. The handle (path, bookmark or tree
/// URI) stays on that device; its allow-listed files (`folder_file_type`) are copied to
/// the service as they change and read there like sources, so the agent searches what
/// the service stores. Read-only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: Id,
    /// The folder's name, e.g. "margin".
    pub name: String,
    /// The device holding the folder; only it can read it.
    pub device_id: Id,
    pub device_name: String,
    pub platform: Platform,
    /// Where the folder is, as the owner would recognise it ("~/Developer/margin",
    /// "iCloud Drive/Notes"). Display only; never used to read.
    pub location: String,
    pub created_at: DateTime<Utc>,
    /// The service's counts of the folder's files (it keeps them up to date).
    #[serde(default)]
    pub sync: WorkspaceSync,
    /// Files the last manifest listed that the device couldn't read (shown greyed out
    /// with their reason; the service keeps whatever it already has of them).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unreadable: Vec<UnreadableFile>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Macos,
    Linux,
    /// iOS and iPadOS.
    Ios,
    Android,
}

/// `POST routes::WORKSPACES`: the calling device registers a folder it holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewWorkspace {
    pub name: String,
    pub platform: Platform,
    pub location: String,
}

// ---------------------------------------------------------------------------
// Request and response bodies
// ---------------------------------------------------------------------------

/// `POST routes::CHATS`. With `message`, the chat is created and its first turn starts
/// in one request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NewChat {
    #[serde(default)]
    pub message: Option<NewMessage>,
    #[serde(default)]
    pub workspace_ids: Vec<Id>,
    /// The provider the chat runs on; `None` uses Settings' default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Provider>,
}

/// `POST routes::chat_messages(id)`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NewMessage {
    pub text: String,
    /// Uploaded sources to attach with this message.
    #[serde(default)]
    pub source_ids: Vec<Id>,
}

/// The answer to `POST routes::chat_messages(id)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageSent {
    pub chat: Chat,
    /// The `UserMessage` item when a turn started; `None` when the message became
    /// `chat.pending`.
    pub item: Option<ChatItem>,
}

/// `PATCH routes::chat(id)`. `None` leaves a field unchanged.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatPatch {
    pub title: Option<String>,
    /// Replaces the attached folders.
    pub workspace_ids: Option<Vec<Id>>,
    /// Replaces the attached files. Files no longer attached to the chat or a topic are
    /// deleted.
    pub source_ids: Option<Vec<Id>>,
}

/// `GET routes::chat(id)`: the chat and its whole transcript, ordered by `seq`. Also the
/// answer to `POST routes::CHATS` (with the first `UserMessage` when one was sent).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatDetail {
    pub chat: Chat,
    pub items: Vec<ChatItem>,
}

/// `POST routes::chat_confirm(id)`: answer a confirmation card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmRequest {
    /// The card's `ChatItem.id`.
    pub item_id: Id,
    pub approve: bool,
}

/// `GET routes::chat_item_output(chat_id, item_id)`: the full output of a tool result,
/// exactly as the model saw it (folder excerpts load this way, on demand).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutputText {
    pub text: String,
}

/// Overrides from the outline card's chips when Create lessons is pressed. `None`
/// keeps what the proposal asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutlineChips {
    pub level: Option<Level>,
    pub with_videos: Option<bool>,
    pub with_quiz: Option<bool>,
}

/// A file of an attached folder, as a citation names it (`Citation.file`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceFile {
    pub workspace_id: Id,
    /// Relative to the folder's root, `/`-separated.
    pub path: String,
}

/// What the service keeps about an attached folder's files (`Workspace.sync`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceSync {
    /// Files of the folder the service has (any status).
    pub files: u32,
    /// Their size as uploaded (originals; what reading them adds is not counted).
    pub bytes: u64,
    /// Files the last manifest listed that the service hasn't received yet.
    pub uploading: u32,
    /// Files received and still being read (Queued or Processing).
    pub reading: u32,
    /// Files that couldn't be read.
    pub failed: u32,
    /// When the holding device last sent a manifest; `None`: never synced.
    pub last_synced_at: Option<DateTime<Utc>>,
}

/// A file's stamp: what tells the service whether its copy is current. The device
/// hashes the file (SHA-256, streamed) so an in-place edit that keeps size and time is
/// still seen (owner Q5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStamp {
    pub bytes: u64,
    #[serde(default)]
    pub modified: Option<DateTime<Utc>>,
    /// Lower-case hex SHA-256 of the file's bytes.
    pub sha256: String,
}

/// One allow-listed file of a folder, as the holding device sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestFile {
    /// Relative to the folder's root, `/`-separated.
    pub path: String,
    pub kind: crate::FolderFileType,
    #[serde(flatten)]
    pub stamp: FileStamp,
}

/// `POST routes::workspace_sync(id)`: every allow-listed, visible file of the folder.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncManifest {
    pub files: Vec<ManifestFile>,
    /// Files the device saw but couldn't read this pass (evicted to iCloud, locked, no
    /// permission). The service keeps what it has of them: not removed, not asked for;
    /// they're sent once a later pass can read them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unreadable: Vec<UnreadableFile>,
}

/// A file of a folder the holding device lists but can't read right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnreadableFile {
    /// Relative to the folder's root, `/`-separated.
    pub path: String,
    pub reason: UnreadableReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnreadableReason {
    /// Only in the cloud (an iCloud file evicted from this device).
    NotDownloaded,
    /// No permission, locked, or the read failed.
    CantRead,
}

impl UnreadableReason {
    /// What the folder's file list shows next to the file.
    pub fn message(self) -> &'static str {
        match self {
            UnreadableReason::NotDownloaded => "Not on this device — download it to include it",
            UnreadableReason::CantRead => "Can't be read",
        }
    }
}

/// The answer to a manifest: the paths to upload (new or changed, in name order), how
/// many files were already current, and how many the service removed (deleted on the
/// device, or changed and so replaced).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncPlan {
    pub upload: Vec<String>,
    pub kept: u32,
    pub removed: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Event, SourceKind};
    use serde_json::json;

    fn at() -> DateTime<Utc> {
        DateTime::from_timestamp(1_760_000_000, 0).unwrap()
    }

    fn round_trip<T>(value: &T) -> serde_json::Value
    where
        T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_value(value).unwrap();
        let back: T = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(&back, value);
        json
    }

    fn chat(state: ChatState) -> Chat {
        Chat {
            id: "c1".into(),
            title: "Fourier".into(),
            title_by: TitleSource::Model,
            state,
            workspace_ids: vec!["w1".into()],
            source_ids: vec![],
            pending: Some(PendingMessage {
                text: "and then?".into(),
                source_ids: vec![],
                device_id: "d1".into(),
                created_at: at(),
            }),
            usage: Usage {
                input_tokens: 10,
                cached_input_tokens: 4,
                output_tokens: 5,
                reasoning_tokens: 2,
                model_calls: 1,
                ..Default::default()
            },
            waiting: Some(Waiting {
                until: at(),
                reason: "The model API asked Margin to slow down.".into(),
            }),
            provider: Provider::OpenAi,
            created_at: at(),
            updated_at: at(),
        }
    }

    fn item(kind: ChatItemKind) -> ChatItem {
        ChatItem {
            id: "i1".into(),
            chat_id: "c1".into(),
            turn_id: "t1".into(),
            seq: 3,
            status: ItemStatus::InProgress,
            created_at: at(),
            updated_at: at(),
            kind,
        }
    }

    #[test]
    fn chat_states_round_trip() {
        for state in [
            ChatState::Idle,
            ChatState::Running {
                turn_id: "t1".into(),
                device_id: "d1".into(),
                started_at: at(),
            },
            ChatState::WaitingForUser {
                turn_id: "t1".into(),
                device_id: "d1".into(),
                item_id: "i9".into(),
            },
            ChatState::Interrupted {
                turn_id: "t1".into(),
            },
            ChatState::Failed {
                turn_id: "t1".into(),
                error: "boom".into(),
            },
        ] {
            round_trip(&chat(state));
        }
        let json = round_trip(&ChatState::Idle);
        assert_eq!(json, json!({"state": "idle"}));
        assert!(
            ChatState::WaitingForUser {
                turn_id: "t".into(),
                device_id: "d".into(),
                item_id: "i".into()
            }
            .is_busy()
        );
        assert!(
            ChatState::Interrupted {
                turn_id: "t".into()
            }
            .can_continue()
        );
        assert!(!ChatState::Idle.is_busy());
        assert_eq!(ChatState::Idle.turn_id(), None);
    }

    #[test]
    fn old_chats_read_with_defaults() {
        let back: Chat = serde_json::from_value(json!({
            "id": "c", "title": "t", "state": {"state": "idle"},
            "created_at": "2025-10-09T08:53:20Z", "updated_at": "2025-10-09T08:53:20Z"
        }))
        .unwrap();
        assert!(back.workspace_ids.is_empty());
        assert_eq!(back.pending, None);
        assert_eq!(back.usage, Usage::default());
        assert_eq!(back.title_by, TitleSource::FirstLine);
        assert_eq!(back.waiting, None);
        let old_error: ChatItemKind =
            serde_json::from_value(json!({"kind": "error", "message": "m", "retryable": true}))
                .unwrap();
        assert_eq!(
            old_error,
            ChatItemKind::Error {
                message: "m".into(),
                retryable: true,
                code: None
            }
        );
        let json = serde_json::to_value(TitleSource::Owner).unwrap();
        assert_eq!(json, "owner");
    }

    #[test]
    fn providers_on_chats_and_web_items() {
        // A chat stored before providers is on OpenAI.
        let mut json = serde_json::to_value(chat(ChatState::Idle)).unwrap();
        json.as_object_mut().unwrap().remove("provider");
        let old: Chat = serde_json::from_value(json).unwrap();
        assert_eq!(old.provider, Provider::OpenAi);
        let mut on_claude = chat(ChatState::Idle);
        on_claude.provider = Provider::Claude;
        assert_eq!(round_trip(&on_claude)["provider"], "claude");

        // An answer without citations parses, and serializes without the field.
        let old: ChatItemKind =
            serde_json::from_value(serde_json::json!({"kind": "assistant_text", "text": "a"}))
                .unwrap();
        assert_eq!(
            old,
            ChatItemKind::AssistantText {
                text: "a".into(),
                citations: vec![]
            }
        );
        assert!(
            serde_json::to_value(&old)
                .unwrap()
                .get("citations")
                .is_none()
        );
        let cited = ChatItemKind::AssistantText {
            text: "Shannon was born in 1916.".into(),
            citations: vec![WebCitation {
                url: "https://en.wikipedia.org/wiki/Claude_Shannon".into(),
                title: Some("Claude Shannon".into()),
                cited_text: Some("born April 30, 1916".into()),
                start: Some(0),
                end: Some(25),
            }],
        };
        round_trip(&item(cited));

        let card = ChatItemKind::Card {
            call_id: "srvtoolu_1".into(),
            card: Card::WebResults(WebResultsCard {
                query: "claude shannon".into(),
                results: vec![WebResult {
                    url: "https://example.com".into(),
                    title: Some("Example".into()),
                    page_age: None,
                }],
            }),
        };
        let json = round_trip(&item(card));
        assert_eq!(json["card"]["type"], "web_results");

        let new: NewChat = serde_json::from_str("{}").unwrap();
        assert_eq!(new.provider, None);
        let new = NewChat {
            provider: Some(Provider::Claude),
            ..Default::default()
        };
        assert_eq!(serde_json::to_value(&new).unwrap()["provider"], "claude");

        let mut usage = Usage::default();
        usage.add(&Usage {
            cache_write_input_tokens: 3,
            web_search_requests: 2,
            web_fetch_requests: 1,
            ..Default::default()
        });
        assert_eq!(usage.web_search_requests, 2);
        let old: Usage = serde_json::from_value(serde_json::json!({
            "input_tokens": 1, "cached_input_tokens": 0, "output_tokens": 1,
            "reasoning_tokens": 0, "model_calls": 1
        }))
        .unwrap();
        assert_eq!(old.cache_write_input_tokens, 0);
        assert_eq!(error_code::ALL.len(), 14);
        assert!(error_code::ALL.contains(&error_code::AUTH_FAILED));
    }

    #[test]
    fn items_round_trip_flat() {
        let kinds = vec![
            ChatItemKind::UserMessage {
                text: "teach me".into(),
                source_ids: vec!["s1".into()],
                device_id: "d1".into(),
                device_name: "MacBook Pro".into(),
            },
            ChatItemKind::AssistantText {
                text: "Sure".into(),
                citations: vec![],
            },
            ChatItemKind::Reasoning {
                summary: String::new(),
            },
            ChatItemKind::ToolCall {
                call_id: "call_1".into(),
                name: "list_files".into(),
                arguments: r#"{"path":""}"#.into(),
                label: "Listing margin".into(),
                device: Some(DeviceRef {
                    device_id: "d1".into(),
                    name: "MacBook Pro".into(),
                }),
            },
            ChatItemKind::ToolResult {
                call_id: "call_1".into(),
                ok: true,
                summary: "12 files".into(),
                output_bytes: 900,
            },
            ChatItemKind::Card {
                call_id: "call_2".into(),
                card: Card::Outline(OutlineCard {
                    proposal_id: "p1".into(),
                }),
            },
            ChatItemKind::Card {
                call_id: "call_3".into(),
                card: Card::Result(ResultCard {
                    target: ResultTarget::Video {
                        lesson_id: "l1".into(),
                    },
                    title: "Lesson 1".into(),
                }),
            },
            ChatItemKind::Card {
                call_id: "call_4".into(),
                card: Card::Passages(PassagesCard {
                    query: "dirichlet".into(),
                    passages: vec![Citation {
                        n: 1,
                        source_id: "s1".into(),
                        source_name: "notes.pdf".into(),
                        kind: SourceKind::Pdf,
                        page: Some(3),
                        start_ms: None,
                        end_ms: None,
                        excerpt: "…".into(),
                        image_url: None,
                        document_id: None,
                        start_line: None,
                        end_line: None,
                        file: None,
                        section_path: None,
                    }],
                }),
            },
            ChatItemKind::Card {
                call_id: "call_5".into(),
                card: Card::Confirmation(ConfirmationCard {
                    tool: "delete_topic".into(),
                    title: "Delete the topic “Fourier series”?".into(),
                    detail: "3 lessons go with it.".into(),
                    approve_label: "Delete".into(),
                    decision: Decision::Denied {
                        device_id: "d2".into(),
                        by_stop: true,
                    },
                }),
            },
            ChatItemKind::Action {
                text: "Created the course.".into(),
            },
            ChatItemKind::Error {
                message: "The model API returned 500.".into(),
                retryable: true,
                code: Some(error_code::SERVER_ERROR.into()),
            },
        ];
        for kind in kinds {
            round_trip(&item(kind));
        }
        let json = round_trip(&item(ChatItemKind::AssistantText {
            text: "x".into(),
            citations: vec![],
        }));
        assert_eq!(json["kind"], "assistant_text");
        assert_eq!(json["status"], "in_progress");
        assert_eq!(json["text"], "x");
        let json = round_trip(&item(ChatItemKind::Card {
            call_id: "c".into(),
            card: Card::Outline(OutlineCard {
                proposal_id: "p".into(),
            }),
        }));
        assert_eq!(json["card"], json!({"type": "outline", "proposal_id": "p"}));
    }

    #[test]
    fn chat_events_round_trip() {
        let events = vec![
            Event::ChatChanged {
                chat: chat(ChatState::Idle),
            },
            Event::ChatDeleted { id: "c1".into() },
            Event::ChatItemChanged {
                item: item(ChatItemKind::AssistantText {
                    text: "a".into(),
                    citations: vec![],
                }),
            },
            Event::ChatItemDelta {
                chat_id: "c1".into(),
                item_id: "i1".into(),
                offset: 1,
                text: "b".into(),
            },
            Event::WorkspaceChanged {
                workspace: Workspace {
                    id: "w1".into(),
                    name: "margin".into(),
                    device_id: "d1".into(),
                    device_name: "MacBook Pro".into(),
                    platform: Platform::Macos,
                    location: "~/Developer/margin".into(),
                    created_at: at(),
                    sync: Default::default(),
                    unreadable: Vec::new(),
                },
            },
            Event::WorkspaceDeleted { id: "w1".into() },
            Event::DeviceChanged {
                device: crate::Device {
                    id: "d1".into(),
                    name: "iPhone".into(),
                    created_at: at(),
                    last_seen_at: None,
                    current: false,
                    online: true,
                },
            },
            Event::DeviceDeleted { id: "d1".into() },
        ];
        for e in events {
            round_trip(&e);
        }
        let json = round_trip(&Event::ChatDeleted { id: "c".into() });
        assert_eq!(json, json!({"type": "chat_deleted", "id": "c"}));
    }

    #[test]
    fn bodies_round_trip() {
        round_trip(&NewChat {
            message: Some(NewMessage {
                text: "hi".into(),
                source_ids: vec![],
            }),
            workspace_ids: vec!["w".into()],
            ..Default::default()
        });
        assert_eq!(
            serde_json::from_value::<NewChat>(json!({})).unwrap(),
            NewChat::default()
        );
        round_trip(&ChatPatch {
            title: Some("New".into()),
            workspace_ids: None,
            source_ids: Some(vec![]),
        });
        round_trip(&ConfirmRequest {
            item_id: "i".into(),
            approve: false,
        });
        round_trip(&MessageSent {
            chat: chat(ChatState::Idle),
            item: None,
        });
        round_trip(&ChatDetail {
            chat: chat(ChatState::Idle),
            items: vec![item(ChatItemKind::Action { text: "x".into() })],
        });
        round_trip(&NewWorkspace {
            name: "notes".into(),
            platform: Platform::Android,
            location: "Documents/notes".into(),
        });
        round_trip(&ToolOutputText { text: "out".into() });
        round_trip(&OutlineChips {
            level: Some(Level::Advanced),
            with_videos: Some(false),
            with_quiz: None,
        });
        round_trip(&WorkspaceFile {
            workspace_id: "w".into(),
            path: "notes/ch1.md".into(),
        });
    }

    #[test]
    fn deltas_apply_by_offset() {
        let mut text = ChatItemKind::AssistantText {
            text: "Hel".into(),
            citations: vec![],
        };
        assert_eq!(text.apply_delta(3, "lo"), DeltaOutcome::Applied);
        assert_eq!(text.streaming_text(), Some("Hello"));
        // Loaded after these were sent.
        assert_eq!(text.apply_delta(0, "He"), DeltaOutcome::AlreadyHad);
        assert_eq!(text.apply_delta(3, "lo"), DeltaOutcome::AlreadyHad);
        // Overlapping: only the new part is added.
        assert_eq!(text.apply_delta(4, "o, w"), DeltaOutcome::Applied);
        assert_eq!(text.streaming_text(), Some("Hello, w"));
        // Missed one.
        assert_eq!(text.apply_delta(20, "x"), DeltaOutcome::Gap);
        assert_eq!(text.streaming_text(), Some("Hello, w"));

        let mut call = ChatItemKind::ToolCall {
            call_id: "c".into(),
            name: "n".into(),
            arguments: "{".into(),
            label: String::new(),
            device: None,
        };
        assert_eq!(call.apply_delta(1, "}"), DeltaOutcome::Applied);
        assert_eq!(call.streaming_text(), Some("{}"));
        let mut error = ChatItemKind::Error {
            message: "x".into(),
            retryable: false,
            code: None,
        };
        assert_eq!(error.apply_delta(0, "y"), DeltaOutcome::NotStreaming);
    }

    #[test]
    fn usage_adds_up() {
        let mut total = Usage::default();
        let one = Usage {
            input_tokens: 3,
            cached_input_tokens: 1,
            output_tokens: 2,
            reasoning_tokens: 1,
            model_calls: 1,
            ..Default::default()
        };
        total.add(&one);
        total.add(&one);
        assert_eq!(total.input_tokens, 6);
        assert_eq!(total.model_calls, 2);
    }

    #[test]
    fn folder_files_in_citations_sources_and_workspaces() {
        // Citations stored before folders parse, and say nothing about a file.
        let old: crate::Citation = serde_json::from_value(json!({
            "n": 1, "source_id": "s", "source_name": "a.pdf", "kind": "pdf", "page": 3,
            "start_ms": null, "end_ms": null, "excerpt": "x", "image_url": null
        }))
        .unwrap();
        assert_eq!(old.file, None);
        assert_eq!(old.document_id, None);
        assert!(!serde_json::to_string(&old).unwrap().contains("file"));
        let cited = crate::Citation {
            file: Some(WorkspaceFile {
                workspace_id: "w".into(),
                path: "papers/a.pdf".into(),
            }),
            source_name: "notes/papers/a.pdf".into(),
            ..old
        };
        let back = round_trip(&cited);
        assert_eq!(
            back["file"],
            json!({"workspace_id": "w", "path": "papers/a.pdf"})
        );

        let source: crate::Source = serde_json::from_value(json!({
            "id": "s", "topic_id": null, "name": "a.md", "folder": null, "kind": "text",
            "bytes": 3, "status": {"state": "queued"}, "created_at": "2025-10-09T08:53:20Z"
        }))
        .unwrap();
        assert_eq!((source.workspace_id.clone(), source.modified), (None, None));
        round_trip(&crate::Source {
            workspace_id: Some("w".into()),
            modified: Some(at()),
            folder: Some("notes".into()),
            ..source
        });

        let ws: Workspace = serde_json::from_value(json!({
            "id": "w", "name": "notes", "device_id": "d", "device_name": "Mac",
            "platform": "macos", "location": "~/notes", "created_at": "2025-10-09T08:53:20Z"
        }))
        .unwrap();
        assert_eq!(ws.sync, WorkspaceSync::default());
        round_trip(&Workspace {
            sync: WorkspaceSync {
                files: 3,
                bytes: 10,
                uploading: 1,
                reading: 1,
                failed: 0,
                last_synced_at: Some(at()),
            },
            ..ws
        });

        let manifest = SyncManifest {
            files: vec![ManifestFile {
                path: "papers/a.pdf".into(),
                kind: crate::FolderFileType::Pdf,
                stamp: FileStamp {
                    bytes: 12,
                    modified: Some(at()),
                    sha256: "ab".repeat(32),
                },
            }],
            unreadable: vec![UnreadableFile {
                path: "papers/evicted.pdf".into(),
                reason: UnreadableReason::NotDownloaded,
            }],
        };
        let json = round_trip(&manifest);
        // An older device sends no `unreadable`.
        let old: SyncManifest = serde_json::from_value(json!({"files": []})).unwrap();
        assert!(old.unreadable.is_empty());
        assert_eq!(json["unreadable"][0]["reason"], "not_downloaded");
        // The stamp is flattened into the file.
        assert_eq!(json["files"][0]["bytes"], 12);
        assert_eq!(json["files"][0]["kind"], "pdf");
        round_trip(&SyncPlan {
            upload: vec!["papers/a.pdf".into()],
            kept: 2,
            removed: 1,
        });
    }
}
