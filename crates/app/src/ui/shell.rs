//! The Option D frame: sidebar, main column, the ⌘K palette, and global shortcuts.

use dioxus::prelude::*;

use crate::routes::Route;
use crate::state::{LiveStatus, LoadStatus, use_app};
use crate::ui::common::{Icon, js};
use crate::ui::search::SearchPalette;
use crate::ui::settings::{SettingsDialog, SettingsPane};
use crate::ui::sidebar::Sidebar;

/// Sidebar width limits when resizing by dragging, in CSS pixels.
const SIDEBAR_MIN: u32 = 200;
const SIDEBAR_MAX: u32 = 480;

#[component]
pub fn Shell() -> Element {
    let mut app = use_app();
    let nav = navigator();
    let route = use_route::<Route>();

    // Cmd/Ctrl+K opens search, Cmd/Ctrl+N opens Ask, Cmd/Ctrl+Shift+N opens All notes, Cmd/Ctrl+\ toggles the sidebar
    // (or the drawer, in windows under 1024px),
    // Cmd/Ctrl+[ and ] go back and forward, from anywhere in the window.
    use_future(move || async move {
        let mut eval = js(
            "MarginApp.watchShortcuts(function (m) { dioxus.send(m); });\nawait new Promise(function () {});",
        );
        while let Ok(message) = eval.recv::<String>().await {
            match message.as_str() {
                "search" => app.search_open.set(true),
                "notes" => {
                    app.search_open.set(false);
                    nav.push(Route::AllNotes {});
                }
                "settings" => app.settings_open.set(Some(SettingsPane::General)),
                "back" => nav.go_back(),
                "forward" => nav.go_forward(),
                "sidebar" => {
                    let hidden = app.config.peek().sidebar_hidden;
                    let _ = app.set_config(|c| c.sidebar_hidden = !hidden);
                    app.drawer_open.set(false);
                }
                // Below 1024px the sidebar is a drawer: the shortcut opens and closes it.
                "drawer" => {
                    let open = *app.drawer_open.peek();
                    app.drawer_open.set(!open);
                }
                "ask" => {
                    app.search_open.set(false);
                    nav.push(Route::Ask {});
                }
                _ => {}
            }
        }
    });

    // Dragging the sidebar's edge resizes it; the width is saved (0 = back to default).
    use_future(move || async move {
        let mut eval = js(&format!(
            "MarginApp.watchSidebarResize({SIDEBAR_MIN}, {SIDEBAR_MAX}, function (w) {{ dioxus.send(w); }});\nawait new Promise(function () {{}});"
        ));
        while let Ok(width) = eval.recv::<u32>().await {
            let width = (width > 0).then(|| width.clamp(SIDEBAR_MIN, SIDEBAR_MAX));
            let _ = app.set_config(|c| c.sidebar_width = width);
        }
    });

    // On every route change: close the drawer (narrow screens) and start the new page at
    // the top. Pages reuse the same scroll element, so its offset would otherwise carry over.
    use_effect(use_reactive!(|route| {
        let _ = route;
        app.drawer_open.set(false);
        document::eval(
            "requestAnimationFrame(function () { document.querySelectorAll('.main .scroll').forEach(function (e) { e.scrollTop = 0; }); });",
        );
    }));

    let lib_status = app.lib_status.read().clone();
    let live = app.live.read().clone();
    let drawer_open = *app.drawer_open.read();
    let sidebar_hidden = app.config.read().sidebar_hidden;
    let sidebar_style = app
        .config
        .read()
        .sidebar_width
        .map(|w| format!("--sb-w: {w}px"))
        .unwrap_or_default();

    rsx! {
        div {
            class: if sidebar_hidden { "app sb-hidden" } else { "app" },
            style: "{sidebar_style}",
            if drawer_open {
                div {
                    class: "drawer-scrim",
                    onclick: move |_| app.drawer_open.set(false),
                }
            }
            Sidebar {}
            main { class: "main",
                match lib_status {
                    LoadStatus::Failed(message) => rsx! {
                        div { class: "banner error", role: "alert",
                            Icon { name: "alert" }
                            span { class: "grow", "Couldn't load your library. {message}" }
                            button {
                                class: "btn btn-secondary btn-sm",
                                onclick: move |_| {
                                    let next = *app.reload.peek() + 1;
                                    app.reload.set(next);
                                    app.lib_status.set(LoadStatus::Loading);
                                },
                                "Retry"
                            }
                        }
                    },
                    _ => rsx! {},
                }
                if let LiveStatus::Reconnecting { error, retry_secs } = live {
                    if *app.lib_status.read() == LoadStatus::Loaded {
                        div { class: "banner", role: "status",
                            span { class: "dot" }
                            span { class: "grow", "Offline: {error} Retrying in {retry_secs} s." }
                            button {
                                class: "btn btn-ghost btn-sm",
                                onclick: move |_| {
                                    let next = *app.reload.peek() + 1;
                                    app.reload.set(next);
                                },
                                "Reload now"
                            }
                        }
                    }
                }
                Outlet::<Route> {}
            }
        }
        if app.settings_open.read().is_some() {
            SettingsDialog {}
        }
        if *app.search_open.read() {
            SearchPalette {}
        }
    }
}
