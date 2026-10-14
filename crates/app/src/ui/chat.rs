//! Ask: chats with the agent. The transcript (the owner's messages, the answer streaming
//! in as Markdown, reasoning summaries, tool calls with their output loaded on demand,
//! inline cards, errors) and the composer (send, Stop, Continue or Retry, and the message
//! waiting for the running answer). The agent runs on the service; every device shows
//! the same transcript live from its events.

use dioxus::prelude::*;
use margin_shared::{
    AcceptProposal, Card, Chat, ChatItem, ChatItemKind, ChatState, ConfirmRequest,
    ConfirmationCard, Decision, Event, Id, ItemStatus, LessonStatus, NewChat, NewMessage, Outline,
    PassagesCard, ProposalStatus, ResultCard, ResultTarget, VideoStatus,
};

use crate::api::ApiFailure;
use crate::citations::{self as cite, kind_label, locator};
use crate::markdown;
use crate::routes::{Route, chat_title};
use crate::state::{AppCtx, LoadStatus, use_app};
use crate::ui::citations::CitationViewer;
use crate::ui::common::{ErrorNotice, Icon, Loading, TopBar, focus_first, js, reveal_topic};
use crate::ui::outline::ProposalView;
use crate::ui::settings::SettingsPane;
use crate::ui::sources::kind_icon;

/// The composer's text field, focused by Cmd+N (`ui::shell`).
pub const PROMPT_ID: &str = "chat-prompt";
/// The transcript's scroll box, kept at its end while the answer streams in.
const LOG_ID: &str = "chat-log";

/// Prompts to start from while the library is empty; a click puts one in the composer.
const EXAMPLE_PROMPTS: [&str; 3] = [
    "How do Fourier series approximate a signal?",
    "Linear algebra for machine learning",
    "How does TCP avoid congestion?",
];

/// One row of the transcript, built from its items by [`rows`].
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    User(ChatItem),
    Text(ChatItem),
    Reasoning(ChatItem),
    /// A tool call and, once it has one, its result (a result whose call isn't in the
    /// transcript stands alone).
    Tool {
        call: Option<ChatItem>,
        result: Option<ChatItem>,
    },
    Card(ChatItem),
    Action(ChatItem),
    Error {
        item: ChatItem,
        /// Retry is offered on the last error while the chat is failed.
        retry: bool,
    },
}

impl Row {
    /// A key that stays with the row as items arrive.
    pub fn key(&self) -> &str {
        match self {
            Row::User(i)
            | Row::Text(i)
            | Row::Reasoning(i)
            | Row::Card(i)
            | Row::Action(i)
            | Row::Error { item: i, .. } => &i.id,
            Row::Tool { call, result } => call
                .as_ref()
                .or(result.as_ref())
                .map(|i| i.id.as_str())
                .unwrap_or_default(),
        }
    }
}

/// The transcript's rows, in order: each tool result joins its call's row, an empty
/// finished reasoning summary and empty actions show nothing, and the last error offers
/// Retry while the chat is failed and the error may go away.
pub fn rows(items: &[ChatItem], state: &ChatState) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let last_error = items
        .iter()
        .rposition(|i| matches!(i.kind, ChatItemKind::Error { .. }));
    // Retry resumes the failed turn, so only that turn's error offers it.
    let failed_turn = match state {
        ChatState::Failed { turn_id, .. } => Some(turn_id.as_str()),
        _ => None,
    };
    for (at, item) in items.iter().enumerate() {
        match &item.kind {
            ChatItemKind::UserMessage { .. } => rows.push(Row::User(item.clone())),
            ChatItemKind::AssistantText { .. } => rows.push(Row::Text(item.clone())),
            ChatItemKind::Reasoning { summary } => {
                if !summary.trim().is_empty() || item.status == ItemStatus::InProgress {
                    rows.push(Row::Reasoning(item.clone()));
                }
            }
            ChatItemKind::ToolCall { .. } => rows.push(Row::Tool {
                call: Some(item.clone()),
                result: None,
            }),
            ChatItemKind::ToolResult { call_id, .. } => {
                let call = rows.iter_mut().rev().find_map(|r| match r {
                    Row::Tool {
                        call: Some(c),
                        result,
                    } if result.is_none() && call_id_of(c) == Some(call_id.as_str()) => {
                        Some(result)
                    }
                    _ => None,
                });
                match call {
                    Some(slot) => *slot = Some(item.clone()),
                    None => rows.push(Row::Tool {
                        call: None,
                        result: Some(item.clone()),
                    }),
                }
            }
            ChatItemKind::Card { .. } => rows.push(Row::Card(item.clone())),
            ChatItemKind::Action { text } => {
                if !text.trim().is_empty() {
                    rows.push(Row::Action(item.clone()));
                }
            }
            ChatItemKind::Error { retryable, .. } => rows.push(Row::Error {
                item: item.clone(),
                retry: failed_turn == Some(item.turn_id.as_str())
                    && *retryable
                    && Some(at) == last_error,
            }),
        }
    }
    rows
}

fn call_id_of(item: &ChatItem) -> Option<&str> {
    match &item.kind {
        ChatItemKind::ToolCall { call_id, .. } => Some(call_id),
        _ => None,
    }
}

/// A tool's name as words, for a call whose own label hasn't arrived yet:
/// "search_library" → "Search library".
pub fn tool_words(name: &str) -> String {
    crate::ui::common::capitalize_first(&name.replace('_', " "))
}

/// The icon of a tool's row.
fn tool_icon(name: &str) -> &'static str {
    match name {
        n if n.starts_with("search_") => "search",
        "list_topics" | "get_topic" | "create_topic" | "rename_topic" | "move_topic" => "topic",
        "read_lesson" | "write_lesson" | "regenerate_lesson" | "edit_lesson_title" => "book",
        n if n.contains("note") => "note",
        "list_sources" | "attach_sources" => "clip",
        "make_video" => "video",
        n if n.contains("file") => "folder",
        _ => "wrench",
    }
}

/// The first line of a reasoning summary without its Markdown emphasis: the models
/// start each summary with a bold heading ("**Checking the library first**").
pub fn reasoning_title(summary: &str) -> Option<String> {
    let line = summary.trim().lines().next()?.trim();
    let line = line
        .trim_matches(|c| c == '*' || c == '_' || c == '#')
        .trim();
    (!line.is_empty()).then(|| line.to_string())
}

/// Arguments as the model wrote them, indented for reading (as they are when they don't
/// parse yet).
pub fn pretty_json(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| text.to_string())
}

/// "2.4 KB", for the size of a tool's output.
pub fn size_text(bytes: u64) -> String {
    match bytes {
        b if b < 1000 => format!("{b} bytes"),
        b if b < 1_000_000 => format!("{:.1} KB", b as f64 / 1000.0),
        b => format!("{:.1} MB", b as f64 / 1_000_000.0),
    }
}

