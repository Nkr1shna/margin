//! Shared UI pieces: icons, the top bar, dialogs, menus, and webview helpers.

use chrono::{DateTime, Local, Utc};
use dioxus::prelude::*;

use crate::commands::Command;
use crate::state::use_app;

// Icons from the mockup's SVG sprite (24×24, stroked); "gear" is an eight-tooth cog
// drawn in the same style (the sprite's was a sun-like burst).
const ICONS: &[(&str, &str)] = &[
    (
        "search",
        r#"<circle cx="11" cy="11" r="6.5"/><path d="M20 20l-4.2-4.2"/>"#,
    ),
    ("plus", r#"<path d="M12 5v14M5 12h14"/>"#),
    (
        "note",
        r#"<path d="M6 3.5h8l4 4V20a.5.5 0 0 1-.5.5h-11A.5.5 0 0 1 6 20z"/><path d="M14 3.5V8h4M9 12h6M9 15.5h6"/>"#,
    ),
    (
        "video",
        r#"<rect x="3" y="6" width="13" height="12" rx="2"/><path d="M16 10.5l5-3v9l-5-3"/>"#,
    ),
    ("chev", r#"<path d="M7 10l5 5 5-5"/>"#),
    (
        "spark",
        r#"<path d="M12 3v4M12 17v4M3 12h4M17 12h4M6 6l2.5 2.5M15.5 15.5L18 18M18 6l-2.5 2.5M8.5 15.5L6 18"/>"#,
    ),
    (
        "gear",
        r#"<path d="M10.45 5.28L10.67 3.10A9 9 0 0 1 13.33 3.10L13.55 5.28A6.9 6.9 0 0 1 15.66 6.15L17.35 4.77A9 9 0 0 1 19.23 6.65L17.85 8.34A6.9 6.9 0 0 1 18.72 10.45L20.90 10.67A9 9 0 0 1 20.90 13.33L18.72 13.55A6.9 6.9 0 0 1 17.85 15.66L19.23 17.35A9 9 0 0 1 17.35 19.23L15.66 17.85A6.9 6.9 0 0 1 13.55 18.72L13.33 20.90A9 9 0 0 1 10.67 20.90L10.45 18.72A6.9 6.9 0 0 1 8.34 17.85L6.65 19.23A9 9 0 0 1 4.77 17.35L6.15 15.66A6.9 6.9 0 0 1 5.28 13.55L3.10 13.33A9 9 0 0 1 3.10 10.67L5.28 10.45A6.9 6.9 0 0 1 6.15 8.34L4.77 6.65A9 9 0 0 1 6.65 4.77L8.34 6.15A6.9 6.9 0 0 1 10.45 5.28z"/><circle cx="12" cy="12" r="2.8"/>"#,
    ),
    (
        "play",
        r#"<path d="M8 5.5v13l10.5-6.5z" fill="currentColor" stroke="none"/>"#,
    ),
    ("check", r#"<path d="M5 12.5l4.5 4.5L19 7.5"/>"#),
    (
        "more",
        r#"<circle cx="6" cy="12" r="1" fill="currentColor"/><circle cx="12" cy="12" r="1" fill="currentColor"/><circle cx="18" cy="12" r="1" fill="currentColor"/>"#,
    ),
    ("up", r#"<path d="M12 19V6M6 11.5L12 5.5l6 6"/>"#),
    ("down", r#"<path d="M12 5v13M6 12.5l6 6 6-6"/>"#),
    (
        "book",
        r#"<path d="M4 5.5A1.5 1.5 0 0 1 5.5 4H11v16H5.5A1.5 1.5 0 0 1 4 18.5zM20 5.5A1.5 1.5 0 0 0 18.5 4H13v16h5.5a1.5 1.5 0 0 0 1.5-1.5z"/>"#,
    ),
    (
        "bulb",
        r#"<path d="M9 18h6M10 21h4M12 3a6 6 0 0 0-3.5 10.9c.6.5 1 1.2 1 2V16h5v-.1c0-.8.4-1.5 1-2A6 6 0 0 0 12 3z"/>"#,
    ),
    (
        "panel",
        r#"<rect x="3.5" y="4.5" width="17" height="15" rx="2"/><path d="M9 4.5v15"/>"#,
    ),
    (
        "sun",
        r#"<circle cx="12" cy="12" r="4"/><path d="M12 2.5v2M12 19.5v2M2.5 12h2M19.5 12h2M5.3 5.3l1.4 1.4M17.3 17.3l1.4 1.4M18.7 5.3l-1.4 1.4M6.7 17.3l-1.4 1.4"/>"#,
    ),
    (
        "moon",
        r#"<path d="M20 14.5A8 8 0 0 1 9.5 4a8 8 0 1 0 10.5 10.5z"/>"#,
    ),
    (
        "half",
        r#"<circle cx="12" cy="12" r="8"/><path d="M12 4v16" /><path d="M12 4a8 8 0 0 1 0 16z" fill="currentColor"/>"#,
    ),
    ("x", r#"<path d="M6 6l12 12M18 6L6 18"/>"#),
    (
        "trash",
        r#"<path d="M4.5 7h15M9.5 7V4.5h5V7M6.5 7l1 13h9l1-13"/>"#,
    ),
    (
        "edit",
        r#"<path d="M4 20h4L19 9l-4-4L4 16z"/><path d="M13.5 6.5l4 4"/>"#,
    ),
    (
        "copy",
        r#"<rect x="8" y="8" width="12" height="12" rx="2"/><path d="M16 8V5.5A1.5 1.5 0 0 0 14.5 4h-9A1.5 1.5 0 0 0 4 5.5v9A1.5 1.5 0 0 0 5.5 16H8"/>"#,
    ),
    (
        "alert",
        r#"<circle cx="12" cy="12" r="8.5"/><path d="M12 7.5v5.5M12 16.5v.01"/>"#,
    ),
    ("back", r#"<path d="M14.5 6l-6 6 6 6"/>"#),
    ("forward", r#"<path d="M9.5 6l6 6-6 6"/>"#),
    (
        "image",
        r#"<rect x="3.5" y="4.5" width="17" height="15" rx="2"/><circle cx="9" cy="10" r="1.6"/><path d="M4 17.5l5-4.5 4 3.5 2.5-2 4.5 4"/>"#,
    ),
    (
        "audio",
        r#"<path d="M9 18.5V6.5l10-2v12"/><circle cx="6.5" cy="18.5" r="2.5"/><circle cx="16.5" cy="16.5" r="2.5"/>"#,
    ),
    (
        "user",
        r#"<circle cx="12" cy="8.5" r="3.5"/><path d="M5 19.5c1.2-3.3 3.8-5 7-5s5.8 1.7 7 5"/>"#,
    ),
    (
        "server",
        r#"<rect x="4" y="4.5" width="16" height="6" rx="1.5"/><rect x="4" y="13.5" width="16" height="6" rx="1.5"/><path d="M7.5 7.5h.01M7.5 16.5h.01"/>"#,
    ),
    (
        "wrench",
        r#"<path d="M14.5 5.5a4 4 0 0 0 4.9 4.9l-8.6 8.6a2 2 0 0 1-2.8-2.8l8.6-8.6"/><path d="M14.5 5.5l2-2 1.5 2.5 2.5 1.5-2 2"/>"#,
    ),
    (
        "refresh",
        r#"<path d="M19.5 12a7.5 7.5 0 1 1-2.2-5.3"/><path d="M19.5 4.5v4h-4"/>"#,
    ),
    (
        "folder",
        r#"<path d="M3.5 6.5A1.5 1.5 0 0 1 5 5h4.5l2 2.5H19a1.5 1.5 0 0 1 1.5 1.5v8.5A1.5 1.5 0 0 1 19 19H5a1.5 1.5 0 0 1-1.5-1.5z"/>"#,
    ),
    // A page with a plus: adding files (sources).
    (
        "file-add",
        r#"<path d="M6 3.5h8l4 4V20a.5.5 0 0 1-.5.5h-11A.5.5 0 0 1 6 20z"/><path d="M14 3.5V8h4M12 11v6M9 14h6"/>"#,
    ),
    // A chat: a speech bubble.
    (
        "chat",
        r#"<path d="M4.5 6.5A2 2 0 0 1 6.5 4.5h11a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H11l-4.5 3.5v-3.5h0A2 2 0 0 1 4.5 14.5z"/>"#,
    ),
    // Stop the answer: a filled square.
    (
        "stop",
        r#"<rect x="7" y="7" width="10" height="10" rx="1.5" fill="currentColor" stroke="none"/>"#,
    ),
    // A topic: stacked layers (a subject and what it holds).
    (
        "topic",
        r#"<path d="M12 4l8.5 4.25L12 12.5 3.5 8.25z"/><path d="M3.5 12.25L12 16.5l8.5-4.25M3.5 16.25L12 20.5l8.5-4.25"/>"#,
    ),
    // Sources: a paperclip, and the code and PDF file kinds ("note" stands for text).
    (
        "clip",
        r#"<path d="M19.5 11.5l-7.6 7.6a4.6 4.6 0 0 1-6.5-6.5l8-8a3.1 3.1 0 0 1 4.4 4.4l-7.9 7.9a1.55 1.55 0 0 1-2.2-2.2l7.3-7.3"/>"#,
    ),
    (
        "code",
        r#"<path d="M8.5 7.5L4 12l4.5 4.5M15.5 7.5L20 12l-4.5 4.5M13.5 5l-3 14"/>"#,
    ),
    (
        "pdf",
        r#"<path d="M6 3.5h8l4 4V20a.5.5 0 0 1-.5.5h-11A.5.5 0 0 1 6 20z"/><path d="M14 3.5V8h4"/><rect x="8.5" y="12" width="7" height="5" rx="1"/>"#,
    ),
];

fn icon_svg(name: &str) -> String {
    let body = ICONS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, b)| *b)
        .unwrap_or("");
    format!(r#"<svg viewBox="0 0 24 24" aria-hidden="true">{body}</svg>"#)
}

#[component]
pub fn Icon(name: &'static str, #[props(default)] class: &'static str) -> Element {
    rsx! {
        span { class: "ic {class}", "aria-hidden": "true", dangerous_inner_html: icon_svg(name) }
    }
}

/// Run a helper from `assets/margin.js`, waiting for the script to load first.
pub fn js(body: &str) -> document::Eval {
    document::eval(&format!(
        "while (!window.MarginApp) {{ await new Promise(function (r) {{ setTimeout(r, 20); }}); }}\n{body}"
    ))
}

/// `onmounted` handler that focuses the element once it is in the page. Use it instead
/// of the `autofocus` attribute, which WebKit honours at most once per document, so
/// menus, dialogs and fields opened later would otherwise start without focus.
pub fn focus_on_mount(e: MountedEvent) {
    spawn(async move {
        let _ = e.set_focus(true).await;
    });
}

/// Say `message` through the shell's polite live region (screen readers read it once).
pub fn announce(message: &str) {
    let arg = serde_json::to_string(message).unwrap_or_default();
    let _ = js(&format!("MarginApp.announce({arg});"));
}

/// Typeset math inside the element with this id (after the DOM update lands).
pub fn render_math(id: &str) {
    let id = serde_json::to_string(id).unwrap_or_default();
    let _ = js(&format!("MarginApp.renderMath({id});"));
}

/// Focus the first element matching `selector` once the next DOM update lands (for a
/// field that was just added, or the first one with an error).
pub fn focus_first(selector: &str) {
    let sel = serde_json::to_string(selector).unwrap_or_default();
    let _ = js(&format!("MarginApp.focusFirst({sel});"));
}

pub fn render_math_in(selector: &str) {
    let sel = serde_json::to_string(selector).unwrap_or_default();
    let _ = js(&format!("MarginApp.renderMathIn({sel});"));
}

/// Copy text to the system clipboard.
pub async fn copy_to_clipboard(text: String) -> Result<(), String> {
    #[cfg(not(any(target_os = "ios", target_os = "android")))]
    {
        arboard::Clipboard::new()
            .and_then(|mut c| c.set_text(text))
            .map_err(|e| format!("Couldn't copy: {e}"))
    }
    #[cfg(any(target_os = "ios", target_os = "android"))]
    {
        let arg = serde_json::to_string(&text).unwrap_or_default();
        match js(&format!("return await MarginApp.copyText({arg});"))
            .join::<bool>()
            .await
        {
            Ok(true) => Ok(()),
            _ => Err("Couldn't copy to the clipboard.".into()),
        }
    }
}

pub fn open_external(url: &str) -> Result<(), String> {
    webbrowser::open(url).map_err(|e| format!("Couldn't open the browser: {e}"))
}

/// "Oct 12", "2 h ago", "yesterday"…, lowercase so it reads right mid-sentence
/// ("edited just now"); use [`capitalize_first`] where it starts a line.
pub fn ago(t: &DateTime<Utc>) -> String {
    ago_at(t, Utc::now())
}

fn ago_at(t: &DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - *t).num_seconds();
    if secs < 45 {
        return "just now".into();
    }
    if secs < 3600 {
        return format!("{} min ago", (secs / 60).max(1));
    }
    if secs < 86_400 {
        return format!("{} h ago", secs / 3600);
    }
    if secs < 2 * 86_400 {
        return "yesterday".into();
    }
    let local = t.with_timezone(&Local);
    if local.format("%Y").to_string() == Local::now().format("%Y").to_string() {
        local.format("%b %-d").to_string()
    } else {
        local.format("%b %-d, %Y").to_string()
    }
}

/// The text with its first letter upper-cased ("just now" → "Just now").
pub fn capitalize_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// A message trimmed, capitalised and ended with terminal punctuation, so it can follow
/// another sentence ("Couldn't load your library. {sentence(message)}").
pub fn sentence(text: &str) -> String {
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    let text = capitalize_first(text);
    if text.ends_with(['.', '!', '?', '…']) {
        text
    } else {
        format!("{text}.")
    }
}

pub fn date(t: &DateTime<Utc>) -> String {
    t.with_timezone(&Local).format("%b %-d, %Y").to_string()
}

/// The app's mark: the app icon (a page with a blue margin rule) drawn small, for the
/// sidebar and the sign-in card. It follows `icon/Margin.icon` (same tile, page, margin
/// rule and note dot) but is simplified on purpose for 20–40 px: three thicker text
/// lines instead of four and a slightly larger dot, so it stays legible when small. Keep
/// the shared parts in step with the icon. In dark mode the tile gets a faint ring (CSS
/// `.mark .tile`) so its edge reads against a black window.
#[component]
pub fn BrandMark() -> Element {
    rsx! {
        svg {
            class: "mark",
            "aria-hidden": "true",
            view_box: "0 0 1024 1024",
            defs {
                linearGradient { id: "mark-bg", x1: "0", y1: "0", x2: "0", y2: "1",
                    stop { offset: "0", stop_color: "#2b2b2e" }
                    stop { offset: "1", stop_color: "#060607" }
                }
            }
            rect { class: "tile", width: "1024", height: "1024", rx: "232", fill: "url(#mark-bg)" }
            rect { x: "262", y: "196", width: "500", height: "632", rx: "64", fill: "#fff" }
            rect { x: "372", y: "330", width: "300", height: "40", rx: "20", fill: "#cfcfcf" }
            rect { x: "372", y: "450", width: "300", height: "40", rx: "20", fill: "#cfcfcf" }
            rect { x: "372", y: "570", width: "220", height: "40", rx: "20", fill: "#cfcfcf" }
            rect { x: "318", y: "270", width: "26", height: "484", rx: "13", fill: "#3a83f7" }
            circle { cx: "220", cy: "437", r: "36", fill: "#3a83f7" }
        }
    }
}

/// Whether the window uses the unified title bar (macOS desktop), where the app draws
/// the drag regions and leaves room for the traffic lights.
pub const UNIFIED_TITLE_BAR: bool = cfg!(all(feature = "desktop", target_os = "macos"));

/// The draggable background of a title-bar area. It fills its (positioned) parent behind
/// the parent's content; CSS lets presses through everything in the bar but controls, so
/// pressing empty bar or plain text moves the window and a double click zooms it.
#[component]
pub fn DragRegion() -> Element {
    if !UNIFIED_TITLE_BAR {
        return rsx! {};
    }
    rsx! {
        div {
            class: "drag-region",
            "aria-hidden": "true",
            onmousedown: move |e: MouseEvent| {
                #[cfg(feature = "desktop")]
                if e.trigger_button() == Some(dioxus::html::input_data::MouseButton::Primary) {
                    dioxus::desktop::window().drag();
                }
                let _ = e;
            },
            ondoubleclick: move |_| {
                #[cfg(feature = "desktop")]
                {
                    let window = dioxus::desktop::window();
                    window.set_maximized(!window.is_maximized());
                }
            },
        }
    }
}

/// Back and forward through the screens visited. They sit in the sidebar's header row,
/// and in the top bar while the sidebar is hidden (CSS picks which shows).
#[component]
pub fn HistoryButtons() -> Element {
    let nav = navigator();
    // Re-render on navigation so the enabled state stays current.
    let _ = use_route::<crate::routes::Route>();
    let (can_back, can_forward) = (nav.can_go_back(), nav.can_go_forward());
    rsx! {
        div { class: "history",
            button {
                class: "icon-btn",
                "aria-label": "Back",
                title: "Back ({Command::Back.shortcut()})",
                disabled: !can_back,
                onclick: move |_| nav.go_back(),
                Icon { name: "back" }
            }
            button {
                class: "icon-btn",
                "aria-label": "Forward",
                title: "Forward ({Command::Forward.shortcut()})",
                disabled: !can_forward,
                onclick: move |_| nav.go_forward(),
                Icon { name: "forward" }
            }
        }
    }
}

/// Where a breadcrumb leads.
#[derive(Clone, PartialEq, Debug)]
pub enum CrumbLink {
    Route(crate::routes::Route),
    /// Shows the topic in the sidebar: expands it (and the topics above it), opens the
    /// sidebar or drawer, and moves focus to it.
    Topic(margin_shared::Id),
}

/// Expand `topic_id` and the topics above it in the sidebar tree, show the sidebar (the
/// drawer on narrow screens), and focus the topic's row.
pub fn reveal_topic(app: crate::state::AppCtx, topic_id: &str) {
    let path: Vec<margin_shared::Id> = app
        .lib
        .peek()
        .topic_path(topic_id)
        .iter()
        .map(|t| t.id.clone())
        .collect();
    let mut collapsed = app.collapsed;
    {
        let mut set = collapsed.write();
        for id in &path {
            set.remove(id);
        }
    }
    if app.config.peek().sidebar_hidden {
        let _ = app.set_config(|c| c.sidebar_hidden = false);
    }
    let selector = serde_json::to_string(&format!(".sb [data-topic=\"{topic_id}\"] > .row"))
        .unwrap_or_default();
    spawn(async move {
        let narrow = document::eval("return window.matchMedia('(max-width: 1023px)').matches;")
            .join::<bool>()
            .await
            .unwrap_or(false);
        if narrow {
            let mut open = app.drawer_open;
            open.set(true);
        }
        let _ = js(&format!("MarginApp.focus({selector});"));
    });
}

/// The top bar of every screen: the drawer button (narrow screens), breadcrumbs, and
/// screen controls on the right. A crumb with a link (`links`, by position) is a button;
/// the last crumb is the current screen and never is. `provisional` shows the last crumb
/// faint: a chat's first-line title, until the model or the owner names the chat.
#[component]
pub fn TopBar(
    crumbs: Vec<String>,
    #[props(default)] links: Vec<Option<CrumbLink>>,
    #[props(default)] provisional: bool,
    children: Element,
) -> Element {
    let mut app = use_app();
    let nav = navigator();
    let last = crumbs.len().saturating_sub(1);
    // Each crumb carries the whole path as its tooltip: crumbs ellipsize when the bar is
    // narrow, and below 720px only the current page shows.
    let path = crumbs.join(" / ");
    rsx! {
        div { class: "topbar",
            DragRegion {}
            button {
                // Wide windows: shown while the sidebar is collapsed, un-collapses it.
                class: "icon-btn menu-btn menu-wide",
                "aria-label": "Show sidebar",
                title: "Show sidebar ({Command::ToggleSidebar.shortcut()})",
                onclick: move |_| {
                    let _ = app.set_config(|c| c.sidebar_hidden = false);
                },
                Icon { name: "panel" }
            }
            button {
                // Narrow windows: the sidebar is a drawer; this opens it.
                class: "icon-btn menu-btn menu-narrow",
                "aria-label": "Open sidebar",
                "aria-expanded": app.drawer_open.read().to_string(),
                onclick: move |_| app.drawer_open.set(true),
                Icon { name: "panel" }
            }
            HistoryButtons {}
            nav { class: "crumbs", "aria-label": "Breadcrumbs",
                for (i, c) in crumbs.iter().enumerate() {
                    if i == last {
                        b { key: "{i}", class: if provisional { "provisional" }, title: "{path}", "aria-current": "page", "{c}" }
                    } else if let Some(link) = links.get(i).cloned().flatten() {
                        span { key: "{i}", class: "crumb-anc", title: "{path}",
                            button {
                                class: "crumb crumb-t",
                                title: match &link {
                                    CrumbLink::Topic(_) => "Show in sidebar",
                                    CrumbLink::Route(_) => "",
                                },
                                onclick: move |_| match &link {
                                    CrumbLink::Route(route) => {
                                        nav.push(route.clone());
                                    }
                                    CrumbLink::Topic(id) => reveal_topic(app, id),
                                },
                                "{c}"
                            }
                            span { class: "sep", "aria-hidden": "true", "/" }
                        }
                    } else {
                        span { key: "{i}", class: "crumb-anc", title: "{path}",
                            span { class: "crumb-t", "{c}" }
                            span { class: "sep", "aria-hidden": "true", "/" }
                        }
                    }
                }
            }
            {children}
        }
        // App-wide status (offline, library failed) sits under the bar, so the title
        // bar row stays the window chrome and lines up with the sidebar's header.
        crate::ui::shell::StatusBanners {}
    }
}

/// A modal confirmation dialog.
#[component]
pub fn ConfirmDialog(
    title: String,
    message: String,
    confirm_label: String,
    #[props(default = true)] danger: bool,
    #[props(default)] busy: bool,
    #[props(default)] error: Option<String>,
    on_confirm: EventHandler<()>,
    on_cancel: EventHandler<()>,
) -> Element {
    // Remember where the focus was, to put it back when the dialog is cancelled.
    use_hook(|| {
        let _ = js("MarginApp.saveFocus();");
    });
    // While the action runs, the dialog can't be dismissed: it would look cancelled
    // while the request carries on.
    let cancel = move || {
        if busy {
            return;
        }
        let _ = js("MarginApp.restoreFocus();");
        on_cancel.call(());
    };
    rsx! {
        div {
            class: "modal-scrim",
            onclick: move |_| cancel(),
            // Escape closes this dialog only, not a dialog it sits in (Settings).
            onkeydown: move |e: KeyboardEvent| {
                if e.key() == Key::Escape {
                    e.stop_propagation();
                    cancel();
                }
            },
            div {
                class: "modal",
                role: "alertdialog",
                "aria-modal": "true",
                "aria-label": "{title}",
                "aria-busy": busy.to_string(),
                onclick: move |e| e.stop_propagation(),
                h2 { "{title}" }
                p { "{message}" }
                if let Some(err) = error {
                    div { class: "notice error", role: "alert",
                        Icon { name: "alert" }
                        div { class: "body", "{err}" }
                    }
                }
                div { class: "row-actions",
                    button {
                        class: "btn btn-secondary btn-sm",
                        disabled: busy,
                        onclick: move |_| cancel(),
                        "Cancel"
                    }
                    button {
                        class: if danger { "btn btn-danger btn-sm" } else { "btn btn-primary btn-sm" },
                        disabled: busy,
                        onmounted: focus_on_mount,
                        onclick: move |_| {
                            if !busy {
                                on_confirm.call(());
                            }
                        },
                        if busy { span { class: "spinner" } }
                        "{confirm_label}"
                    }
                }
            }
        }
    }
}

/// The one "delete this note" confirmation, wherever a note is deleted from.
#[component]
pub fn DeleteNoteDialog(
    note_title: String,
    #[props(default)] busy: bool,
    #[props(default)] error: Option<String>,
    on_confirm: EventHandler<()>,
    on_cancel: EventHandler<()>,
) -> Element {
    rsx! {
        ConfirmDialog {
            title: "Delete “{note_title}”?",
            message: "This note will be deleted. This can't be undone.",
            confirm_label: "Delete note",
            busy,
            error,
            on_confirm,
            on_cancel,
        }
    }
}

/// Confirmation before regenerating a lesson, which replaces its text. A failed request
/// shows its `error` here, with the dialog left open to try again.
#[component]
pub fn RegenerateLessonDialog(
    lesson_title: String,
    #[props(default)] busy: bool,
    #[props(default)] error: Option<String>,
    on_confirm: EventHandler<()>,
    on_cancel: EventHandler<()>,
) -> Element {
    rsx! {
        ConfirmDialog {
            title: "Regenerate “{lesson_title}”?",
            message: "The lesson is written again and its current text is replaced. Your notes stay, though the passages they quote may change. This can't be undone.",
            confirm_label: "Regenerate",
            danger: false,
            busy,
            error,
            on_confirm,
            on_cancel,
        }
    }
}

/// An error notice with an optional "Open Settings" action (for `no_api_key` and
/// similar) that opens the given settings pane (a `SettingsPane` slug).
#[component]
pub fn ErrorNotice(
    message: String,
    #[props(default)] settings_pane: Option<crate::ui::settings::SettingsPane>,
    #[props(default)] on_retry: Option<EventHandler<()>>,
) -> Element {
    let mut app = use_app();
    rsx! {
        div { class: "notice error", role: "alert",
            Icon { name: "alert" }
            div { class: "body",
                span { "{message}" }
                if settings_pane.is_some() || on_retry.is_some() {
                    div { class: "row-actions",
                        if let Some(pane) = settings_pane {
                            button {
                                class: "btn btn-secondary btn-sm",
                                onclick: move |_| app.settings_open.set(Some(pane)),
                                Icon { name: "gear" }
                                "Open Settings"
                            }
                        }
                        if let Some(retry) = on_retry {
                            button {
                                class: "btn btn-secondary btn-sm",
                                onclick: move |_| retry.call(()),
                                "Try again"
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
pub fn Loading(#[props(default = "Loading…".to_string())] label: String) -> Element {
    rsx! {
        div { class: "empty", role: "status",
            span { class: "spinner" }
            p { "{label}" }
        }
    }
}

/// The margin notes' passages to mark in a lesson body, as the JSON `MarginApp.markQuotes`
/// takes: `[{id, quote}]` for each note that quotes the lesson.
pub fn quote_marks_json<'a>(notes: impl IntoIterator<Item = &'a margin_shared::Note>) -> String {
    let marks: Vec<serde_json::Value> = notes
        .into_iter()
        .filter_map(|n| {
            let quote = n.quote.as_deref()?.trim();
            (!quote.is_empty()).then(|| serde_json::json!({ "id": n.id, "quote": quote }))
        })
        .collect();
    serde_json::Value::Array(marks).to_string()
}

/// Mark the notes' passages (from `quote_marks_json`) in the element with this id; the
/// marks are kept as the body updates, and tapping one shows its note.
pub fn mark_quotes(body_id: &str, marks_json: &str) {
    let id = serde_json::to_string(body_id).unwrap_or_default();
    let _ = js(&format!("MarginApp.markQuotes({id}, {marks_json});"));
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use margin_shared::Note;

    use super::*;

    fn note(id: &str, quote: Option<&str>) -> Note {
        Note {
            id: id.into(),
            topic_id: "t".into(),
            lesson_id: Some("l".into()),
            title: String::new(),
            body_md: String::new(),
            quote: quote.map(Into::into),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn quote_marks_keep_only_notes_with_a_quote() {
        let notes = [
            note("a", Some("  the integral  ")),
            note("b", None),
            note("c", Some("   ")),
            note("d", Some("say \"hi\" </script>")),
        ];
        let json = quote_marks_json(&notes);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed,
            serde_json::json!([
                { "id": "a", "quote": "the integral" },
                { "id": "d", "quote": "say \"hi\" </script>" },
            ])
        );
    }

    #[test]
    fn quote_marks_of_no_notes_is_an_empty_list() {
        assert_eq!(quote_marks_json(&[]), "[]");
    }

    #[test]
    fn ago_is_lowercase_for_recent_times() {
        let now = Utc::now();
        assert_eq!(ago_at(&now, now), "just now");
        assert_eq!(ago_at(&(now - Duration::minutes(5)), now), "5 min ago");
        assert_eq!(ago_at(&(now - Duration::hours(3)), now), "3 h ago");
        assert_eq!(ago_at(&(now - Duration::hours(30)), now), "yesterday");
    }

    #[test]
    fn capitalize_first_only_touches_the_first_letter() {
        assert_eq!(capitalize_first("just now"), "Just now");
        assert_eq!(capitalize_first("5 min ago"), "5 min ago");
        assert_eq!(capitalize_first("éclair"), "Éclair");
        assert_eq!(capitalize_first(""), "");
    }

    #[test]
    fn sentence_ends_with_one_terminal_mark() {
        assert_eq!(sentence("connection reset"), "Connection reset.");
        assert_eq!(
            sentence("  The live connection closed.  "),
            "The live connection closed."
        );
        assert_eq!(
            sentence("Is the service running?"),
            "Is the service running?"
        );
        assert_eq!(sentence("Waiting…"), "Waiting…");
        assert_eq!(sentence("   "), "");
    }
}
