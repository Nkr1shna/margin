//! The Option D frame: sidebar, main column, the ⌘K palette, and global shortcuts.

use dioxus::prelude::*;

use crate::commands::Command;
use crate::routes::{Route, page_title};
use crate::state::{AppCtx, LiveStatus, LoadStatus, SourceViewer, use_app};
use crate::store::Milestones;
use crate::tools::app_control;
use crate::ui::citations::CitationViewer;
use crate::ui::common::{Icon, announce, js, sentence};
use crate::ui::search::SearchPalette;
use crate::ui::settings::{SettingsDialog, SettingsPane};
use crate::ui::sidebar::Sidebar;

/// Sidebar width limits when resizing by dragging, in CSS pixels.
const SIDEBAR_MIN: u32 = 200;
const SIDEBAR_MAX: u32 = 480;

/// Close the narrow-layout drawer and put focus back on the button that opened it.
pub fn close_drawer(app: AppCtx) {
    let mut open = app.drawer_open;
    if *open.peek() {
        open.set(false);
        let _ = js("MarginApp.focus('.menu-btn.menu-narrow');");
    }
}

#[component]
pub fn Shell() -> Element {
    let mut app = use_app();
    let nav = navigator();
    let route = use_route::<Route>();

    // The shortcuts in `Command`, from anywhere in the window. Key presses and the menu
    // bar (`SignedIn` in main.rs) both arrive here through `MarginApp.command`, which
    // drops them under a modal.
    use_future(move || async move {
        let mut eval = js(
            "MarginApp.watchShortcuts(function (m) { dioxus.send(m); });\nawait new Promise(function () {});",
        );
        while let Ok(message) = eval.recv::<String>().await {
            // Below 1024px the sidebar is a drawer: Toggle Sidebar opens and closes it
            // (`MarginApp.command` sends "drawer" in place of "sidebar" there).
            if message == "drawer" {
                app.search_open.set(false);
                if *app.drawer_open.peek() {
                    close_drawer(app);
                } else {
                    app.drawer_open.set(true);
                }
                continue;
            }
            let Some(command) = Command::from_slug(&message) else {
                continue;
            };
            // The search palette is the one modal commands pass through; they close it.
            if command != Command::Search {
                app.search_open.set(false);
            }
            match command {
                Command::Search => app.search_open.set(true),
                Command::AllNotes => {
                    nav.push(Route::AllNotes {});
                }
                Command::Settings => {
                    // Already open: stay on the pane the person is on.
                    if app.settings_open.peek().is_none() {
                        app.settings_open.set(Some(SettingsPane::General));
                    }
                }
                Command::Back => nav.go_back(),
                Command::Forward => nav.go_forward(),
                Command::ToggleSidebar => {
                    let hidden = app.config.peek().sidebar_hidden;
                    let _ = app.set_config(|c| c.sidebar_hidden = !hidden);
                    app.drawer_open.set(false);
                }
                Command::NewChat => {
                    nav.push(Route::Ask {});
                    // Ready to type straight away, also when a new chat is already open.
                    let _ = js(&format!(
                        "MarginApp.focus('#{}');",
                        crate::ui::chat::PROMPT_ID
                    ));
                }
            }
        }
    });

    // The drawer closes when the window widens out of the narrow layout.
    use_future(move || async move {
        let mut eval = js(
            "MarginApp.watchWide(function (w) { dioxus.send(w); });\nawait new Promise(function () {});",
        );
        while eval.recv::<bool>().await.is_ok() {
            app.drawer_open.set(false);
        }
    });

    // Opening the drawer moves focus into it.
    let drawer_open = *app.drawer_open.read();
    use_effect(use_reactive!(|drawer_open| {
        if drawer_open {
            let _ = js("MarginApp.focusDrawer('.sb.open .sb-toggle');");
        }
    }));

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
    // A chat starts at its end instead (`MarginApp.followChat`).
    use_effect(use_reactive!(|route| {
        let _ = route;
        app.drawer_open.set(false);
        document::eval(
            "requestAnimationFrame(function () { document.querySelectorAll('.main .scroll:not(.chat-scroll)').forEach(function (e) { e.scrollTop = 0; }); });",
        );
    }));

    // App-control calls from the agent (`tools`), run here on the main thread where the
    // router is. A citation in a lesson that isn't open waits in `parked` until the
    // lesson is the screen (the screen being left closes its viewer as it goes).
    let parked = use_signal(|| None::<SourceViewer>);
    use_effect(move || {
        if app.agent_commands.read().is_empty() {
            return;
        }
        let commands: Vec<_> = app.agent_commands.write().drain(..).collect();
        for command in commands {
            spawn(app_control::perform(app, parked, command));
        }
    });
    use_effect(use_reactive!(|route| {
        let mut parked = parked;
        let Some(viewer) = parked.take() else {
            return;
        };
        if route
            == (Route::LessonRead {
                id: viewer.lesson_id.clone(),
            })
        {
            app.source_viewer.set(Some(viewer));
        }
    }));

    // Tell screen readers when background work finishes: an outline drafted, a lesson
    // written, a video made (or any of them failing). Measured against the last state seen.
    let seen = use_hook(|| std::rc::Rc::new(std::cell::RefCell::new(None::<Milestones>)));
    use_effect(move || {
        let lib = app.lib.read();
        let mut seen = seen.borrow_mut();
        if let Some(before) = seen.as_ref() {
            let news = lib.announcements(before);
            if !news.is_empty() {
                announce(&news.join(" "));
            }
        }
        *seen = Some(lib.milestones());
    });

    let title = page_title(&route, &app.lib.read());
    let sidebar_hidden = app.config.read().sidebar_hidden;
    let sidebar_style = app
        .config
        .read()
        .sidebar_width
        .map(|w| format!("--sb-w: {w}px"))
        .unwrap_or_default();

    rsx! {
        document::Title { "{title}" }
        // The app's polite live region for completion messages (`common::announce`).
        div { id: "announcer", class: "sr-only", role: "status", "aria-live": "polite" }
        div {
            class: if sidebar_hidden { "app sb-hidden" } else { "app" },
            style: "{sidebar_style}",
            if drawer_open {
                div {
                    class: "drawer-scrim",
                    onclick: move |_| close_drawer(app),
                }
            }
            Sidebar {}
            main {
                class: "main",
                // Behind the open drawer the page is out of reach, for focus and screen readers.
                "inert": drawer_open.then_some("true"),
                // Every screen starts with a TopBar, which shows `StatusBanners` under it.
                Outlet::<Route> {}
            }
        }
        if app.settings_open.read().is_some() {
            SettingsDialog {}
        }
        if *app.search_open.read() {
            SearchPalette {}
        }
        if let Some(passage) = app.shown_passage.read().clone() {
            // A passage the agent showed (`show_passage`): one passage, no Previous/Next.
            CitationViewer {
                citations: vec![passage.clone()],
                n: passage.n,
                fallback: "main.main".to_string(),
                on_go: move |_| {},
                on_close: move |_| app.shown_passage.set(None),
            }
        }
    }
}

