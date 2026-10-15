//! The Margin app (desktop, iOS, Android): a thin, live client of the Margin service.

mod api;
#[cfg(all(feature = "desktop", target_os = "macos"))]
mod chrome;
mod citations;
mod commands;
mod config;
mod drafts;
mod events;
#[cfg(test)]
mod fake;
// `StdDir` and some `DirError`s are used by the platforms' `open` (4a, 4b, 4c), which
// aren't built on every platform yet.
#[allow(dead_code)]
mod folders;
mod markdown;
mod picker;
mod routes;
#[cfg(target_os = "ios")]
mod scene;
mod secrets;
mod sources;
mod state;
mod store;
mod tools;
mod ui;
mod zoom;

use std::collections::HashSet;
use std::time::Duration;

use dioxus::prelude::*;
use futures_util::StreamExt;
use margin_shared::{Event, ServerFrame};

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

/// The page viewport. `viewport-fit=cover` runs the page under the notch and home
/// indicator (main.css pads with the safe-area insets). Pinch zoom stays on. On Android
/// the soft keyboard resizes the page, so fixed panels and 100vh stay above it.
#[cfg(target_os = "android")]
macro_rules! index_viewport {
    () => {
        "width=device-width, initial-scale=1, viewport-fit=cover, interactive-widget=resizes-content"
    };
}
#[cfg(not(target_os = "android"))]
macro_rules! index_viewport {
    () => {
        "width=device-width, initial-scale=1, viewport-fit=cover"
    };
}

/// The page the webview starts from (in place of Dioxus's default): the document
/// language for screen readers, the app's name, and one viewport (`index_viewport`).
/// Keeps Dioxus's head and module-loader markers.
const INDEX_HTML: &str = concat!(
    r#"<!DOCTYPE html>
<html lang="en">
    <head>
        <meta charset="utf-8">
        <title>Margin</title>
        <meta name="viewport" content=""#,
    index_viewport!(),
    r#"">
        <!-- CUSTOM HEAD -->
    </head>
    <body>
        <div id="main"></div>
        <!-- MODULE LOADER -->
    </body>
</html>
"#
);

fn main() {
    picker::clear_staging();
    // The folders this device holds, before anything syncs them.
    folders::store::load();
    #[cfg(target_os = "ios")]
    scene::register();
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
                Config::new()
                    .with_window(window)
                    .with_menu(menu_bar())
                    .with_custom_event_handler(flush_on_exit),
            ))
            .launch(App);
    }
    #[cfg(all(feature = "mobile", not(feature = "desktop")))]
    dioxus::LaunchBuilder::new()
        .with_cfg(with_index(
            dioxus::mobile::Config::new().with_custom_event_handler(lifecycle),
        ))
        .launch(App);
    #[cfg(not(any(feature = "desktop", feature = "mobile")))]
    dioxus::launch(App);
}

/// Closing the window or quitting ends the app's runtime, and with it any note save
/// still waiting on its debounce; send those edits first.
#[cfg(feature = "desktop")]
fn flush_on_exit<T>(
    event: &dioxus::desktop::tao::event::Event<'_, T>,
    _: &dioxus::desktop::tao::event_loop::EventLoopWindowTarget<T>,
) {
    use dioxus::desktop::tao::event::{Event, WindowEvent};
    if matches!(
        event,
        Event::LoopDestroyed
            | Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            }
    ) {
        drafts::flush_blocking(Duration::from_secs(3));
    }
}

