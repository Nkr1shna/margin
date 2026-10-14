//! Render tests: the signed-in app rendered to HTML on a route, with a given library and
//! no window or service. Effects, futures and JS calls don't run, so what shows is each
//! screen's first render (like `examples/render_screens.rs`).

use std::collections::HashSet;
use std::rc::Rc;

use chrono::Utc;
use dioxus::history::{History, MemoryHistory};
use dioxus::prelude::*;
use margin_shared::{Device, Me};

use crate::api::Api;
use crate::config::{Appearance, LocalConfig};
use crate::routes::Route;
use crate::state::{AppCtx, LiveStatus, LoadStatus, Session};
use crate::store::LibraryState;
use crate::ui::settings::SettingsPane;

#[derive(Props, Clone, PartialEq)]
struct HarnessProps {
    route: String,
    lib: LibraryState,
    settings: Option<SettingsPane>,
}

#[allow(non_snake_case)]
fn Harness(props: HarnessProps) -> Element {
    let route = props.route.clone();
    use_context_provider(|| Rc::new(MemoryHistory::with_initial_path(route)) as Rc<dyn History>);
    let lib = props.lib.clone();
    let settings = props.settings;
    use_context_provider(move || AppCtx {
        config: Signal::new(LocalConfig {
            service_url: Some("https://margin.example".into()),
            appearance: Appearance::System,
            sidebar_hidden: false,
            sidebar_width: None,
            zoom: 100,
        }),
        session: Signal::new(Some(Session {
            api: Api::with_token("http://127.0.0.1:9", "x"),
            me: Some(Me {
                github_login: "Nkr1shna".into(),
                device: Device {
                    id: "d1".into(),
                    name: "test-mac".into(),
                    created_at: Utc::now(),
                    last_seen_at: None,
                    current: true,
                    online: true,
                },
            }),
        })),
        signed_out_reason: Signal::new(None),
        lib: Signal::new(lib),
        lib_status: Signal::new(LoadStatus::Loaded),
        reload: Signal::new(0),
        live: Signal::new(LiveStatus::Live),
        search_open: Signal::new(false),
        drawer_open: Signal::new(false),
        collapsed: Signal::new(HashSet::new()),
        outline_drafts: Signal::new(Default::default()),
        ask_prompt: Signal::new(String::new()),
        uploads: Signal::new(Vec::new()),
        settings_open: Signal::new(settings),
        source_viewer: Signal::new(None),
        transcripts: Signal::new(Default::default()),
    });
    rsx! { Router::<Route> {} }
}

/// The app's HTML on `route` with `lib` loaded.
pub fn render_route(route: &str, lib: LibraryState) -> String {
    render_with_settings(route, lib, None)
}

/// The app's HTML on `route` with Settings open at `pane`.
pub fn render_with_settings(route: &str, lib: LibraryState, pane: Option<SettingsPane>) -> String {
    let mut dom = VirtualDom::new_with_props(
        Harness,
        HarnessProps {
            route: route.into(),
            lib,
            settings: pane,
        },
    );
    dom.rebuild_in_place();
    dioxus_ssr::render(&dom)
}