/// Ask the service for the chat's transcript and merge it into the store (events for it
/// apply from now on). `Err(true)` when the chat no longer exists.
async fn fetch_chat(app: AppCtx, id: Id) -> Result<(), (bool, ApiFailure)> {
    let Some(api) = app.api() else { return Ok(()) };
    let mut lib = app.lib;
    if !lib.peek().chat_items.contains_key(&id) {
        lib.write().chat_items.insert(id.clone(), Vec::new());
    }
    match api.chat(&id).await {
        Ok(detail) => {
            lib.write().load_chat(detail);
            Ok(())
        }
        Err(e) => {
            let gone = e.status == Some(404);
            let mut lib = lib.write();
            lib.stale_chats.remove(&id);
            if gone {
                lib.chats.remove(&id);
                lib.chat_items.remove(&id);
            }
            drop(lib);
            app.failure(&e);
            Err((gone, e))
        }
    }
}

/// Ask (no `id`: a new chat, created by its first message) or one chat.
#[component]
pub fn ChatScreen(id: Option<Id>) -> Element {
    let app = use_app();
    let nav = navigator();
    // The new chat's draft lives in the app, so it survives a look elsewhere.
    let own_draft = use_signal(String::new);
    let mut draft = if id.is_none() {
        app.ask_prompt
    } else {
        own_draft
    };
    // Bumped when code replaces the composer text, to remount the (uncontrolled) textarea.
    let mut draft_rev = use_signal(|| 0u64);
    let mut sending = use_signal(|| false);
    let mut send_error = use_signal(|| None::<ApiFailure>);
    let mut loading = use_signal(|| false);
    let mut load_error = use_signal(|| None::<ApiFailure>);
    let mut gone = use_signal(|| false);
    // Stop, Continue or Retry in flight.
    let mut acting = use_signal(|| false);
    let mut act_error = use_signal(|| None::<ApiFailure>);

    // Whether an API key is set, to say so before a message is sent rather than after
    // (SettingsChanged events keep it current).
    use_future(move || async move {
        if app.lib.peek().settings.is_some() {
            return;
        }
        let Some(api) = app.api() else { return };
        if let Ok(settings) = api.settings().await {
            let mut lib = app.lib;
            lib.write().apply(Event::SettingsChanged { settings });
        }
    });

    // Fetches the transcript (again), for the effect below and the Retry buttons.
    let reload = use_callback(move |id: Id| {
        spawn(async move {
            loading.set(true);
            match fetch_chat(app, id).await {
                Ok(()) => load_error.set(None),
                Err((true, _)) => gone.set(true),
                Err((false, e)) => load_error.set(Some(e)),
            }
            loading.set(false);
        });
    });

    // Load the transcript when it isn't here, and again when it missed an event.
    let needs_load = id.as_ref().is_some_and(|id| {
        let lib = app.lib.read();
        !lib.chat_items.contains_key(id) || lib.stale_chats.contains(id)
    });
    let load_id = id.clone();
    use_effect(use_reactive!(|needs_load| {
        let Some(id) = load_id.clone() else { return };
        if !needs_load || *loading.peek() {
            return;
        }
        reload.call(id);
    }));

    // Keep the transcript at its end while the answer streams in (unless scrolled up).
    let following = id.is_some();
    use_effect(move || {
        if following {
            let _ = js(&format!("MarginApp.followChat('{LOG_ID}');"));
        }
    });

    let lib = app.lib.read();
    let chat: Option<Chat> = id.as_ref().and_then(|id| lib.chats.get(id).cloned());
    let items: Vec<ChatItem> = id
        .as_ref()
        .and_then(|id| lib.chat_items.get(id).cloned())
        .unwrap_or_default();
    let no_key = lib.settings.as_ref().is_some_and(|s| !s.has_api_key);
    let empty_library = lib.topics.is_empty() && *app.lib_status.read() == LoadStatus::Loaded;
    let chats_loaded = lib.chats_loaded;
    drop(lib);

    let busy = chat.as_ref().is_some_and(|c| c.state.is_busy());
    let waiting = chat.as_ref().is_some_and(|c| c.pending.is_some());
    let send = use_callback({
        let id = id.clone();
        move |()| {
            let text = draft.peek().trim().to_string();
            if text.is_empty() || *sending.peek() || no_key || waiting {
                return;
            }
            let id = id.clone();
            spawn(async move {
                let Some(api) = app.api() else { return };
                sending.set(true);
                send_error.set(None);
                let message = NewMessage {
                    text,
                    source_ids: vec![],
                };
                let mut lib = app.lib;
                match id {
                    None => {
                        let body = NewChat {
                            message: Some(message),
                            workspace_ids: vec![],
                        };
                        match api.create_chat(&body).await {
                            Ok(detail) => {
                                let id = detail.chat.id.clone();
                                let mut l = lib.write();
                                l.load_chat(detail);
                                // Item events sent before this answer arrived weren't
                                // applied (the transcript wasn't here yet): fetch it once
                                // more when the chat opens.
                                l.stale_chats.insert(id.clone());
                                drop(l);
                                draft.set(String::new());
                                draft_rev += 1;
                                sending.set(false);
                                nav.replace(Route::Chat { id });
                                return;
                            }
                            Err(e) => {
                                app.failure(&e);
                                send_error.set(Some(e));
                            }
                        }
                    }
                    Some(id) => match api.send_message(&id, &message).await {
                        Ok(sent) => {
                            let mut lib = lib.write();
                            lib.upsert_chat(sent.chat);
                            if let Some(item) = sent.item {
                                lib.upsert_chat_item(item);
                            }
                            drop(lib);
                            draft.set(String::new());
                            draft_rev += 1;
                            let _ = js(&format!("MarginApp.chatToEnd('{LOG_ID}');"));
                        }
                        Err(e) => {
                            app.failure(&e);
                            send_error.set(Some(e));
                        }
                    },
                }
                sending.set(false);
            });
        }
    });

    // Stop, Continue / Retry and dropping the pending message: each answers with the chat.
    let act = use_callback({
        let id = id.clone();
        move |action: ChatAction| {
            let Some(id) = id.clone() else { return };
            if *acting.peek() {
                return;
            }
            spawn(async move {
                let Some(api) = app.api() else { return };
                acting.set(true);
                act_error.set(None);
                let result = match action {
                    ChatAction::Stop => api.stop_chat(&id).await,
                    ChatAction::Continue => api.continue_chat(&id).await,
                    ChatAction::DropPending => api.drop_pending(&id).await,
                };
                match result {
                    Ok(chat) => {
                        let mut lib = app.lib;
                        lib.write().upsert_chat(chat);
                    }
                    // Someone else got there first (another device stopped or
                    // continued it): show the chat as it is now.
                    Err(e) if e.status == Some(409) => {
                        let _ = fetch_chat(app, id).await;
                    }
                    Err(e) => {
                        app.failure(&e);
                        act_error.set(Some(e));
                    }
                }
                acting.set(false);
            });
        }
    });

    let draft_empty = draft.read().trim().is_empty();
    let is_sending = *sending.read();
    let stopping = *acting.read();
    let chat_open = id.is_some();
    let composer = move |placeholder: &'static str| {
        let can_send = !draft_empty && !is_sending && !no_key && !waiting;
        let send_title = if no_key {
            "Add an API key in Settings to ask"
        } else if waiting {
            "A message is already waiting for this answer to finish"
        } else if busy {
            "Send when the answer finishes (Enter)"
        } else {
            "Send (Enter)"
        };
        rsx! {
            Composer {
                draft,
                rev: draft_rev,
                placeholder,
                docked: chat_open,
                can_send,
                send_title,
                no_key,
                sending: is_sending,
                busy,
                stopping,
                on_send: move |_| send.call(()),
                on_stop: move |_| act.call(ChatAction::Stop),
            }
        }
    };
    let notices = rsx! {
        if no_key {
            div { id: "chat-no-key", class: "notice", role: "status",
                Icon { name: "alert" }
                div { class: "body",
                    span { "Add an API key to ask. Margin needs one for your AI provider before it can answer." }
                    div { class: "row-actions",
                        button {
                            class: "btn btn-secondary btn-sm",
                            onclick: move |_| {
                                let mut open = app.settings_open;
                                open.set(Some(SettingsPane::Ai));
                            },
                            Icon { name: "gear" }
                            "Open Settings → AI"
                        }
                    }
                }
            }
        }
        if let Some(err) = send_error.read().clone() {
            ErrorNotice { message: err.message.clone(), settings_pane: err.settings_pane() }
        }
        if let Some(err) = act_error.read().clone() {
            ErrorNotice { message: err.message.clone(), settings_pane: err.settings_pane() }
        }
    };

    // A new chat: the question first, as Ask always was.
    let Some(id) = id else {
        let show_examples = empty_library && draft_empty;
        return rsx! {
            TopBar { crumbs: vec!["New chat".to_string()] }
            div { class: "scroll",
                div { class: "gen",
                    h1 { "What do you want to learn?" }
                    {composer("A topic, a question, or what you already know…")}
                    if show_examples {
                        div { class: "examples", role: "group", "aria-label": "Example prompts",
                            for example in EXAMPLE_PROMPTS {
                                button {
                                    key: "{example}",
                                    class: "chip",
                                    onclick: move |_| {
                                        draft.set(example.to_string());
                                        draft_rev += 1;
                                        focus_first(&format!("#{PROMPT_ID}"));
                                    },
                                    "{example}"
                                }
                            }
                        }
                    }
                    {notices}
                }
            }
        };
    };

    let Some(chat) = chat else {
        // Deleted here or elsewhere, or never there.
        let deleted =
            *gone.read() || (chats_loaded && !*loading.read() && load_error.read().is_none());
        return rsx! {
            TopBar { crumbs: vec!["Chat".to_string()] }
            div { class: "scroll",
                if let Some(err) = load_error.read().clone() {
                    div { class: "gen",
                        ErrorNotice {
                            message: format!("Couldn't open this chat. {}", err.message),
                            on_retry: move |_| reload.call(id.clone()),
                        }
                    }
                } else if deleted {
                    div { class: "empty",
                        h2 { "This chat was deleted" }
                        p { "It was deleted here or on another device." }
                        button {
                            class: "btn btn-primary",
                            onclick: move |_| {
                                nav.replace(Route::Ask {});
                            },
                            "New chat"
                        }
                    }
                } else {
                    Loading { label: "Opening the chat…" }
                }
            }
        };
    };

    let title = chat_title(&chat.title);
    let rows = rows(&items, &chat.state);
    let first_load = *loading.read() && items.is_empty();
    let streaming = items.iter().any(|i| i.status == ItemStatus::InProgress);
    let running = matches!(chat.state, ChatState::Running { .. });
    // Failed without an error item to retry from (it couldn't be stored).
    let bare_failure = match &chat.state {
        ChatState::Failed { turn_id, error } => (!items
            .iter()
            .any(|i| &i.turn_id == turn_id && matches!(i.kind, ChatItemKind::Error { .. })))
        .then(|| error.clone()),
        _ => None,
    };
    let interrupted = matches!(chat.state, ChatState::Interrupted { .. });
    let pending = chat.pending.clone();
    let acting_now = *acting.read();

    rsx! {
        TopBar { crumbs: vec![title] }
        div { id: "{LOG_ID}", class: "scroll chat-scroll",
            div { class: "chat-col chat-log", role: "log", "aria-label": "Conversation",
                if first_load {
                    Loading { label: "Opening the chat…" }
                }
                if let Some(err) = load_error.read().clone() {
                    ErrorNotice {
                        message: format!("Couldn't load the whole chat. {}", err.message),
                        on_retry: {
                            let id = id.clone();
                            move |_| reload.call(id.clone())
                        },
                    }
                }
                for row in rows {
                    ChatRow { key: "{row.key()}", row: row.clone(), chat: chat.clone(), on_retry: move |_| act.call(ChatAction::Continue) }
                }
                if running && !streaming {
                    div { class: "chat-working", role: "status",
                        span { class: "spinner xs" }
                        "Working…"
                    }
                }
                if let Some(error) = bare_failure {
                    ErrorNotice {
                        message: format!("The answer stopped on an error. {error}"),
                        on_retry: move |_| act.call(ChatAction::Continue),
                    }
                }
                if interrupted {
                    div { class: "notice", role: "status",
                        Icon { name: "alert" }
                        div { class: "body",
                            span { "The service stopped before this answer finished." }
                            div { class: "row-actions",
                                button {
                                    class: "btn btn-secondary btn-sm",
                                    disabled: acting_now,
                                    onclick: move |_| act.call(ChatAction::Continue),
                                    if acting_now { span { class: "spinner" } }
                                    "Continue"
                                }
                            }
                        }
                    }
                }
            }
        }
        div { class: "chat-foot",
            div { class: "chat-col",
                if let Some(p) = pending {
                    div { class: "pending", role: "status",
                        div { class: "grow",
                            small { "Will send when the answer finishes" }
                            p { "{p.text}" }
                        }
                        button {
                            class: "icon-btn sm",
                            "aria-label": "Don't send this message",
                            title: "Don't send",
                            disabled: acting_now,
                            onclick: move |_| act.call(ChatAction::DropPending),
                            Icon { name: "x" }
                        }
                    }
                }
                {notices}
                {composer("Ask a follow-up…")}
            }
        }
    }
}

