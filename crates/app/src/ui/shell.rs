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

    // Cmd/Ctrl+K opens search, Cmd/Ctrl+N opens Ask, from anywhere in the window.
    use_future(move || async move {
        let mut eval = js(
            "MarginApp.watchShortcuts(function (m) { dioxus.send(m); });\nawait new Promise(function () {});",
        );
        while let Ok(message) = eval.recv::<String>().await {
            match message.as_str() {
                "search" => app.search_open.set(true),
                "ask" => {
                    app.search_open.set(false);
                    nav.push(Route::Ask {});
                }
                _ => {}
            }
        }
    });

    // Close the drawer whenever the route changes (narrow screens).
    use_effect(use_reactive!(|route| {
        let _ = route;
        app.drawer_open.set(false);
    }));

    let lib_status = app.lib_status.read().clone();
    let live = app.live.read().clone();
    let drawer_open = *app.drawer_open.read();

    rsx! {
        div { class: "app",
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