/// Android: the activity pausing and resuming arrive as tao's `Suspended` and `Resumed`;
/// the app is in the background in between (`state::set_foreground`). On iOS these
/// don't fire under scenes, so the scene delegate reports it (src/scene.rs).
#[cfg(all(feature = "mobile", not(feature = "desktop")))]
fn lifecycle<T>(
    event: &dioxus::mobile::tao::event::Event<'_, T>,
    _: &dioxus::mobile::tao::event_loop::EventLoopWindowTarget<T>,
) {
    #[cfg(target_os = "android")]
    {
        use dioxus::mobile::tao::event::Event;
        match event {
            Event::Suspended => state::set_foreground(false),
            Event::Resumed => state::set_foreground(true),
            _ => {}
        }
    }
    #[cfg(not(target_os = "android"))]
    let _ = event;
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
/// shortcuts only through the menu's standard items. The app's own commands (Settings,
/// File, View, Go) carry their shortcuts so they can be found here; choosing one runs it
/// like the key press (`ui::shell::Shell` handles both).
#[cfg(feature = "desktop")]
fn menu_bar() -> dioxus::desktop::muda::Menu {
    use dioxus::desktop::muda::accelerator::{Accelerator, CMD_OR_CTRL, Code, Modifiers};
    use dioxus::desktop::muda::{Menu, MenuItem, PredefinedMenuItem, Submenu};

    use crate::commands::Command;

    let item = |label: &str, command: Command| {
        let code = match command {
            Command::Search => Code::KeyK,
            Command::NewChat | Command::AllNotes => Code::KeyN,
            Command::Settings => Code::Comma,
            Command::Back => Code::BracketLeft,
            Command::Forward => Code::BracketRight,
            Command::ToggleSidebar => Code::Backslash,
        };
        let mods = if command.shift() {
            CMD_OR_CTRL | Modifiers::SHIFT
        } else {
            CMD_OR_CTRL
        };
        MenuItem::with_id(
            command.slug(),
            label,
            true,
            Some(Accelerator::new(Some(mods), code)),
        )
    };
    let settings = item("Settings…", Command::Settings);
    let new_chat = item("New Chat", Command::NewChat);
    let all_notes = item("All Notes", Command::AllNotes);
    let toggle_sidebar = item("Toggle Sidebar", Command::ToggleSidebar);
    let back = item("Back", Command::Back);
    let forward = item("Forward", Command::Forward);
    let search = item("Search…", Command::Search);

    let app = Submenu::new("Margin", true);
    let file = Submenu::new("File", true);
    let edit = Submenu::new("Edit", true);
    let view = Submenu::new("View", true);
    let go = Submenu::new("Go", true);
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
            &settings,
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::services(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::hide(Some("Hide Margin")),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::quit(Some("Quit Margin")),
        ])
        .and_then(|_| file.append_items(&[&new_chat, &all_notes]))
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
        .and_then(|_| {
            view.append_items(&[
                &toggle_sidebar,
                &PredefinedMenuItem::separator(),
                &actual,
                &bigger,
                &smaller,
            ])
        })
        .and_then(|_| {
            go.append_items(&[&back, &forward, &PredefinedMenuItem::separator(), &search])
        })
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
        .and_then(|_| menu.append_items(&[&app, &file, &edit, &view, &go, &window]));
    if let Err(error) = built {
        eprintln!("couldn't build the menu bar: {error}");
    }
    menu
}

#[component]
fn App() -> Element {
    #[cfg(all(feature = "desktop", target_os = "macos"))]
    chrome::use_traffic_lights();
    // iOS: the window shows once it joins the UIKit scene (src/scene.rs).
    #[cfg(target_os = "ios")]
    use_hook(|| {
        #[cfg(feature = "desktop")]
        use dioxus::desktop as host;
        #[cfg(not(feature = "desktop"))]
        use dioxus::mobile as host;
        use host::tao::platform::ios::WindowExtIOS;
        scene::adopt_window(host::window().window.ui_window());
    });
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
            outline_drafts: Signal::new(Default::default()),
            ask_prompt: Signal::new(String::new()),
            uploads: Signal::new(Vec::new()),
            settings_open: Signal::new(None),
            source_viewer: Signal::new(None),
            transcripts: Signal::new(Default::default()),
            shown_passage: Signal::new(None),
            device_frames: Signal::new(None),
            agent_commands: Signal::new(Default::default()),
            foreground: Signal::new(*state::foreground_changes().borrow()),
            folders: Signal::new(folders::store::snapshot()),
            folder_sync: Signal::new(Default::default()),
            ask_workspaces: Signal::new(Vec::new()),
        }
    });

    // The held folders and their syncs into `AppCtx::folders` and `folder_sync` (the
    // store and the engine never touch a signal themselves).
    use_future(move || async move {
        let mut changes = folders::store::changes();
        let mut held = ctx.folders;
        loop {
            changes.borrow_and_update();
            let now = folders::store::snapshot();
            if *held.peek() != now {
                held.set(now);
            }
            if changes.changed().await.is_err() {
                return;
            }
        }
    });
    use_future(move || async move {
        let mut states = folders::sync::engine().states();
        let mut shown = ctx.folder_sync;
        loop {
            let now = states.borrow_and_update().clone();
            if *shown.peek() != now {
                shown.set(now);
            }
            if states.changed().await.is_err() {
                return;
            }
        }
    });

    // The platform's lifecycle (src/scene.rs on iOS, `lifecycle` on Android) into
    // `AppCtx::foreground`, which the live loop follows.
    use_future(move || async move {
        let mut changes = state::foreground_changes();
        let mut foreground = ctx.foreground;
        loop {
            let now = *changes.borrow_and_update();
            if *foreground.peek() != now {
                foreground.set(now);
            }
            if changes.changed().await.is_err() {
                return;
            }
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
            document::Meta { name: "viewport", content: index_viewport!() }
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
    // The menu bar's app commands run like their key presses (`ui::shell::Shell`).
    #[cfg(all(feature = "desktop", any(target_os = "macos", target_os = "linux")))]
    dioxus::desktop::use_muda_event_handler(move |event| {
        if let Some(command) = commands::Command::from_slug(&event.id().0) {
            commands::dispatch(command);
        }
    });
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
            // Note edits an earlier run (or a failed save) left unsent.
            drafts::resume(ctx, api);
            load_workspaces(ctx, api).await;
            load_chats(ctx, api).await;
            true
        }
        Err(e) => {
            let message = ctx.failure(&e);
            // Keep showing what we have if this is a reload.
            if *status.peek() != LoadStatus::Loaded {
                status.set(LoadStatus::Failed(message));
            }
            // Chats load on their own (and their transcripts may have missed events).
            load_chats(ctx, api).await;
            false
        }
    }
}