/// What the chat's own buttons ask the service for.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ChatAction {
    Stop,
    Continue,
    DropPending,
}

/// The message field with Send, and Stop while an answer runs. Enter sends, Shift+Enter
/// starts a new line.
#[component]
fn Composer(
    draft: Signal<String>,
    rev: Signal<u64>,
    placeholder: &'static str,
    /// Under a transcript: one line that grows with the text.
    docked: bool,
    can_send: bool,
    send_title: &'static str,
    /// Without an API key the send button points at the notice saying so.
    no_key: bool,
    sending: bool,
    busy: bool,
    stopping: bool,
    on_send: EventHandler<()>,
    on_stop: EventHandler<()>,
) -> Element {
    rsx! {
        div { class: "composer",
            for rev in std::iter::once(rev()) {
                textarea {
                    key: "{rev}",
                    id: PROMPT_ID,
                    rows: if docked { 1 } else { 2 },
                    // Ready to type on arrival, where there's a mouse or trackpad (on a
                    // touch screen it would pop up the keyboard every visit).
                    onmounted: move |_| {
                        let _ = js(&format!("MarginApp.focus('#{PROMPT_ID}', true);"));
                    },
                    "aria-label": "Message",
                    placeholder,
                    initial_value: "{draft}",
                    oninput: move |e| draft.set(e.value()),
                    onkeydown: move |e: KeyboardEvent| {
                        if e.key() == Key::Enter && !e.modifiers().shift() && !e.is_composing() {
                            e.prevent_default();
                            on_send.call(());
                        }
                    },
                }
            }
            div { class: "composer-row",
                if busy {
                    button {
                        class: "btn btn-secondary btn-sm stop",
                        title: "Stop the answer",
                        disabled: stopping,
                        onclick: move |_| on_stop.call(()),
                        if stopping {
                            span { class: "spinner" }
                        } else {
                            Icon { name: "stop" }
                        }
                        "Stop"
                    }
                }
                button {
                    class: "send",
                    "aria-label": "Send",
                    title: send_title,
                    "aria-describedby": no_key.then_some("chat-no-key"),
                    disabled: !can_send,
                    onclick: move |_| on_send.call(()),
                    if sending {
                        span { class: "spinner" }
                    } else {
                        Icon { name: "up" }
                    }
                }
            }
        }
    }
}

