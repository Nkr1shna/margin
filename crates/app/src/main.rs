//! The Margin app (desktop, iOS, Android): a thin, live client of the Margin service.

mod api;
#[cfg(all(feature = "desktop", target_os = "macos"))]
mod chrome;
mod config;
mod events;
#[cfg(test)]
mod fake;
mod markdown;
mod routes;
mod secrets;
mod sources;
mod state;
mod store;
mod ui;
mod zoom;

use std::collections::HashSet;
use std::time::Duration;

use dioxus::prelude::*;

use crate::api::Api;
use crate::events::ConnectError;
use crate::state::{AppCtx, LiveStatus, LoadStatus, Session};
use crate::store::LibraryState;
use crate::ui::common::js;

const MAIN_CSS: Asset = asset!("/assets/main.css");
/// Layout for the macOS unified title bar; loaded only when `UNIFIED_TITLE_BAR`.
const UNIFIED_CSS: Asset = asset!("/assets/unified.css");
const MARGIN_JS: Asset = asset!("/assets/margin.js");
const KATEX: Asset = asset!("/assets/katex", AssetOptions::folder());
const FONTS: Asset = asset!("/assets/fonts", AssetOptions::folder());

/// The page the webview starts from (in place of Dioxus's default): the document
/// language for screen readers, the app's name, and one viewport that leaves pinch
/// zoom on. Keeps Dioxus's head and module-loader markers.
const INDEX_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
    <head>
        <meta charset="utf-8">
        <title>Margin</title>
        <meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
        <!-- CUSTOM HEAD -->
    </head>
    <body>
        <div id="main"></div>
        <!-- MODULE LOADER -->
    </body>
</html>
"#;

fn main() {
    #[cfg(feature = "desktop")]
    {
        use dioxus::desktop::{Config, LogicalSize, WindowBuilder};
        let window = WindowBuilder::new()
            .with_title("Margin")
            .with_inner_size(LogicalSize::new(1240.0, 800.0))
            .with_min_inner_size(LogicalSize::new(360.0, 480.0));
        // Unified title bar: the content runs to the top edge, the traffic lights sit in
        // the sidebar's header row, and the app draws its own drag regions
        // (`ui::common::DragRegion`). `chrome` places the traffic lights.
        #[cfg(target_os = "macos")]
        let window = {
            use dioxus::desktop::tao::platform::macos::WindowBuilderExtMacOS;
            window
                .with_titlebar_transparent(true)
                .with_title_hidden(true)
                .with_fullsize_content_view(true)
        };
        dioxus::LaunchBuilder::new()
            .with_cfg(with_index(
                Config::new().with_window(window).with_menu(menu_bar()),
            ))
            .launch(App);
    }
    #[cfg(all(feature = "mobile", not(feature = "desktop")))]
    dioxus::LaunchBuilder::new()
        .with_cfg(with_index(dioxus::mobile::Config::new()))
        .launch(App);
    #[cfg(not(any(feature = "desktop", feature = "mobile")))]
    dioxus::launch(App);
}

/// Release builds start from `INDEX_HTML`. Debug builds keep Dioxus's development page
/// (it shows `dx serve`'s rebuild notices); margin.js sets its language and the root
/// adds the viewport there.
#[cfg(feature = "desktop")]
fn with_index(cfg: dioxus::desktop::Config) -> dioxus::desktop::Config {
    if cfg!(debug_assertions) {
        cfg
    } else {
        cfg.with_custom_index(INDEX_HTML.to_string())
    }
}

#[cfg(all(feature = "mobile", not(feature = "desktop")))]
fn with_index(cfg: dioxus::mobile::Config) -> dioxus::mobile::Config {
    if cfg!(debug_assertions) {
        cfg
    } else {
        cfg.with_custom_index(INDEX_HTML.to_string())
    }
}

/// Menu item ids for the View menu's zoom commands.
#[cfg(feature = "desktop")]
const ZOOM_IN: &str = "zoom-in";
#[cfg(feature = "desktop")]
const ZOOM_OUT: &str = "zoom-out";
#[cfg(feature = "desktop")]
const ZOOM_RESET: &str = "zoom-reset";