/// App-wide status under the top bar: the library failed to load, or the live
/// connection dropped and is being retried.
#[component]
pub fn StatusBanners() -> Element {
    let mut app = use_app();
    let lib_status = app.lib_status.read().clone();
    let live = app.live.read().clone();
    rsx! {
        if let LoadStatus::Failed(message) = lib_status {
            div { class: "banner error", role: "alert",
                Icon { name: "alert" }
                span { class: "grow", title: "{message}", "Couldn't load your library. {sentence(&message)}" }
                button {
                    class: "btn btn-secondary btn-sm",
                    onclick: move |_| {
                        let next = *app.reload.peek() + 1;
                        app.reload.set(next);
                        app.lib_status.set(LoadStatus::Loading);
                    },
                    "Try again"
                }
            }
        } else if let LiveStatus::Reconnecting { error, retry_secs } = live {
            if lib_status == LoadStatus::Loaded {
                div { class: "banner", role: "status",
                    span { class: "dot" }
                    span { class: "grow", title: "{sentence(&error)}",
                        "You're offline. Retrying in {retry_secs} s. "
                        span { class: "faint", "{sentence(&error)}" }
                    }
                    button {
                        class: "btn btn-secondary btn-sm",
                        onclick: move |_| {
                            let next = *app.reload.peek() + 1;
                            app.reload.set(next);
                        },
                        "Try now"
                    }
                }
            }
        }
    }
}