/// One row of the transcript.
#[component]
fn ChatRow(row: Row, chat: Chat, on_retry: EventHandler<()>) -> Element {
    match row {
        Row::User(item) => rsx! { UserMessage { item } },
        Row::Text(item) => {
            let ChatItemKind::AssistantText { text } = &item.kind else {
                return rsx! {};
            };
            rsx! {
                AnswerText {
                    item_id: item.id.clone(),
                    text: text.clone(),
                    status: item.status,
                }
            }
        }
        Row::Reasoning(item) => rsx! { ReasoningRow { item } },
        Row::Tool { call, result } => rsx! { ToolRow { chat_id: chat.id.clone(), call, result } },
        Row::Card(item) => rsx! { CardRow { item, chat } },
        Row::Action(item) => {
            let ChatItemKind::Action { text } = &item.kind else {
                return rsx! {};
            };
            rsx! { p { class: "chat-action", "{text}" } }
        }
        Row::Error { item, retry } => {
            let ChatItemKind::Error { message, .. } = &item.kind else {
                return rsx! {};
            };
            if retry {
                rsx! { ErrorNotice { message: message.clone(), on_retry: move |_| on_retry.call(()) } }
            } else {
                rsx! { ErrorNotice { message: message.clone() } }
            }
        }
    }
}

#[component]
fn UserMessage(item: ChatItem) -> Element {
    let app = use_app();
    let ChatItemKind::UserMessage {
        text,
        device_id,
        device_name,
        ..
    } = &item.kind
    else {
        return rsx! {};
    };
    // Which device sent it, when it wasn't this one.
    let mine = app
        .session
        .read()
        .as_ref()
        .and_then(|s| s.me.as_ref().map(|m| &m.device.id == device_id))
        .unwrap_or(true);
    rsx! {
        div { class: "msg-user",
            if !mine {
                small { "From {device_name}" }
            }
            div { class: "bubble", "{text}" }
        }
    }
}

/// The answer's text as Markdown. While it streams in, blocks that didn't change keep
/// their (typeset) DOM, so math doesn't flash as raw TeX.
#[component]
fn AnswerText(item_id: Id, text: String, status: ItemStatus) -> Element {
    let html = markdown::render(&text);
    let dom_id = format!("md-{item_id}");
    let streaming = status == ItemStatus::InProgress;
    let effect_id = dom_id.clone();
    let effect_html = html.clone();
    use_effect(use_reactive!(|effect_html, streaming| {
        if streaming {
            let id = serde_json::to_string(&effect_id).unwrap_or_default();
            let html = serde_json::to_string(&effect_html).unwrap_or_default();
            let _ = js(&format!("MarginApp.setHtml({id}, {html});"));
        } else {
            crate::ui::common::render_math(&effect_id);
        }
    }));
    rsx! {
        div { class: "msg-answer",
            if streaming {
                div { id: "{dom_id}", class: "md chat-md", "aria-busy": "true" }
            } else {
                div { id: "{dom_id}", class: "md chat-md", dangerous_inner_html: "{html}" }
            }
            if status == ItemStatus::Cancelled {
                p { class: "chat-cut", "Stopped before it finished." }
            }
        }
    }
}

/// A summary of the model's reasoning, folded away under its first line.
#[component]
fn ReasoningRow(item: ChatItem) -> Element {
    let ChatItemKind::Reasoning { summary } = &item.kind else {
        return rsx! {};
    };
    let thinking = item.status == ItemStatus::InProgress;
    let headline = reasoning_title(summary);
    let html = markdown::render(summary);
    rsx! {
        details { class: "collapse reasoning",
            summary {
                Icon { name: "chev", class: "chev" }
                if thinking {
                    span { class: "spinner xs", "aria-hidden": "true" }
                    "Thinking"
                    if let Some(h) = headline { span { class: "faint", " · {h}" } }
                } else {
                    "Thought"
                    if let Some(h) = headline { span { class: "faint", " · {h}" } }
                }
            }
            div { class: "md reasoning-md", dangerous_inner_html: "{html}" }
        }
    }
}

