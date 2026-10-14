//! First run and sign-in: the service URL, then "Sign in with GitHub"
//! (POST /auth/start → open the browser → POST /auth/poll every 2 s).

use std::time::Duration;

use chrono::Utc;
use dioxus::core::Task;
use dioxus::prelude::*;
use margin_shared::{LoginState, StartLogin};

use crate::api::Api;
use crate::config::{self, normalize_service_url};
use crate::secrets;
use crate::state::{Session, use_app};
use crate::ui::common::{BrandMark, DragRegion, Icon, open_external};

#[derive(Clone, PartialEq, Debug)]
enum Phase {
    Idle,
    Starting,
    Waiting {
        authorize_url: String,
        browser_error: Option<String>,
        poll_error: Option<String>,
    },
    Finishing,
    Failed(String),
    Denied(String),
    Expired,
}

fn device_name() -> String {
    let name = gethostname::gethostname()
        .to_string_lossy()
        .trim()
        .to_string();
    let name = name.strip_suffix(".local").unwrap_or(&name).to_string();
    if name.is_empty() || name == "localhost" {
        if cfg!(target_os = "ios") {
            "iPhone or iPad".into()
        } else if cfg!(target_os = "android") {
            "Android device".into()
        } else {
            "This computer".into()
        }
    } else {
        name
    }
}