/// The attached folders of every device. A held folder the service no longer lists was
/// removed while this device was away: it is let go (only after a list that loaded).
async fn load_workspaces(ctx: AppCtx, api: &Api) {
    match api.workspaces().await {
        Ok(list) => {
            let ids = list.iter().map(|w| w.id.clone()).collect();
            let mut lib = ctx.lib;
            lib.write().replace_workspaces(list);
            folders::sync::prune_held(ids).await;
        }
        Err(e) => {
            ctx.failure(&e);
        }
    }
}

/// How often the held folders are synced while the app runs.
const FOLDER_SYNC_EVERY: Duration = Duration::from_secs(margin_shared::FOLDER_SYNC_INTERVAL_SECS);

/// How long the live loop waits before asking for the chat list again after it failed.
const CHATS_RETRY: Duration = Duration::from_secs(5);

/// The chat list, and the transcripts open this session again: events may have been
/// missed while they were away. Transcripts that couldn't be fetched (all of them when
/// the list couldn't) are marked stale, so an open chat fetches itself again; a list
/// that never loaded is asked for again by the live loop (`CHATS_RETRY`). Whether the
/// list loaded.
async fn load_chats(ctx: AppCtx, api: &Api) -> bool {
    let mut lib = ctx.lib;
    let open: Vec<margin_shared::Id> = lib.peek().chat_items.keys().cloned().collect();
    match api.chats().await {
        Ok(chats) => lib.write().replace_chats(chats),
        Err(e) => {
            ctx.failure(&e);
            lib.write().stale_chats.extend(open);
            return false;
        }
    }
    let open: Vec<margin_shared::Id> = lib.peek().chat_items.keys().cloned().collect();
    for id in open {
        match api.chat(&id).await {
            Ok(detail) => lib.write().load_chat(detail),
            Err(_) => {
                lib.write().stale_chats.insert(id);
            }
        }
    }
    true
}

/// How often the live loop looks at the reload button, the chat list and the app's
/// foreground state while connected or waiting.
const LIVE_TICK: Duration = Duration::from_millis(250);

/// The devices again, with who is online now (`DeviceChanged` only says what changes
/// while the socket is live). Settings shows the ones known before if this fails.
async fn load_devices(ctx: AppCtx, api: &Api) {
    match api.devices().await {
        Ok(list) => {
            let mut lib = ctx.lib;
            lib.write().replace_devices(list);
        }
        Err(e) => {
            ctx.failure(&e);
        }
    }
}

/// This device's own presence, which the service never reports to the device itself:
/// online exactly while its socket is live.
fn set_self_online(ctx: AppCtx, online: bool) {
    let mut lib = ctx.lib;
    let me = ctx.session.peek().as_ref().and_then(|s| {
        s.me.as_ref()
            .and_then(|me| me.device.as_ref())
            .map(|d| d.id.clone())
    });
    let id = me.or_else(|| {
        lib.peek()
            .devices
            .values()
            .find(|d| d.current)
            .map(|d| d.id.clone())
    });
    let Some(id) = id else { return };
    let changed = lib
        .peek()
        .devices
        .get(&id)
        .is_some_and(|d| d.online != online);
    if changed && let Some(device) = lib.write().devices.get_mut(&id) {
        device.online = online;
    }
}

