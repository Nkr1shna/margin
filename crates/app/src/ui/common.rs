//! Shared UI pieces: icons, the top bar, dialogs, menus, and webview helpers.

use chrono::{DateTime, Local, Utc};
use dioxus::prelude::*;

use crate::state::use_app;

// Icons from the mockup's SVG sprite (24×24, stroked).
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
        r#"<circle cx="12" cy="12" r="3"/><path d="M12 3v2.5M12 18.5V21M3 12h2.5M18.5 12H21M5.6 5.6l1.8 1.8M16.6 16.6l1.8 1.8M18.4 5.6l-1.8 1.8M7.4 16.6l-1.8 1.8"/>"#,
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
    (
        "refresh",
        r#"<path d="M19.5 12a7.5 7.5 0 1 1-2.2-5.3"/><path d="M19.5 4.5v4h-4"/>"#,
    ),
    (
        "folder",
        r#"<path d="M3.5 6.5A1.5 1.5 0 0 1 5 5h4.5l2 2.5H19a1.5 1.5 0 0 1 1.5 1.5v8.5A1.5 1.5 0 0 1 19 19H5a1.5 1.5 0 0 1-1.5-1.5z"/>"#,
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

/// Typeset math inside the element with this id (after the DOM update lands).
pub fn render_math(id: &str) {
    let id = serde_json::to_string(id).unwrap_or_default();
    let _ = js(&format!("MarginApp.renderMath({id});"));
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

/// "Oct 12", "2 h ago", "Yesterday"…
pub fn ago(t: &DateTime<Utc>) -> String {
    let now = Utc::now();
    let secs = (now - *t).num_seconds();
    if secs < 45 {
        return "Just now".into();
    }
    if secs < 3600 {
        return format!("{} min ago", (secs / 60).max(1));
    }
    if secs < 86_400 {
        return format!("{} h ago", secs / 3600);
    }
    if secs < 2 * 86_400 {
        return "Yesterday".into();
    }
    let local = t.with_timezone(&Local);
    if local.format("%Y").to_string() == Local::now().format("%Y").to_string() {
        local.format("%b %-d").to_string()
    } else {
        local.format("%b %-d, %Y").to_string()
    }
}

pub fn date(t: &DateTime<Utc>) -> String {
    t.with_timezone(&Local).format("%b %-d, %Y").to_string()
}

/// The top bar of every screen: the drawer button (narrow screens), breadcrumbs, and
/// screen controls on the right.
#[component]
pub fn TopBar(crumbs: Vec<String>, children: Element) -> Element {
    let mut app = use_app();
    let last = crumbs.len().saturating_sub(1);
    rsx! {
        div { class: "topbar",
            button {
                class: "icon-btn menu-btn",
                "aria-label": "Open sidebar",
                onclick: move |_| app.drawer_open.set(true),
                Icon { name: "panel" }
            }
            nav { class: "crumbs", "aria-label": "Breadcrumbs",
                for (i, c) in crumbs.iter().enumerate() {
                    if i == last {
                        b { key: "{i}", "{c}" }
                    } else {
                        span { key: "{i}", "{c}" span { class: "sep", " /" } }
                    }
                }
            }
            {children}
        }
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
    rsx! {
        div {
            class: "modal-scrim",
            onclick: move |_| on_cancel.call(()),
            onkeydown: move |e: KeyboardEvent| {
                if e.key() == Key::Escape {
                    on_cancel.call(());
                }
            },
            div {
                class: "modal",
                role: "alertdialog",
                "aria-modal": "true",
                onclick: move |e| e.stop_propagation(),
                h2 { "{title}" }
                p { "{message}" }
                if let Some(err) = error {
                    div { class: "notice error",
                        Icon { name: "alert" }
                        div { class: "body", "{err}" }
                    }
                }
                div { class: "row-actions",
                    button {
                        class: "btn btn-secondary btn-sm",
                        onclick: move |_| on_cancel.call(()),
                        "Cancel"
                    }
                    button {
                        class: if danger { "btn btn-danger btn-sm" } else { "btn btn-primary btn-sm" },
                        disabled: busy,
                        autofocus: true,
                        onclick: move |_| on_confirm.call(()),
                        if busy { span { class: "spinner" } }
                        "{confirm_label}"
                    }
                }
            }
        }
    }
}

/// An error notice with an optional "Open Settings" action (for `no_api_key` and similar).
#[component]
pub fn ErrorNotice(
    message: String,
    #[props(default)] settings_link: bool,
    #[props(default)] on_retry: Option<EventHandler<()>>,
) -> Element {
    let nav = navigator();
    rsx! {
        div { class: "notice error", role: "alert",
            Icon { name: "alert" }
            div { class: "body",
                span { "{message}" }
                if settings_link || on_retry.is_some() {
                    div { class: "row-actions",
                        if settings_link {
                            button {
                                class: "btn btn-secondary btn-sm",
                                onclick: move |_| {
                                    nav.push(crate::routes::Route::SettingsPage {});
                                },
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