#[component]
pub fn ConnectScreen() -> Element {
    let mut app = use_app();
    let mut url = use_signal(|| app.service_url());
    // Bumped when the address is rewritten by code (normalized), to remount the field.
    let mut url_rev = use_signal(|| 0u64);
    let mut phase = use_signal(|| Phase::Idle);
    let mut task = use_signal(|| None::<Task>);

    let mut start = move || {
        if let Some(t) = task.take() {
            t.cancel();
        }
        app.signed_out_reason.set(None);
        let handle = spawn(async move {
            let base = match normalize_service_url(&url.peek()) {
                Ok(base) => base,
                Err(message) => {
                    phase.set(Phase::Failed(message));
                    return;
                }
            };
            if *url.peek() != base {
                url.set(base.clone());
                url_rev += 1;
            }
            phase.set(Phase::Starting);
            let api = Api::anonymous(&base);
            let login = match api.start_login(&device_name()).await {
                Ok(login) => login,
                Err(e) => {
                    phase.set(Phase::Failed(e.message));
                    return;
                }
            };
            let browser_error = open_external(&login.authorize_url).err();
            phase.set(Phase::Waiting {
                authorize_url: login.authorize_url.clone(),
                browser_error,
                poll_error: None,
            });
            match wait_for_approval(&api, &login, phase).await {
                Outcome::Approved(token) => {
                    phase.set(Phase::Finishing);
                    if let Err(message) = secrets::save_token(&base, &token) {
                        phase.set(Phase::Failed(format!(
                            "Signed in, but the token couldn't be stored. {message}"
                        )));
                        return;
                    }
                    if let Err(message) = app.set_config(|c| c.service_url = Some(base.clone())) {
                        phase.set(Phase::Failed(message));
                        return;
                    }
                    let authed = Api::with_token(&base, &token);
                    let me = authed.me().await.ok();
                    app.session.set(Some(Session { api: authed, me }));
                }
                Outcome::Denied(reason) => phase.set(Phase::Denied(reason)),
                Outcome::Expired => phase.set(Phase::Expired),
            }
        });
        task.set(Some(handle));
    };

    let cancel = move |_| {
        if let Some(t) = task.take() {
            t.cancel();
        }
        phase.set(Phase::Idle);
    };

    let current = phase.read().clone();
    let busy = matches!(
        current,
        Phase::Starting | Phase::Waiting { .. } | Phase::Finishing
    );
    let reason = app.signed_out_reason.read().clone();

    rsx! {
        div { class: "connect",
            div { class: "connect-bar", DragRegion {} }
            div { class: "connect-card",
                div { class: "connect-head",
                    BrandMark {}
                    "Margin"
                }
                h1 { "Connect to your Margin service" }
                p { "Margin keeps your library on your own Mac. Enter the service's address, then sign in with GitHub to give this device its own key." }
                if let Some(reason) = reason {
                    div { class: "notice", role: "status",
                        Icon { name: "alert" }
                        div { class: "body", "{reason}" }
                    }
                }
                form {
                    class: "field",
                    onsubmit: move |e| {
                        e.prevent_default();
                        if !busy {
                            start();
                        }
                    },
                    label { r#for: "service-url", "Service address" }
                    for rev in std::iter::once(url_rev()) {
                    input {
                        key: "{rev}",
                        id: "service-url",
                        class: "input",
                        r#type: "url",
                        inputmode: "url",
                        autocomplete: "url",
                        spellcheck: false,
                        disabled: busy,
                        initial_value: "{url}",
                        placeholder: config::DEFAULT_SERVICE_URL,
                        oninput: move |e| url.set(e.value()),
                    }
                    }
                }
                match current {
                    Phase::Idle => rsx! {
                        button { class: "btn btn-primary", onclick: move |_| start(), "Sign in with GitHub" }
                    },
                    Phase::Starting => rsx! {
                        div { class: "waiting", role: "status",
                            span { class: "spinner" }
                            span { class: "grow", "Contacting the service…" }
                            button { class: "btn btn-secondary btn-sm", onclick: cancel, "Cancel" }
                        }
                    },
                    Phase::Waiting { authorize_url, browser_error, poll_error } => rsx! {
                        div { class: "waiting", role: "status",
                            span { class: "spinner" }
                            div { class: "grow", style: "white-space: normal",
                                div { "Waiting for GitHub…" }
                                div { class: "faint", style: "font-size: 13px",
                                    "Finish signing in in your browser. This page continues by itself."
                                }
                            }
                        }
                        if let Some(err) = browser_error {
                            div { class: "notice error", Icon { name: "alert" } div { class: "body", "{err}" } }
                        }
                        if let Some(err) = poll_error {
                            div { class: "notice", Icon { name: "alert" } div { class: "body", "{err} Still trying…" } }
                        }
                        div { class: "row-actions",
                            button {
                                class: "btn btn-secondary btn-sm",
                                onclick: move |_| {
                                    let url = authorize_url.clone();
                                    if let Err(err) = open_external(&url) {
                                        let mut p = phase.write();
                                        if let Phase::Waiting { browser_error, .. } = &mut *p {
                                            *browser_error = Some(err);
                                        }
                                    }
                                },
                                "Open GitHub again"
                            }
                            button { class: "btn btn-ghost btn-sm", onclick: cancel, "Cancel" }
                        }
                    },
                    Phase::Finishing => rsx! {
                        div { class: "waiting", role: "status", span { class: "spinner" } span { "Signing in…" } }
                    },
                    Phase::Failed(message) => rsx! {
                        div { class: "notice error", role: "alert",
                            Icon { name: "alert" }
                            div { class: "body", "{message}" }
                        }
                        button { class: "btn btn-primary", onclick: move |_| start(), "Try again" }
                    },
                    Phase::Denied(reason) => rsx! {
                        div { class: "notice error", role: "alert",
                            Icon { name: "alert" }
                            div { class: "body",
                                b { style: "font-weight: 500", "GitHub sign-in was denied." }
                                span { "{reason}" }
                            }
                        }
                        button { class: "btn btn-primary", onclick: move |_| start(), "Try again" }
                    },
                    Phase::Expired => rsx! {
                        div { class: "notice error", role: "alert",
                            Icon { name: "alert" }
                            div { class: "body", "The sign-in request expired before it was approved. Start again to get a new one." }
                        }
                        button { class: "btn btn-primary", onclick: move |_| start(), "Start again" }
                    },
                }
            }
        }
    }
}

enum Outcome {
    Approved(String),
    Denied(String),
    Expired,
}

async fn wait_for_approval(api: &Api, login: &StartLogin, mut phase: Signal<Phase>) -> Outcome {
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        match api.poll_login(login).await {
            Ok(LoginState::Pending) => {
                if Utc::now() > login.expires_at {
                    return Outcome::Expired;
                }
                set_poll_error(&mut phase, None);
            }
            Ok(LoginState::Approved { token, .. }) => return Outcome::Approved(token),
            Ok(LoginState::Denied { reason }) => return Outcome::Denied(reason),
            Ok(LoginState::Expired) => return Outcome::Expired,
            Err(e) => {
                if Utc::now() > login.expires_at {
                    return Outcome::Expired;
                }
                set_poll_error(&mut phase, Some(e.message));
            }
        }
    }
}

fn set_poll_error(phase: &mut Signal<Phase>, error: Option<String>) {
    let changed =
        matches!(&*phase.peek(), Phase::Waiting { poll_error, .. } if *poll_error != error);
    if changed {
        let mut p = phase.write();
        if let Phase::Waiting { poll_error, .. } = &mut *p {
            *poll_error = error;
        }
    }
}
