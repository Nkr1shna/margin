//! Render tests: the signed-in app rendered to HTML on a route, with a given library and
//! no window or service. Effects, futures and JS calls don't run, so what shows is each
//! screen's first render (like `examples/render_screens.rs`).

use std::collections::{BTreeMap, HashSet};
use std::rc::Rc;

use chrono::Utc;
use dioxus::history::{History, MemoryHistory};
use dioxus::prelude::*;
use margin_shared::{Citation, Device, Id, Me};

use crate::api::Api;
use crate::config::{Appearance, LocalConfig};
use crate::folders::store::LocalFolders;
use crate::folders::sync::SyncState;
use crate::routes::Route;
use crate::state::{AppCtx, LiveStatus, LoadStatus, Session};
use crate::store::LibraryState;
use crate::ui::settings::SettingsPane;

#[derive(Props, Clone, PartialEq)]
struct HarnessProps {
    route: String,
    lib: LibraryState,
    settings: Option<SettingsPane>,
    extra: Extra,
}

/// What else a render test sets up: this device's folders and their syncs, the new chat's
/// folders, a passage open in the viewer.
#[derive(Clone, Default, PartialEq)]
pub struct Extra {
    pub folders: LocalFolders,
    pub folder_sync: BTreeMap<Id, SyncState>,
    pub ask_workspaces: Vec<Id>,
    pub shown_passage: Option<Citation>,
    /// Signed out at this configured service URL: the sign-in screen renders instead.
    pub connect: Option<Option<String>>,
}

#[allow(non_snake_case)]
fn Harness(props: HarnessProps) -> Element {
    let route = props.route.clone();
    use_context_provider(|| Rc::new(MemoryHistory::with_initial_path(route)) as Rc<dyn History>);
    let lib = props.lib.clone();
    let settings = props.settings;
    let extra = props.extra.clone();
    let connect = extra.connect.clone();
    let signed_in = connect.is_none();
    use_context_provider(move || AppCtx {
        config: Signal::new(LocalConfig {
            service_url: connect.unwrap_or(Some("https://margin.example".into())),
            appearance: Appearance::System,
            sidebar_hidden: false,
            sidebar_width: None,
            zoom: 100,
        }),
        session: Signal::new(signed_in.then(|| Session {
            api: Api::with_token("http://127.0.0.1:9", "x"),
            me: Some(Me {
                agent: None,
                person: None,
                github_login: "Nkr1shna".into(),
                device: Some(Device {
                    person_id: None,
                    id: "d1".into(),
                    name: "test-mac".into(),
                    created_at: Utc::now(),
                    last_seen_at: None,
                    current: true,
                    online: true,
                }),
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
        shown_passage: Signal::new(extra.shown_passage),
        device_frames: Signal::new(None),
        agent_commands: Signal::new(Default::default()),
        foreground: Signal::new(true),
        folders: Signal::new(extra.folders),
        folder_sync: Signal::new(extra.folder_sync),
        ask_workspaces: Signal::new(extra.ask_workspaces),
    });
    if !signed_in {
        return rsx! { crate::ui::signin::ConnectScreen {} };
    }
    rsx! { Router::<Route> {} }
}

/// The app's HTML on `route` with `lib` loaded.
pub fn render_route(route: &str, lib: LibraryState) -> String {
    render_with_settings(route, lib, None)
}

/// The app's HTML on `route` with Settings open at `pane`.
pub fn render_with_settings(route: &str, lib: LibraryState, pane: Option<SettingsPane>) -> String {
    render_with(route, lib, pane, Extra::default())
}

/// The app's HTML on `route` with Settings open at `pane` and `extra` set up.
pub fn render_with(
    route: &str,
    lib: LibraryState,
    pane: Option<SettingsPane>,
    extra: Extra,
) -> String {
    let mut dom = VirtualDom::new_with_props(
        Harness,
        HarnessProps {
            route: route.into(),
            lib,
            settings: pane,
            extra,
        },
    );
    dom.rebuild_in_place();
    dioxus_ssr::render(&dom)
}

/// The sign-in screen's HTML for a device whose saved service URL is `service_url`.
pub fn render_connect(service_url: Option<&str>) -> String {
    render_with(
        "/",
        LibraryState::default(),
        None,
        Extra {
            connect: Some(service_url.map(String::from)),
            ..Extra::default()
        },
    )
}
