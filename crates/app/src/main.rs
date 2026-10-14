//! The Margin app (desktop, iOS, Android): a thin, live client of the Margin service.

mod api;
mod config;
mod events;
#[cfg(test)]
mod fake;
mod markdown;
mod routes;
mod secrets;
mod state;
mod store;
mod ui;

use std::collections::HashSet;
use std::time::Duration;

use dioxus::prelude::*;

use crate::api::Api;
use crate::events::ConnectError;
use crate::state::{AppCtx, LiveStatus, LoadStatus, Session};
use crate::store::LibraryState;
use crate::ui::common::js;

const MAIN_CSS: Asset = asset!("/assets/main.css");
const MARGIN_JS: Asset = asset!("/assets/margin.js");
const KATEX: Asset = asset!("/assets/katex", AssetOptions::folder());
const FONTS: Asset = asset!("/assets/fonts", AssetOptions::folder());

fn main() {
    #[cfg(feature = "desktop")]
    {
        use dioxus::desktop::{Config, LogicalSize, WindowBuilder};
        let window = WindowBuilder::new()
            .with_title("Margin")
            .with_inner_size(LogicalSize::new(1240.0, 800.0))
            .with_min_inner_size(LogicalSize::new(360.0, 480.0));
        dioxus::LaunchBuilder::new()
            .with_cfg(Config::new().with_window(window).with_menu(None))
            .launch(App);
    }
    #[cfg(not(feature = "desktop"))]
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
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
        }
    });

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
        document::Meta { name: "viewport", content: "width=device-width, initial-scale=1, viewport-fit=cover" }
        document::Style { {fonts} }
        document::Stylesheet { href: MAIN_CSS }
        document::Stylesheet { href: format!("{KATEX}/katex.min.css") }
        document::Script { src: format!("{KATEX}/katex.min.js") }
        document::Script { src: MARGIN_JS }
        if signed_in {
            SignedIn {}
        } else {
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
