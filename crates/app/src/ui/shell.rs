//! The Option D frame: sidebar, main column, the ⌘K palette, and global shortcuts.

use dioxus::prelude::*;

use crate::routes::Route;
use crate::state::{LiveStatus, LoadStatus, use_app};
use crate::ui::common::{Icon, js};
use crate::ui::search::SearchPalette;
use crate::ui::sidebar::Sidebar;

#[component]
pub fn Shell() -> Element {
    let mut app = use_app();
    let nav = navigator();
    let route = use_route::<Route>();

    // Cmd/Ctrl+K opens search, Cmd/Ctrl+N opens Ask, Cmd/Ctrl+\ toggles the sidebar,
    // from anywhere in the window.
    use_future(move || async move {
        let mut eval = js(
            "MarginApp.watchShortcuts(function (m) { dioxus.send(m); });\nawait new Promise(function () {});",
        );
        while let Ok(message) = eval.recv::<String>().await {
            match message.as_str() {
                "search" => app.search_open.set(true),
                "sidebar" => {
                    let hidden = app.config.peek().sidebar_hidden;
                    let _ = app.set_config(|c| c.sidebar_hidden = !hidden);
                    app.drawer_open.set(hidden);
                }
                "ask" => {
                    app.search_open.set(false);
                    nav.push(Route::Ask {});
                }
                _ => {}
            }
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

    rsx! {
        div { class: if sidebar_hidden { "app sb-hidden" } else { "app" },
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
        if *app.search_open.read() {
            SearchPalette {}
        }
    }
}