/// A tool call: one line (what it does, and how it went) that opens to show the
/// arguments and, loaded only then, everything the tool returned.
#[component]
fn ToolRow(chat_id: Id, call: Option<ChatItem>, result: Option<ChatItem>) -> Element {
    let app = use_app();
    let mut open = use_signal(|| false);
    let mut output = use_signal(|| None::<Result<String, ApiFailure>>);
    let mut fetching = use_signal(|| false);

    let (name, arguments, label, device) = match call.as_ref().map(|c| &c.kind) {
        Some(ChatItemKind::ToolCall {
            name,
            arguments,
            label,
            device,
            ..
        }) => (
            name.clone(),
            arguments.clone(),
            label.clone(),
            device.clone(),
        ),
        _ => (String::new(), String::new(), String::new(), None),
    };
    let outcome = result.as_ref().and_then(|r| match &r.kind {
        ChatItemKind::ToolResult {
            ok,
            summary,
            output_bytes,
            ..
        } => Some((*ok, summary.clone(), *output_bytes)),
        _ => None,
    });
    let cancelled = call
        .as_ref()
        .is_some_and(|c| c.status == ItemStatus::Cancelled);
    let running = outcome.is_none() && !cancelled;
    let line = if !label.is_empty() {
        label.clone()
    } else if name.is_empty() {
        "A tool".to_string()
    } else {
        tool_words(&name)
    };
    let result_id = result.as_ref().map(|r| r.id.clone());
    let row_id = call
        .as_ref()
        .or(result.as_ref())
        .map(|i| i.id.clone())
        .unwrap_or_default();
    let fetch = move |result_id: Id| {
        let chat_id = chat_id.clone();
        spawn(async move {
            let Some(api) = app.api() else { return };
            fetching.set(true);
            let got = api.chat_item_output(&chat_id, &result_id).await;
            if let Err(e) = &got {
                app.failure(e);
            }
            output.set(Some(got));
            fetching.set(false);
        });
    };
    // A row opened while its call ran loads the output once the result arrives.
    let fetch_when_done = fetch.clone();
    use_effect(use_reactive!(|result_id| {
        if let Some(id) = result_id.clone()
            && *open.peek()
            && output.peek().is_none()
            && !*fetching.peek()
        {
            fetch_when_done(id);
        }
    }));
    let toggle = {
        let result_id = result_id.clone();
        let fetch = fetch.clone();
        move |_| {
            let opening = !*open.peek();
            open.set(opening);
            // The output loads the first time the row opens (or after a failure).
            if opening
                && let Some(id) = result_id.clone()
                && !matches!(*output.peek(), Some(Ok(_)))
                && !*fetching.peek()
            {
                fetch(id);
            }
        }
    };
    let class = match &outcome {
        _ if running => "tool-row run",
        Some((false, ..)) => "tool-row bad",
        _ if cancelled => "tool-row bad",
        _ => "tool-row",
    };
    let body_id = format!("tool-{row_id}");
    let state_text = match (&outcome, cancelled) {
        (Some((_, summary, _)), _) if !summary.is_empty() => summary.clone(),
        (Some((true, ..)), _) => "Done".to_string(),
        (Some((false, ..)), _) => "Failed".to_string(),
        (None, true) => "Cancelled".to_string(),
        (None, false) => String::new(),
    };

    rsx! {
        div { class: "{class}",
            button {
                class: "tool-head",
                "aria-expanded": open.read().to_string(),
                "aria-controls": "{body_id}",
                onclick: toggle,
                Icon { name: tool_icon(&name), class: "tool-ic" }
                span { class: "tool-label", title: "{line}", "{line}" }
                if let Some(d) = &device {
                    span { class: "tool-device", "on {d.name}" }
                }
                if running {
                    span { class: "spinner xs", "aria-hidden": "true" }
                    span { class: "sr-only", "running" }
                } else {
                    if matches!(outcome, Some((false, ..))) || cancelled {
                        Icon { name: "alert", class: "tool-bad" }
                    }
                    span { class: "tool-state", "{state_text}" }
                }
                Icon { name: "chev", class: "chev" }
            }
            if *open.read() {
                div { id: "{body_id}", class: "tool-body",
                    if !name.is_empty() {
                        div { class: "tool-sec",
                            h4 { "Arguments" span { class: "mono faint", " · {name}" } }
                            pre { class: "code", "{pretty_json(&arguments)}" }
                        }
                    }
                    if let (Some(id), Some((_, _, bytes))) = (result_id.clone(), outcome.clone()) {
                        div { class: "tool-sec",
                            h4 { "Output" span { class: "faint", " · {size_text(bytes)}" } }
                            match output.read().clone() {
                                Some(Ok(text)) => rsx! { pre { class: "code", "{pretty_json(&text)}" } },
                                Some(Err(e)) => rsx! {
                                    ErrorNotice {
                                        message: format!("Couldn't load the output. {}", e.message),
                                        on_retry: {
                                            let fetch = fetch.clone();
                                            move |_| fetch(id.clone())
                                        },
                                    }
                                },
                                None => rsx! { Loading { label: "Loading the output…" } },
                            }
                        }
                    }
                }
            }
        }
    }
}

/// An inline card a tool made.
#[component]
fn CardRow(item: ChatItem, chat: Chat) -> Element {
    let ChatItemKind::Card { card, .. } = &item.kind else {
        return rsx! {};
    };
    match card.clone() {
        Card::Passages(card) => rsx! { PassagesView { item_id: item.id.clone(), card } },
        Card::Result(card) => rsx! { ResultView { card } },
        Card::Confirmation(card) => {
            rsx! { ConfirmationView { item_id: item.id.clone(), card, chat } }
        }
        Card::Outline(card) => rsx! { OutlineView { proposal_id: card.proposal_id } },
    }
}

/// Source passages a search found; each opens in the source viewer.
#[component]
fn PassagesView(item_id: Id, card: PassagesCard) -> Element {
    let mut viewing = use_signal(|| None::<u32>);
    let passages = cite::ordered(&card.passages);
    let card_id = format!("card-{item_id}");
    rsx! {
        section { id: "{card_id}", class: "chat-card", "aria-label": "Passages for {card.query}",
            div { class: "chat-card-h",
                Icon { name: "clip" }
                b { "Passages for “{card.query}”" }
            }
            ol { class: "cite-list",
                for c in passages.clone() {
                    li { key: "{c.n}",
                        button {
                            class: "cite-item",
                            "data-n": "{c.n}",
                            "aria-label": "{cite::marker_label(c.n, &c.source_name)}",
                            onclick: move |_| viewing.set(Some(c.n)),
                            span { class: "cite-n", "{c.n}" }
                            Icon { name: kind_icon(c.kind), class: "cite-ic" }
                            span { class: "cite-main",
                                span { class: "cite-name", "{c.source_name}" }
                                span { class: "cite-meta",
                                    "{kind_label(c.kind)}"
                                    if let Some(at) = locator(&c) { " · {at}" }
                                }
                                if !c.excerpt.trim().is_empty() {
                                    span { class: "cite-excerpt", title: "{c.excerpt.trim()}", "{c.excerpt.trim()}" }
                                }
                            }
                            if let Some(url) = c.image_url.clone() {
                                img { class: "cite-thumb", src: "{url}", alt: "", loading: "lazy" }
                            }
                        }
                    }
                }
            }
        }
        if let Some(n) = *viewing.read() {
            CitationViewer {
                citations: passages.clone(),
                n,
                fallback: format!("#{card_id} .cite-item[data-n=\"{n}\"]"),
                on_go: move |to| viewing.set(Some(to)),
                on_close: move |_| viewing.set(None),
            }
        }
    }
}