/// Keeps the library live over the events socket and runs the device tools the service
/// sends this device. The loop owns both halves of the socket: frames for the service
/// (tool answers) come to it through `AppCtx::device_frames`. On iOS, iPadOS and Android
/// the socket is closed while the app is in the background and opened again when it
/// comes back.
async fn live_loop(ctx: AppCtx) {
    let Some(api) = ctx.api() else { return };
    let Some(token) = api.token().map(str::to_string) else {
        return;
    };
    let mut live = ctx.live;
    let mut lib = ctx.lib;
    let mut device_frames = ctx.device_frames;
    let mut reload_seen = *ctx.reload.peek();
    let runner = tools::Runner::new();
    let host = tools::AppHost(ctx);
    let engine = folders::sync::engine();
    engine.set_api(Some(api.clone()));
    load_library(ctx, &api).await;
    let mut chats_retry_at = tokio::time::Instant::now() + CHATS_RETRY;
    let mut attempt: u32 = 0;
    let mut connected_before = false;
    loop {
        if !*ctx.foreground.peek() {
            // Uploads stop with the app (iOS, iPadOS, Android); parts resume later.
            engine.pause();
            live.set(LiveStatus::Background);
            while !*ctx.foreground.peek() {
                tokio::time::sleep(LIVE_TICK).await;
            }
            attempt = 0;
        }
        live.set(LiveStatus::Connecting);
        let error = match events::connect(&api.events_url(), &token).await {
            Ok(socket) => {
                attempt = 0;
                if connected_before || *ctx.lib_status.peek() != LoadStatus::Loaded {
                    // Events may have been missed while disconnected.
                    load_library(ctx, &api).await;
                }
                if connected_before {
                    // So was who came online or went offline meanwhile.
                    load_devices(ctx, &api).await;
                }
                connected_before = true;
                live.set(LiveStatus::Live);
                set_self_online(ctx, true);
                // Every held folder syncs once the socket is live (launch, reconnect,
                // back in front), then every `FOLDER_SYNC_EVERY`.
                engine.resume();
                engine.sync_all();
                let mut next_folder_sync = tokio::time::Instant::now() + FOLDER_SYNC_EVERY;
                let (mut sender, mut frames) = socket.split();
                // This connection's outgoing frames; device tools get the sender once
                // the service said hello.
                let (outgoing_tx, mut outgoing) = tokio::sync::mpsc::unbounded_channel();
                let mut outgoing_tx = Some(outgoing_tx);
                let mut tick = tokio::time::interval(LIVE_TICK);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                let error = loop {
                    tokio::select! {
                        frame = events::next_frame(&mut frames) => match frame {
                            Some(ServerFrame::Event(event)) => {
                                if matches!(event, Event::Hello { .. })
                                    && let Some(tx) = outgoing_tx.take()
                                {
                                    device_frames.set(Some(tx));
                                }
                                lib.write().apply(event);
                            }
                            // Device-tool requests for this device and their cancellations.
                            Some(ServerFrame::Directed(frame)) => {
                                runner.handle(frame, &host, |task| {
                                    spawn(task);
                                });
                            }
                            None => break "The live connection closed.".to_string(),
                        },
                        Some(frame) = outgoing.recv() => {
                            if let Err(e) = events::send_frame(&mut sender, &frame).await {
                                break format!("Live updates are disconnected ({e}).");
                            }
                        }
                        _ = tick.tick() => {
                            if !*ctx.foreground.peek() {
                                engine.pause();
                                events::close(&mut sender).await;
                                break String::new();
                            }
                            set_self_online(ctx, true);
                            if tokio::time::Instant::now() >= next_folder_sync {
                                engine.sync_all();
                                next_folder_sync = tokio::time::Instant::now() + FOLDER_SYNC_EVERY;
                            }
                            let wanted = *ctx.reload.peek();
                            if wanted != reload_seen {
                                reload_seen = wanted;
                                load_library(ctx, &api).await;
                            } else if !lib.peek().chats_loaded
                                && tokio::time::Instant::now() >= chats_retry_at
                            {
                                // The chat list never loaded: the sidebar waits on it.
                                load_chats(ctx, &api).await;
                                chats_retry_at = tokio::time::Instant::now() + CHATS_RETRY;
                            }
                        }
                    }
                };
                // The service gives up on this connection's requests; so does the app.
                device_frames.set(None);
                runner.cancel_all();
                set_self_online(ctx, false);
                error
            }
            Err(ConnectError::Unauthorized) => {
                ctx.forget_session(Some(
                    "This device was signed out by the service. Sign in again.".into(),
                ));
                return;
            }
            Err(ConnectError::Other(message)) => message,
        };
        // Gone to the background: wait for the app to come back, not for a retry.
        if !*ctx.foreground.peek() {
            continue;
        }
        let delay = events::backoff(attempt);
        attempt = attempt.saturating_add(1);
        live.set(LiveStatus::Reconnecting {
            error,
            retry_secs: delay.as_secs(),
        });
        // Wait, but honor a manual reload request (Retry) in the meantime.
        let deadline = tokio::time::Instant::now() + delay;
        while tokio::time::Instant::now() < deadline && *ctx.foreground.peek() {
            tokio::time::sleep(LIVE_TICK).await;
            set_self_online(ctx, false);
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
