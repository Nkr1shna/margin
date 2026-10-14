//! Render app screens to static HTML with the fake library, for visual checks without a
//! window (e.g. snapshotting in an offscreen webview on CI or a headless machine):
//!
//! ```sh
//! cargo run -p margin-app --example render_screens -- /lesson/l1 out.html [light|dark] [WIDTHpx]
//! cargo run -p margin-app --example render_screens -- connect out.html
//! ```
//!
//! Effects, futures and JS evals don't run in server-side rendering, so live parts
//! (selection, settings fetched over HTTP) show their initial state. Test tooling only.

#![allow(dead_code, unused_imports)]

#[path = "../src/api.rs"]
mod api;
#[path = "../src/config.rs"]
mod config;
#[path = "../src/fake.rs"]
mod fake;
#[path = "../src/markdown.rs"]
mod markdown;
#[path = "../src/routes.rs"]
mod routes;
#[path = "../src/secrets.rs"]
mod secrets;
#[path = "../src/state.rs"]
mod state;
#[path = "../src/store.rs"]
mod store;
#[path = "../src/ui/mod.rs"]
mod ui;

use std::collections::HashSet;
use std::path::Path;
use std::rc::Rc;

use chrono::Utc;
use dioxus::history::{History, MemoryHistory};
use dioxus::prelude::*;
use margin_shared::{Device, Me, Settings};

use crate::api::Api;
use crate::state::{AppCtx, LiveStatus, LoadStatus, Session};
use crate::store::LibraryState;

#[derive(Props, Clone, PartialEq)]
struct HarnessProps {
    route: String,
}

#[allow(non_snake_case)]
fn Harness(props: HarnessProps) -> Element {
    let route = props.route.clone();
    use_context_provider(|| Rc::new(MemoryHistory::with_initial_path(route)) as Rc<dyn History>);
    let connect = props.route == "connect";
    use_context_provider(move || {
        let mut lib = LibraryState::from_library(fake::sample_library());
        lib.settings = Some(Settings {
            openai_base_url: "https://api.openai.com/v1".into(),
            openai_model: "gpt-5".into(),
            has_api_key: true,
            voice: None,
            render_width: 1920,
            render_height: 1080,
            render_fps: 30,
        });
        let session = (!connect).then(|| Session {
            api: Api::with_token("http://127.0.0.1:9", "x"),
            me: Some(Me {
                github_login: "Nkr1shna".into(),
                device: Device {
                    id: "d1".into(),
                    name: "Krishna's MacBook".into(),
                    created_at: Utc::now(),
                    last_seen_at: Some(Utc::now()),
                    current: true,
                },
            }),
        });
        AppCtx {
            config: Signal::new(config::LocalConfig {
                service_url: Some("https://margin.krishnanelloore.com".into()),
                appearance: config::Appearance::System,
                sidebar_hidden: false,
            }),
            session: Signal::new(session),
            signed_out_reason: Signal::new(None),
            lib: Signal::new(lib),
            lib_status: Signal::new(LoadStatus::Loaded),
            reload: Signal::new(0),
            live: Signal::new(LiveStatus::Live),
            search_open: Signal::new(false),
            drawer_open: Signal::new(false),
            collapsed: Signal::new(HashSet::new()),
            ask_proposal: Signal::new(None),
            ask_prompt: Signal::new(String::new()),
            settings_open: Signal::new(None),
        }
    });
    if connect {
        rsx! { ui::signin::ConnectScreen {} }
    } else {
        rsx! { Router::<routes::Route> {} }
    }
}

fn file_url(p: &Path) -> String {
    format!("file://{}", p.display())
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let route = args.next().unwrap_or_else(|| "/".into());
    let out = args.next().unwrap_or_else(|| "screen.html".into());
    let theme = args.next().unwrap_or_else(|| "light".into());

    let mut dom = VirtualDom::new_with_props(Harness, HarnessProps { route });
    dom.rebuild_in_place();
    let body = dioxus_ssr::render(&dom);

    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
    let fonts = assets.join("fonts");
    let html = format!(
        r#"<!doctype html>
<html data-theme="{theme}"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
@font-face {{ font-family: 'Albert Sans'; font-weight: 100 900; src: url('{f1}') format('truetype'); }}
@font-face {{ font-family: 'Albert Sans'; font-style: italic; font-weight: 100 900; src: url('{f2}') format('truetype'); }}
</style>
<link rel="stylesheet" href="{css}">
<link rel="stylesheet" href="{kcss}">
<script src="{kjs}"></script>
<script src="{mjs}"></script>
</head><body><div id="main">{body}</div>
<script>window.addEventListener('load', function () {{ MarginApp.renderMathIn('.md'); }});</script>
</body></html>"#,
        f1 = file_url(&fonts.join("AlbertSans-wght.ttf")),
        f2 = file_url(&fonts.join("AlbertSans-Italic-wght.ttf")),
        css = file_url(&assets.join("main.css")),
        kcss = file_url(&assets.join("katex/katex.min.css")),
        kjs = file_url(&assets.join("katex/katex.min.js")),
        mjs = file_url(&assets.join("margin.js")),
    );
    std::fs::write(&out, html).expect("write html");
    println!("wrote {out}");
}