/// Something a tool made or started, as it is now in the library.
#[component]
fn ResultView(card: ResultCard) -> Element {
    let app = use_app();
    let nav = navigator();
    let lib = app.lib.read();
    // (kind, live title, state, where Open goes)
    let (kind, title, state, open): (&str, Option<String>, Option<String>, Option<Route>) =
        match &card.target {
            ResultTarget::Topic { topic_id } => (
                "Topic",
                lib.topics.get(topic_id).map(|t| t.title.clone()),
                lib.topics
                    .get(topic_id)
                    .map(|_| match lib.lesson_count_deep(topic_id) {
                        1 => "1 lesson".to_string(),
                        n => format!("{n} lessons"),
                    }),
                None,
            ),
            ResultTarget::Lesson { lesson_id } => {
                let lesson = lib.lessons.get(lesson_id);
                (
                    "Lesson",
                    lesson.map(|l| l.title.clone()),
                    lesson.map(|l| {
                        match &l.status {
                            LessonStatus::Queued => "Waiting to be written",
                            LessonStatus::Generating => "Writing…",
                            LessonStatus::Ready => "Ready",
                            LessonStatus::Failed { .. } => "Couldn't be written",
                        }
                        .to_string()
                    }),
                    lesson.map(|l| Route::LessonRead { id: l.id.clone() }),
                )
            }
            ResultTarget::Video { lesson_id } => {
                let lesson = lib.lessons.get(lesson_id);
                let video = lib.video_for_lesson(lesson_id);
                (
                    "Video",
                    lesson.map(|l| l.title.clone()),
                    lesson.map(|_| {
                        match video.map(|v| &v.status) {
                            None => "Not made",
                            Some(VideoStatus::Queued) => "Waiting to render",
                            Some(VideoStatus::Ready) => "Ready",
                            Some(VideoStatus::Failed { .. }) => "Failed",
                            Some(_) => "Rendering…",
                        }
                        .to_string()
                    }),
                    lesson.map(|l| Route::LessonVideo { id: l.id.clone() }),
                )
            }
            ResultTarget::Note { note_id } => {
                let note = lib.notes.get(note_id);
                (
                    "Note",
                    note.map(|n| n.title.clone()),
                    note.map(|_| "Saved".to_string()),
                    note.map(|n| Route::NoteOpen { id: n.id.clone() }),
                )
            }
        };
    drop(lib);
    let exists = title.is_some();
    let shown = title.unwrap_or_else(|| card.title.clone());
    let topic_id = match &card.target {
        ResultTarget::Topic { topic_id } if exists => Some(topic_id.clone()),
        _ => None,
    };
    rsx! {
        div { class: "chat-card result-card",
            Icon { name: match card.target {
                ResultTarget::Topic { .. } => "topic",
                ResultTarget::Lesson { .. } => "book",
                ResultTarget::Video { .. } => "video",
                ResultTarget::Note { .. } => "note",
            } }
            div { class: "grow",
                b { "{shown}" }
                small {
                    "{kind}"
                    if exists {
                        if let Some(s) = state { " · {s}" }
                    } else {
                        " · Deleted"
                    }
                }
            }
            if let Some(route) = open {
                button {
                    class: "btn btn-secondary btn-sm",
                    onclick: move |_| {
                        nav.push(route.clone());
                    },
                    "Open"
                }
            }
            if let Some(topic_id) = topic_id {
                button {
                    class: "btn btn-secondary btn-sm",
                    onclick: move |_| reveal_topic(app, &topic_id),
                    "Show in sidebar"
                }
            }
        }
    }
}

/// What the agent wants to do that needs the owner's OK; answerable from any device
/// while the answer waits on it.
#[component]
fn ConfirmationView(item_id: Id, card: ConfirmationCard, chat: Chat) -> Element {
    let app = use_app();
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let waiting = matches!(&chat.state, ChatState::WaitingForUser { item_id: w, .. } if *w == item_id)
        && card.decision == Decision::Pending;
    let device_name = |id: &str| app.lib.read().devices.get(id).map(|d| d.name.clone());
    let outcome = match &card.decision {
        Decision::Pending if waiting => None,
        Decision::Pending => Some("No longer waiting for an answer".to_string()),
        Decision::Approved { device_id } => Some(match device_name(device_id) {
            Some(name) => format!("Approved on {name}"),
            None => "Approved".to_string(),
        }),
        Decision::Denied { by_stop: true, .. } => Some("Stopped before you answered".to_string()),
        Decision::Denied { device_id, .. } => Some(match device_name(device_id) {
            Some(name) => format!("Declined on {name}"),
            None => "Declined".to_string(),
        }),
        Decision::Expired => Some("The service restarted before you answered".to_string()),
    };
    let answer = move |approve: bool| {
        let chat_id = chat.id.clone();
        let item_id = item_id.clone();
        spawn(async move {
            let Some(api) = app.api() else { return };
            busy.set(true);
            error.set(None);
            match api
                .confirm(&chat_id, &ConfirmRequest { item_id, approve })
                .await
            {
                Ok(chat) => {
                    let mut lib = app.lib;
                    lib.write().upsert_chat(chat);
                }
                Err(e) if e.status == Some(409) => {
                    let _ = fetch_chat(app, chat_id).await;
                }
                Err(e) => error.set(Some(app.failure(&e))),
            }
            busy.set(false);
        });
    };
    let (deny, approve) = (answer.clone(), answer);
    let danger = card.tool.starts_with("delete");
    rsx! {
        section { class: "chat-card confirm-card", role: "group", "aria-label": "{card.title}",
            b { "{card.title}" }
            if !card.detail.is_empty() {
                p { "{card.detail}" }
            }
            if let Some(err) = error.read().clone() {
                div { class: "notice error", role: "alert",
                    Icon { name: "alert" }
                    div { class: "body", "{err}" }
                }
            }
            match outcome {
                None => rsx! {
                    div { class: "row-actions",
                        button {
                            class: "btn btn-secondary btn-sm",
                            disabled: *busy.read(),
                            onclick: move |_| deny(false),
                            "Don't"
                        }
                        button {
                            class: if danger { "btn btn-danger btn-sm" } else { "btn btn-primary btn-sm" },
                            disabled: *busy.read(),
                            onclick: move |_| approve(true),
                            if *busy.read() { span { class: "spinner" } }
                            "{card.approve_label}"
                        }
                    }
                },
                Some(text) => rsx! { small { class: "faint", "{text}" } },
            }
        }
    }
}