/// The native menu bar. Besides the usual app and window commands it carries the Edit
/// menu, which is what makes Cmd+Z/X/C/V/A work in text fields: the webview handles those
/// shortcuts only through the menu's standard items.
#[cfg(feature = "desktop")]
fn menu_bar() -> dioxus::desktop::muda::Menu {
    use dioxus::desktop::muda::accelerator::{Accelerator, CMD_OR_CTRL, Code};
    use dioxus::desktop::muda::{Menu, MenuItem, PredefinedMenuItem, Submenu};

    let app = Submenu::new("Margin", true);
    let edit = Submenu::new("Edit", true);
    let view = Submenu::new("View", true);
    let zoom_item = |id: &str, label: &str, key: Code| {
        MenuItem::with_id(
            id,
            label,
            true,
            Some(Accelerator::new(Some(CMD_OR_CTRL), key)),
        )
    };
    let (actual, bigger, smaller) = (
        zoom_item(ZOOM_RESET, "Actual Size", Code::Digit0),
        zoom_item(ZOOM_IN, "Zoom In", Code::Equal),
        zoom_item(ZOOM_OUT, "Zoom Out", Code::Minus),
    );
    let window = Submenu::new("Window", true);
    let menu = Menu::new();
    let built = app
        .append_items(&[
            &PredefinedMenuItem::about(Some("About Margin"), None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::services(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::hide(Some("Hide Margin")),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::quit(Some("Quit Margin")),
        ])
        .and_then(|_| {
            edit.append_items(&[
                &PredefinedMenuItem::undo(None),
                &PredefinedMenuItem::redo(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::cut(None),
                &PredefinedMenuItem::copy(None),
                &PredefinedMenuItem::paste(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::select_all(None),
            ])
        })
        .and_then(|_| view.append_items(&[&actual, &bigger, &smaller]))
        .and_then(|_| {
            window.append_items(&[
                &PredefinedMenuItem::minimize(None),
                &PredefinedMenuItem::maximize(Some("Zoom")),
                &PredefinedMenuItem::fullscreen(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::bring_all_to_front(None),
                &PredefinedMenuItem::close_window(None),
            ])
        })
        .and_then(|_| menu.append_items(&[&app, &edit, &view, &window]));
    if let Err(error) = built {
        eprintln!("couldn't build the menu bar: {error}");
    }
    menu
}

#[component]
fn App() -> Element {
    #[cfg(all(feature = "desktop", target_os = "macos"))]
    chrome::use_traffic_lights();
    let ctx = use_context_provider(|| {
        let config = config::load();
        // Restore the session from the keychain, if this device signed in before.
        let session = config.service_url.as_ref().and_then(|url| {
            secrets::load_token(url)
                .ok()
                .flatten()
                .map(|token| Session {
                    api: Api::with_token(url, &token),
                    me: None,
                })
        });
        AppCtx {
            config: Signal::new(config),
            session: Signal::new(session),
            signed_out_reason: Signal::new(None),
            lib: Signal::new(LibraryState::default()),
            lib_status: Signal::new(LoadStatus::Loading),
            reload: Signal::new(0),
            live: Signal::new(LiveStatus::Connecting),
            search_open: Signal::new(false),
            drawer_open: Signal::new(false),
            collapsed: Signal::new(HashSet::new()),
            ask_proposal: Signal::new(None),
            ask_prompt: Signal::new(String::new()),
            uploads: Signal::new(Vec::new()),
            settings_open: Signal::new(None),
        }
    });

    // View > Zoom In / Zoom Out / Actual Size change the saved zoom level.
    #[cfg(feature = "desktop")]
    dioxus::desktop::use_muda_event_handler(move |event| {
        let id = event.id().0.as_str();
        let current = ctx.config.peek().zoom;
        let next = match id {
            ZOOM_IN => zoom::zoom_in(current),
            ZOOM_OUT => zoom::zoom_out(current),
            ZOOM_RESET => zoom::DEFAULT,
            _ => return,
        };
        if next != current {
            let _ = ctx.set_config(|c| c.zoom = next);
        }
    });
    zoom::use_page_zoom(ctx);

    // Appearance: System follows prefers-color-scheme; Light/Dark set <html data-theme>.
    use_effect(move || {
        let theme = ctx.config.read().appearance.data_theme();
        let arg = serde_json::to_string(&theme).unwrap_or_else(|_| "null".into());
        let _ = js(&format!("MarginApp.setTheme({arg});"));
    });

    let fonts = format!(
        "@font-face {{ font-family: 'Albert Sans'; font-style: normal; font-weight: 100 900; font-display: swap; src: url('{FONTS}/AlbertSans-wght.ttf') format('truetype'); }}\n\
         @font-face {{ font-family: 'Albert Sans'; font-style: italic; font-weight: 100 900; font-display: swap; src: url('{FONTS}/AlbertSans-Italic-wght.ttf') format('truetype'); }}"
    );
    let signed_in = ctx.session.read().is_some();

    rsx! {
        if cfg!(debug_assertions) {
            // Development page only (see `with_index`): replaces its no-zoom viewport.
            document::Meta { name: "viewport", content: "width=device-width, initial-scale=1, viewport-fit=cover" }
        }
        document::Style { {fonts} }
        document::Stylesheet { href: MAIN_CSS }
        if ui::common::UNIFIED_TITLE_BAR {
            document::Stylesheet { href: UNIFIED_CSS }
        }
        document::Stylesheet { href: format!("{KATEX}/katex.min.css") }
        document::Script { src: format!("{KATEX}/katex.min.js") }
        document::Script { src: MARGIN_JS }
        if signed_in {
            SignedIn {}
        } else {
            document::Title { "Sign in — Margin" }
            ui::signin::ConnectScreen {}
        }
    }
}

/// Loads the library, keeps it live over the events socket, and renders the router.
#[component]
fn SignedIn() -> Element {
    let ctx = state::use_app();
    use_future(move || async move { live_loop(ctx).await });
    // Fetch the account (GitHub login, this device) once.
    use_future(move || async move {
        let Some(api) = ctx.api() else { return };
        match api.me().await {
            Ok(me) => {
                let mut session = ctx.session;
                if let Some(s) = session.write().as_mut() {
                    s.me = Some(me);
                }
            }
            Err(e) => {
                ctx.failure(&e);
            }
        }
    });
    rsx! { Router::<routes::Route> {} }
}

pub async fn load_library(ctx: AppCtx, api: &Api) -> bool {
    let mut status = ctx.lib_status;
    let mut lib = ctx.lib;
    match api.library().await {
        Ok(library) => {
            lib.write().replace_library(library);
            status.set(LoadStatus::Loaded);
            true
        }
        Err(e) => {
            let message = ctx.failure(&e);
            // Keep showing what we have if this is a reload.
            if *status.peek() != LoadStatus::Loaded {
                status.set(LoadStatus::Failed(message));
            }
            false
        }
    }
}

async fn live_loop(ctx: AppCtx) {
    let Some(api) = ctx.api() else { return };
    let Some(token) = api.token().map(str::to_string) else {
        return;
    };
    let mut live = ctx.live;
    let mut lib = ctx.lib;
    let mut reload_seen = *ctx.reload.peek();
    load_library(ctx, &api).await;
    let mut attempt: u32 = 0;
    let mut connected_before = false;
    loop {
        live.set(LiveStatus::Connecting);
        let error = match events::connect(&api.events_url(), &token).await {
            Ok(mut socket) => {
                attempt = 0;
                if connected_before || *ctx.lib_status.peek() != LoadStatus::Loaded {
                    // Events may have been missed while disconnected.
                    load_library(ctx, &api).await;
                }
                connected_before = true;
                live.set(LiveStatus::Live);
                loop {
                    tokio::select! {
                        event = events::next_event(&mut socket) => match event {
                            Some(event) => lib.write().apply(event),
                            None => break,
                        },
                        _ = tokio::time::sleep(Duration::from_millis(250)) => {
                            let wanted = *ctx.reload.peek();
                            if wanted != reload_seen {
                                reload_seen = wanted;
                                load_library(ctx, &api).await;
                            }
                        }
                    }
                }
                "The live connection closed.".to_string()
            }
            Err(ConnectError::Unauthorized) => {
                ctx.forget_session(Some(
                    "This device was signed out by the service. Sign in again.".into(),
                ));
                return;
            }
            Err(ConnectError::Other(message)) => message,
        };
        let delay = events::backoff(attempt);
        attempt = attempt.saturating_add(1);
        live.set(LiveStatus::Reconnecting {
            error,
            retry_secs: delay.as_secs(),
        });
        // Wait, but honor a manual reload request (Retry) in the meantime.
        let deadline = tokio::time::Instant::now() + delay;
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let wanted = *ctx.reload.peek();
            if wanted != reload_seen {
                reload_seen = wanted;
                load_library(ctx, &api).await;
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::INDEX_HTML;

    #[test]
    fn index_page_is_accessible_and_bootable() {
        // What Dioxus needs: a head to add styles to, a body for its loader, the root.
        for needed in ["</head>", "</body>", r#"<div id="main"></div>"#] {
            assert!(INDEX_HTML.contains(needed), "{needed}");
        }
        assert!(INDEX_HTML.contains(r#"<html lang="en">"#));
        assert!(INDEX_HTML.contains("<title>Margin</title>"));
        // One viewport, which leaves pinch zoom alone.
        assert_eq!(INDEX_HTML.matches(r#"name="viewport""#).count(), 1);
        assert!(!INDEX_HTML.contains("user-scalable"));
        assert!(!INDEX_HTML.contains("maximum-scale"));
    }
}