/// A proposed course outline: the outline editor with Create lessons.
#[component]
fn OutlineView(proposal_id: Id) -> Element {
    let app = use_app();
    let nav = navigator();
    let mut draft = use_signal(|| None::<Outline>);
    let mut accepting = use_signal(|| false);
    let mut accept_error = use_signal(|| None::<ApiFailure>);
    let mut fetch_error = use_signal(|| None::<ApiFailure>);
    let fetch_id = proposal_id.clone();
    // Proposals aren't part of the library: fetch it once, events keep it current.
    use_future(move || {
        let id = fetch_id.clone();
        async move {
            if app.lib.peek().proposals.contains_key(&id) {
                return;
            }
            let Some(api) = app.api() else { return };
            match api.proposal(&id).await {
                Ok(proposal) => {
                    let mut lib = app.lib;
                    lib.write().apply(Event::ProposalChanged { proposal });
                }
                Err(e) => fetch_error.set(Some(e)),
            }
        }
    });
    // Seed the editable outline once it's ready.
    let seed_id = proposal_id.clone();
    use_effect(move || {
        let p = app.lib.read().proposals.get(&seed_id).cloned();
        if let Some(p) = p
            && p.status == ProposalStatus::Ready
            && draft.peek().is_none()
            && let Some(outline) = p.outline
        {
            draft.set(Some(outline));
        }
    });
    let proposal = app.lib.read().proposals.get(&proposal_id).cloned();
    let Some(proposal) = proposal else {
        return match fetch_error.read().clone() {
            Some(e) => {
                rsx! { ErrorNotice { message: format!("Couldn't load the outline. {}", e.message) } }
            }
            None => rsx! { Loading { label: "Loading the outline…" } },
        };
    };
    rsx! {
        div { class: "chat-card outline-card",
            ProposalView {
                proposal: proposal.clone(),
                draft,
                subject_name: None,
                accepting: *accepting.read(),
                accept_error: accept_error.read().clone(),
                on_accept: move |outline: Outline| {
                    let id = proposal_id.clone();
                    spawn(async move {
                        let Some(api) = app.api() else { return };
                        accepting.set(true);
                        accept_error.set(None);
                        let body = AcceptProposal { outline, chips: Default::default() };
                        match api.accept_proposal(&id, &body).await {
                            Ok(topic) => {
                                // The new lessons are here before the first one opens.
                                let mut lib = app.lib;
                                if let Ok(library) = api.library().await {
                                    lib.write().replace_library(library);
                                }
                                lib.write().apply(Event::TopicChanged { topic: topic.clone() });
                                let first = lib.peek().first_lesson_in(&topic.id).map(|l| l.id.clone());
                                let mut collapsed = app.collapsed;
                                collapsed.write().remove(&topic.id);
                                accepting.set(false);
                                match first {
                                    Some(lesson) => nav.push(Route::LessonRead { id: lesson }),
                                    None => nav.push(Route::TopicNotes { topic_id: topic.id }),
                                };
                                return;
                            }
                            Err(e) => {
                                app.failure(&e);
                                accept_error.set(Some(e));
                            }
                        }
                        accepting.set(false);
                    });
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use margin_shared::DeviceRef;

    fn item(id: &str, seq: u64, status: ItemStatus, kind: ChatItemKind) -> ChatItem {
        ChatItem {
            id: id.into(),
            chat_id: "c1".into(),
            turn_id: "t1".into(),
            seq,
            status,
            created_at: Utc.timestamp_opt(seq as i64, 0).unwrap(),
            updated_at: Utc.timestamp_opt(seq as i64, 0).unwrap(),
            kind,
        }
    }

    fn call(id: &str, seq: u64, call_id: &str) -> ChatItem {
        item(
            id,
            seq,
            ItemStatus::InProgress,
            ChatItemKind::ToolCall {
                call_id: call_id.into(),
                name: "search_library".into(),
                arguments: "{}".into(),
                label: String::new(),
                device: None,
            },
        )
    }

    fn result(id: &str, seq: u64, call_id: &str) -> ChatItem {
        item(
            id,
            seq,
            ItemStatus::Done,
            ChatItemKind::ToolResult {
                call_id: call_id.into(),
                ok: true,
                summary: "2 matches".into(),
                output_bytes: 10,
            },
        )
    }

    fn error(id: &str, seq: u64, retryable: bool) -> ChatItem {
        item(
            id,
            seq,
            ItemStatus::Done,
            ChatItemKind::Error {
                message: "boom".into(),
                retryable,
            },
        )
    }

    #[test]
    fn results_join_their_calls_and_quiet_items_hide() {
        let items = vec![
            item(
                "u",
                1,
                ItemStatus::Done,
                ChatItemKind::UserMessage {
                    text: "hi".into(),
                    source_ids: vec![],
                    device_id: "d1".into(),
                    device_name: "Mac".into(),
                },
            ),
            // An empty finished summary shows nothing; one still coming shows Thinking.
            item(
                "r1",
                2,
                ItemStatus::Done,
                ChatItemKind::Reasoning {
                    summary: " ".into(),
                },
            ),
            item(
                "r2",
                3,
                ItemStatus::InProgress,
                ChatItemKind::Reasoning {
                    summary: String::new(),
                },
            ),
            call("a", 4, "call_a"),
            call("b", 5, "call_b"),
            result("rb", 6, "call_b"),
            result("ra", 7, "call_a"),
            result("rx", 8, "call_unknown"),
            item(
                "act",
                9,
                ItemStatus::Done,
                ChatItemKind::Action {
                    text: String::new(),
                },
            ),
        ];
        let rows = rows(&items, &ChatState::Idle);
        let keys: Vec<&str> = rows.iter().map(Row::key).collect();
        assert_eq!(keys, ["u", "r2", "a", "b", "rx"]);
        assert!(
            matches!(&rows[2], Row::Tool { call: Some(c), result: Some(r) } if c.id == "a" && r.id == "ra")
        );
        assert!(matches!(&rows[3], Row::Tool { result: Some(r), .. } if r.id == "rb"));
        assert!(matches!(
            &rows[4],
            Row::Tool {
                call: None,
                result: Some(_)
            }
        ));
    }

    #[test]
    fn retry_only_on_the_last_retryable_error_of_the_failed_turn() {
        let items = vec![error("e1", 1, true), error("e2", 2, true)];
        let failed = ChatState::Failed {
            turn_id: "t1".into(),
            error: "boom".into(),
        };
        let retry = |rows: Vec<Row>| -> Vec<bool> {
            rows.into_iter()
                .map(|r| matches!(r, Row::Error { retry: true, .. }))
                .collect()
        };
        assert_eq!(retry(rows(&items, &failed)), [false, true]);
        assert_eq!(retry(rows(&items, &ChatState::Idle)), [false, false]);
        let items = vec![error("e1", 1, true), error("e2", 2, false)];
        assert_eq!(retry(rows(&items, &failed)), [false, false]);
        // A later turn failed without an error item: an earlier turn's error doesn't
        // offer Retry (the chat's own notice does).
        let items = vec![error("e1", 1, true)];
        let later = ChatState::Failed {
            turn_id: "t2".into(),
            error: "boom".into(),
        };
        assert_eq!(retry(rows(&items, &later)), [false]);
    }

    #[test]
    fn text_helpers() {
        assert_eq!(tool_words("search_library"), "Search library");
        assert_eq!(tool_icon("search_sources"), "search");
        assert_eq!(tool_icon("read_lesson"), "book");
        assert_eq!(tool_icon("list_files"), "folder");
        assert_eq!(tool_icon("anything"), "wrench");
        assert_eq!(
            reasoning_title("**Checking the library**\n\nMore.").as_deref(),
            Some("Checking the library")
        );
        assert_eq!(reasoning_title("  "), None);
        assert_eq!(pretty_json(r#"{"a":1}"#), "{\n  \"a\": 1\n}");
        assert_eq!(pretty_json(r#"{"a":"#), r#"{"a":"#);
        assert_eq!(size_text(912), "912 bytes");
        assert_eq!(size_text(2_480), "2.5 KB");
        assert_eq!(size_text(3_200_000), "3.2 MB");
    }

    // ---- render tests: the whole signed-in app, rendered to HTML on a route ----

    use crate::fake;
    use crate::store::LibraryState;
    use crate::ui::test_render::render_route;

    fn sample() -> LibraryState {
        let mut lib = LibraryState::from_library(fake::sample_library());
        let chats = fake::sample_chats();
        lib.replace_chats(chats.iter().map(|(c, _)| c.clone()).collect());
        for (chat, items) in chats {
            lib.chat_items.insert(chat.id, items);
        }
        lib
    }

    #[test]
    fn chat_renders_its_transcript() {
        let html = render_route("/chat/c1", sample());
        // The question, the answer as Markdown (math left for KaTeX), reasoning folded.
        assert!(html.contains(r#"class="msg-user""#), "{html}");
        assert!(html.contains("How do Fourier series converge?"));
        assert!(html.contains("<strong>midpoint of the jump</strong>"));
        assert!(html.contains(r#"class="math math-display""#));
        assert!(html.contains("Thought"));
        assert!(html.contains("Checking the library first"));
        // Tool calls are one collapsed line each, with what they found.
        assert!(html.contains("Searching the library for “fourier convergence”"));
        assert!(html.contains("2 matches: 1 lesson, 1 note"));
        assert!(html.contains(r#"aria-expanded="false""#));
        assert!(!html.contains(r#"class="tool-body""#));
        // The passages card lists each passage to open.
        assert!(html.contains("Passages for “partial sums overshoot”"));
        assert!(html.contains("Oppenheim ch3.pdf"));
        // An idle chat: Send, no Stop, no pending message.
        assert!(html.contains(r#"aria-label="Send""#));
        assert!(!html.contains(">Stop<"));
        assert!(!html.contains("Will send when the answer finishes"));
        // The sidebar lists the chats, this one current.
        assert!(html.contains(r#"aria-label="Actions for How do Fourier series converge?""#));
    }

    #[test]
    fn interrupted_chat_offers_continue() {
        let html = render_route("/chat/c2", sample());
        assert!(html.contains("The service stopped before this answer finished."));
        assert!(html.contains(">Continue<"), "{html}");
        // The cut-off answer stays, marked.
        assert!(html.contains("Eigenvectors are the directions"));
        assert!(html.contains("Stopped before it finished."));
        assert!(html.contains("The service stopped while answering."));
    }

    #[test]
    fn running_chat_shows_stop_streaming_and_the_pending_message() {
        let mut lib = sample();
        let chat = lib.chats.get_mut("c1").unwrap();
        chat.state = ChatState::Running {
            turn_id: "t9".into(),
            device_id: "d2".into(),
            started_at: Utc::now(),
        };
        chat.pending = Some(margin_shared::PendingMessage {
            text: "And in two dimensions?".into(),
            source_ids: vec![],
            device_id: "d1".into(),
            created_at: Utc::now(),
        });
        let items = lib.chat_items.get_mut("c1").unwrap();
        let mut asked = item(
            "u9",
            20,
            ItemStatus::Done,
            ChatItemKind::UserMessage {
                text: "What about square waves?".into(),
                source_ids: vec![],
                device_id: "d2".into(),
                device_name: "iPhone".into(),
            },
        );
        asked.turn_id = "t9".into();
        items.push(asked);
        let mut busy_call = call("k9", 21, "call_9");
        if let ChatItemKind::ToolCall { label, device, .. } = &mut busy_call.kind {
            *label = "Reading “Why sums of sines?”".into();
            *device = Some(DeviceRef {
                device_id: "d2".into(),
                name: "iPhone".into(),
            });
        }
        items.push(busy_call);
        let lib_running = lib.clone();
        let html = render_route("/chat/c1", lib);
        assert!(html.contains(">Stop<"), "{html}");
        assert!(html.contains("Will send when the answer finishes"));
        assert!(html.contains("And in two dimensions?"));
        assert!(
            html.contains(r#"aria-label="Don&#39;t send this message""#),
            "{html}"
        );
        // Sending again waits for the pending one.
        assert!(html.contains("A message is already waiting for this answer to finish"));
        // Another device's message says where it came from; the running call spins.
        assert!(html.contains("From iPhone"));
        assert!(html.contains("Reading “Why sums of sines?”"));
        assert!(html.contains("on iPhone"));
        assert!(html.contains("tool-row run"));
        // The running call shows the answer is under way; without one, the chat says so.
        assert!(!html.contains("Working…"));
        let mut lib = lib_running;
        lib.chat_items.get_mut("c1").unwrap().pop();
        let html = render_route("/chat/c1", lib);
        assert!(html.contains("Working…"), "{html}");
    }

    #[test]
    fn failed_chat_offers_retry_on_its_error() {
        let mut lib = sample();
        lib.chats.get_mut("c1").unwrap().state = ChatState::Failed {
            turn_id: "t2".into(),
            error: "The model API answered 500 three times.".into(),
        };
        let mut failure = error("e9", 30, true);
        failure.turn_id = "t2".into();
        failure.kind = ChatItemKind::Error {
            message: "The model API answered 500 three times.".into(),
            retryable: true,
        };
        lib.chat_items.get_mut("c1").unwrap().push(failure);
        let html = render_route("/chat/c1", lib);
        assert!(html.contains("The model API answered 500 three times."));
        assert!(html.contains("Try again"), "{html}");
    }

    #[test]
    fn cards_render_live_state() {
        let mut lib = sample();
        let items = lib.chat_items.get_mut("c1").unwrap();
        let card = |id: &str, seq, card| {
            item(
                id,
                seq,
                ItemStatus::Done,
                ChatItemKind::Card {
                    call_id: "x".into(),
                    card,
                },
            )
        };
        items.push(card(
            "k1",
            40,
            Card::Result(ResultCard {
                target: ResultTarget::Lesson {
                    lesson_id: "l1".into(),
                },
                title: "Old title".into(),
            }),
        ));
        items.push(card(
            "k2",
            41,
            Card::Result(ResultCard {
                target: ResultTarget::Note {
                    note_id: "gone".into(),
                },
                title: "A deleted note".into(),
            }),
        ));
        items.push(card(
            "k3",
            42,
            Card::Confirmation(ConfirmationCard {
                tool: "delete_note".into(),
                title: "Delete the note “Questions for Friday”?".into(),
                detail: "This can't be undone.".into(),
                approve_label: "Delete".into(),
                decision: Decision::Pending,
            }),
        ));
        lib.chats.get_mut("c1").unwrap().state = ChatState::WaitingForUser {
            turn_id: "t2".into(),
            device_id: "d1".into(),
            item_id: "k3".into(),
        };
        let html = render_route("/chat/c1", lib);
        // The live lesson, not the card's old title.
        assert!(html.contains("Why sums of sines?"));
        assert!(!html.contains("Old title"));
        assert!(html.contains("A deleted note"));
        assert!(html.contains("Note · Deleted"));
        // The question waits for an answer, with the tool's own button.
        assert!(html.contains("Delete the note “Questions for Friday”?"));
        assert!(html.contains(">Don&#39;t<"), "{html}");
        assert!(html.contains(r#"class="btn btn-danger btn-sm""#), "{html}");
    }

    #[test]
    fn new_chat_asks_what_to_learn() {
        let html = render_route("/", sample());
        assert!(html.contains("What do you want to learn?"));
        assert!(html.contains(&format!(r#"id="{PROMPT_ID}""#)));
        // A new chat has nothing to stop.
        assert!(!html.contains(">Stop<"));
    }

    #[test]
    fn a_missing_chat_says_so() {
        let html = render_route("/chat/nope", sample());
        assert!(html.contains("This chat was deleted"), "{html}");
    }
}
