//! Settings: service, account and devices, AI provider and key, the embedding model for
//! sources, voice, render size, tools on the service Mac, and appearance.

use dioxus::prelude::*;
use margin_shared::{Device, EmbeddingModelState, Event, Settings, SettingsPatch, ToolStatus};

use crate::config::{Appearance, normalize_service_url};
use crate::sources::human_bytes;
use crate::state::use_app;
use crate::ui::common::{
    ConfirmDialog, ErrorNotice, Icon, ago, copy_to_clipboard, date, focus_first, js,
};

/// How long a Copy button says "Copied".
const COPIED_FOR: std::time::Duration = std::time::Duration::from_secs(2);

/// Why the AI provider's base URL can't be saved, if it can't (checked before sending).
fn base_url_problem(input: &str) -> Option<&'static str> {
    let t = input.trim();
    if t.is_empty() {
        return Some("Enter the API's base URL.");
    }
    let Some(rest) = t
        .strip_prefix("https://")
        .or_else(|| t.strip_prefix("http://"))
    else {
        return Some("The base URL must start with https:// or http://.");
    };
    if rest.is_empty() || rest.contains(char::is_whitespace) {
        return Some("That doesn't look like a web address.");
    }
    None
}

/// The label of a Save button: "Saving…" with a spinner while busy.
#[component]
fn SaveLabel(busy: bool, #[props(default = "Save".to_string())] label: String) -> Element {
    rsx! {
        if busy {
            span { class: "spinner" }
            "Saving…"
        } else {
            "{label}"
        }
    }
}
use crate::ui::sources::{Progress, download_embedding, refresh_embedding};

/// The panes of the Settings dialog, in sidebar order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SettingsPane {
    General,
    Account,
    Service,
    Ai,
    Sources,
    Video,
    Tools,
}

impl SettingsPane {
    pub const ALL: [SettingsPane; 7] = [
        SettingsPane::General,
        SettingsPane::Account,
        SettingsPane::Service,
        SettingsPane::Ai,
        SettingsPane::Sources,
        SettingsPane::Video,
        SettingsPane::Tools,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SettingsPane::General => "General",
            SettingsPane::Account => "Account",
            SettingsPane::Service => "Service",
            SettingsPane::Ai => "AI",
            SettingsPane::Sources => "Sources",
            SettingsPane::Video => "Voice & video",
            SettingsPane::Tools => "Tools",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            SettingsPane::General => "half",
            SettingsPane::Account => "user",
            SettingsPane::Service => "server",
            SettingsPane::Ai => "spark",
            SettingsPane::Sources => "clip",
            SettingsPane::Video => "video",
            SettingsPane::Tools => "wrench",
        }
    }
}

/// Settings, as a dialog over the app: panes on the left, the open pane on the right.
/// Open it by setting `AppCtx::settings_open`; Esc or a click outside closes it.
#[component]
pub fn SettingsDialog() -> Element {
    let mut app = use_app();
    let pane = app.settings_open.read().unwrap_or(SettingsPane::General);
    // Phones show the pane list or the open pane (CSS, `.list`); it opens on the pane asked
    // for, and its back button goes to the list.
    let mut show_list = use_signal(|| false);

    // Load settings into the store (SettingsChanged events keep them current).
    let mut settings_error = use_signal(|| None::<String>);
    let mut settings_load = use_resource(move || async move {
        let Some(api) = app.api() else { return };
        match api.settings().await {
            Ok(settings) => {
                settings_error.set(None);
                app.lib.write().apply(Event::SettingsChanged { settings });
            }
            Err(e) => settings_error.set(Some(app.failure(&e))),
        }
    });
    let settings = app.lib.read().settings.clone();
    // Panes backed by the service's settings: their content, or the load state.
    let service_settings =
        |body: fn(Settings) -> Element| match (settings.clone(), settings_error.read().clone()) {
            (Some(s), _) => body(s),
            (None, Some(err)) => rsx! {
                ErrorNotice { message: err, on_retry: move |_| settings_load.restart() }
            },
            (None, None) => rsx! {
                div { class: "skel", style: "width: 60%; margin-top: 20px" }
                div { class: "skel", style: "width: 40%" }
            },
        };

    // Focus the dialog so Esc reaches it.
    use_effect(|| {
        let _ = js("document.getElementById('settings-dialog')?.focus();");
    });

    rsx! {
        div {
            class: "modal-scrim center full",
            onclick: move |_| app.settings_open.set(None),
            div {
                id: "settings-dialog",
                class: if *show_list.read() { "modal settings-modal list" } else { "modal settings-modal" },
                role: "dialog",
                "aria-modal": "true",
                "aria-label": "Settings",
                tabindex: "-1",
                onclick: move |e| e.stop_propagation(),
                onkeydown: move |e: KeyboardEvent| {
                    if e.key() == Key::Escape {
                        app.settings_open.set(None);
                    }
                },
                nav { class: "set-nav", "aria-label": "Settings",
                    div { class: "set-nav-head",
                        h2 { "Settings" }
                        button {
                            class: "icon-btn",
                            "aria-label": "Close settings",
                            onclick: move |_| app.settings_open.set(None),
                            Icon { name: "x" }
                        }
                    }
                    for p in SettingsPane::ALL {
                        button {
                            key: "{p:?}",
                            class: "row",
                            "aria-current": (p == pane).to_string(),
                            onclick: move |_| {
                                app.settings_open.set(Some(p));
                                show_list.set(false);
                            },
                            Icon { name: p.icon() }
                            span { class: "grow", {p.label()} }
                            Icon { name: "forward", class: "set-chev" }
                        }
                    }
                }
                div { class: "set-body",
                    div { class: "set-head",
                        button {
                            class: "icon-btn back-btn",
                            "aria-label": "All settings",
                            onclick: move |_| show_list.set(true),
                            Icon { name: "back" }
                        }
                        h1 { {pane.label()} }
                        button {
                            class: "icon-btn",
                            "aria-label": "Close settings",
                            title: "Close (Esc)",
                            onclick: move |_| app.settings_open.set(None),
                            Icon { name: "x" }
                        }
                    }
                    div { class: "settings", key: "{pane:?}",
                        match pane {
                            SettingsPane::General => rsx! { AppearanceSection {} },
                            SettingsPane::Account => rsx! { AccountSection {} },
                            SettingsPane::Service => rsx! { ServiceSection {} },
                            SettingsPane::Ai => service_settings(|s| rsx! { AiSection { settings: s } }),
                            SettingsPane::Sources => rsx! { SourcesSection {} },
                            SettingsPane::Video => service_settings(|s| rsx! {
                                VoiceSection { settings: s.clone() }
                                RenderSection { settings: s }
                            }),
                            SettingsPane::Tools => rsx! { ToolsSection {} },
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn ServiceSection() -> Element {
    let app = use_app();
    let mut editing = use_signal(|| false);
    let mut value = use_signal(String::new);
    let mut error = use_signal(|| None::<String>);
    let mut confirm = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let current = app.service_url();

    rsx! {
        section { class: "set-sec",
            h2 { "Service" }
            p { class: "help", "The Margin service on your Mac that keeps your library." }
            if *editing.read() {
                form {
                    style: "display: grid; gap: 14px",
                    onsubmit: {
                        let current = current.clone();
                        move |e: FormEvent| {
                            e.prevent_default();
                            match normalize_service_url(&value.peek()) {
                                Ok(url) if url == current => editing.set(false),
                                Ok(url) => {
                                    error.set(None);
                                    confirm.set(Some(url));
                                }
                                Err(e) => {
                                    error.set(Some(e));
                                    focus_first("#svc-url");
                                }
                            }
                        }
                    },
                    div { class: "field",
                        label { r#for: "svc-url", "Service address" }
                        input {
                            id: "svc-url",
                            class: "input",
                            r#type: "url",
                            "aria-invalid": error.read().is_some().to_string(),
                            "aria-describedby": error.read().is_some().then_some("svc-url-err"),
                            initial_value: "{value}",
                            onmounted: move |e| async move { let _ = e.set_focus(true).await; },
                            oninput: move |e| {
                                error.set(None);
                                value.set(e.value());
                            },
                        }
                        if let Some(err) = error.read().clone() {
                            span { id: "svc-url-err", class: "field-error", role: "alert", "{err}" }
                        }
                    }
                    div { class: "row-actions",
                        button { r#type: "submit", class: "btn btn-primary btn-sm", "Change service" }
                        button { r#type: "button", class: "btn btn-ghost btn-sm", onclick: move |_| editing.set(false), "Cancel" }
                    }
                }
            } else {
                div { class: "kv",
                    span { "Address" }
                    span { class: "mono", "{current}" }
                }
                div { class: "row-actions",
                    button {
                        class: "btn btn-secondary btn-sm",
                        onclick: {
                            let current = current.clone();
                            move |_| {
                                value.set(current.clone());
                                error.set(None);
                                editing.set(true);
                            }
                        },
                        "Change…"
                    }
                }
            }
        }
        if let Some(url) = confirm.read().clone() {
            ConfirmDialog {
                title: "Switch to another service?",
                message: format!("This device will sign out of {current} and connect to {url}. You'll sign in again with GitHub."),
                confirm_label: "Sign out and switch",
                danger: false,
                busy: *busy.read(),
                on_cancel: move |_| confirm.set(None),
                on_confirm: {
                    let url = url.clone();
                    move |_| {
                        let url = url.clone();
                        async move {
                            busy.set(true);
                            app.sign_out().await;
                            if let Err(e) = app.set_config(|c| c.service_url = Some(url)) {
                                error.set(Some(e));
                            }
                            busy.set(false);
                        }
                    }
                },
            }
        }
    }
}

#[component]
fn AccountSection() -> Element {
    let app = use_app();
    let mut devices = use_resource(move || async move {
        let api = app.api().ok_or_else(|| "Not signed in.".to_string())?;
        api.devices().await.map_err(|e| app.failure(&e))
    });
    let mut confirm_revoke = use_signal(|| None::<Device>);
    let mut confirm_signout = use_signal(|| false);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);

    let me = app.session.read().as_ref().and_then(|s| s.me.clone());
    let list = devices.read().clone();

    rsx! {
        section { class: "set-sec",
            h2 { "Account" }
            div { class: "kv",
                span { "GitHub" }
                span { {me.as_ref().map(|m| m.github_login.clone()).unwrap_or_else(|| "Loading…".into())} }
                span { "This device" }
                span { {me.as_ref().map(|m| m.device.name.clone()).unwrap_or_else(|| "Loading…".into())} }
            }
            div { class: "row-actions",
                button { class: "btn btn-secondary btn-sm", onclick: move |_| confirm_signout.set(true), "Sign out" }
            }
        }
        section { class: "set-sec",
            h2 { "Devices" }
            p { class: "help", "Every signed-in device has its own key. Revoke one to sign it out." }
            if let Some(err) = error.read().clone() {
                ErrorNotice { message: err }
            }
            match list {
                None => rsx! { div { class: "skel", style: "width: 50%" } },
                Some(Err(err)) => rsx! { ErrorNotice { message: err, on_retry: move |_| devices.restart() } },
                Some(Ok(list)) => rsx! {
                    div {
                        for d in list {
                            div { key: "{d.id}", class: "device",
                                div { class: "grow",
                                    "{d.name}"
                                    if d.current { span { class: "badge", style: "margin-left: 8px", "This device" } }
                                    small {
                                        "Added {date(&d.created_at)}"
                                        if let Some(seen) = d.last_seen_at { " · last seen {ago(&seen)}" }
                                    }
                                }
                                if d.current {
                                    button { class: "btn btn-ghost btn-sm", onclick: move |_| confirm_signout.set(true), "Sign out" }
                                } else {
                                    button {
                                        class: "btn btn-secondary btn-sm",
                                        onclick: {
                                            let d = d.clone();
                                            move |_| confirm_revoke.set(Some(d.clone()))
                                        },
                                        "Revoke"
                                    }
                                }
                            }
                        }
                    }
                },
            }
        }
        if let Some(device) = confirm_revoke.read().clone() {
            ConfirmDialog {
                title: "Revoke “{device.name}”?",
                message: "That device will be signed out and will need to sign in with GitHub again.",
                confirm_label: "Revoke",
                busy: *busy.read(),
                on_cancel: move |_| confirm_revoke.set(None),
                on_confirm: {
                    let id = device.id.clone();
                    move |_| {
                        let id = id.clone();
                        async move {
                            let Some(api) = app.api() else { return };
                            busy.set(true);
                            match api.revoke_device(&id).await {
                                Ok(()) => {
                                    error.set(None);
                                    devices.restart();
                                }
                                Err(e) => error.set(Some(app.failure(&e))),
                            }
                            busy.set(false);
                            confirm_revoke.set(None);
                        }
                    }
                },
            }
        }
        if *confirm_signout.read() {
            ConfirmDialog {
                title: "Sign out of Margin?",
                message: "This device's key is revoked and removed from the keychain. Your library stays on the service.",
                confirm_label: "Sign out",
                busy: *busy.read(),
                on_cancel: move |_| confirm_signout.set(false),
                on_confirm: move |_| async move {
                    busy.set(true);
                    app.sign_out().await;
                },
            }
        }
    }
}

/// PATCH /api/settings and store the result.
async fn save_settings(
    app: crate::state::AppCtx,
    patch: SettingsPatch,
    mut error: Signal<Option<String>>,
    mut busy: Signal<bool>,
) -> bool {
    let Some(api) = app.api() else { return false };
    busy.set(true);
    let ok = match api.patch_settings(&patch).await {
        Ok(settings) => {
            let mut lib = app.lib;
            lib.write().apply(Event::SettingsChanged { settings });
            error.set(None);
            true
        }
        Err(e) => {
            error.set(Some(app.failure(&e)));
            false
        }
    };
    busy.set(false);
    ok
}

#[component]
fn AiSection(settings: Settings) -> Element {
    let app = use_app();
    let mut base = use_signal(|| settings.openai_base_url.clone());
    let mut model = use_signal(|| settings.openai_model.clone());
    let mut key = use_signal(String::new);
    // Bumped when the key field is cleared by code, to remount it empty.
    let mut key_rev = use_signal(|| 0u64);
    let mut replacing = use_signal(|| false);
    let mut confirm_remove = use_signal(|| false);
    let error = use_signal(|| None::<String>);
    let key_error = use_signal(|| None::<String>);
    let busy = use_signal(|| false);
    let key_busy = use_signal(|| false);
    let mut saved_note = use_signal(|| false);
    let mut base_error = use_signal(|| None::<&'static str>);
    let mut model_error = use_signal(|| None::<&'static str>);

    let dirty = *base.read() != settings.openai_base_url || *model.read() != settings.openai_model;
    let show_key_input = !settings.has_api_key || *replacing.read();

    rsx! {
        section { class: "set-sec",
            h2 { "AI provider" }
            p { class: "help", "Margin uses an OpenAI-compatible Responses API for outlines, lessons and video scenes." }
            form {
                style: "display: grid; gap: 14px",
                onsubmit: {
                    let settings = settings.clone();
                    move |e: FormEvent| {
                        e.prevent_default();
                        let settings = settings.clone();
                        async move {
                            if *busy.peek() {
                                return;
                            }
                            let b = base.peek().trim().to_string();
                            let m = model.peek().trim().to_string();
                            if b == settings.openai_base_url && m == settings.openai_model {
                                return;
                            }
                            // Checked here, each under its own field, before anything is sent.
                            let b_problem = base_url_problem(&b);
                            let m_problem = m.is_empty().then_some("Enter a model name.");
                            base_error.set(b_problem);
                            model_error.set(m_problem);
                            if b_problem.is_some() {
                                focus_first("#ai-base");
                                return;
                            }
                            if m_problem.is_some() {
                                focus_first("#ai-model");
                                return;
                            }
                            let patch = SettingsPatch {
                                openai_base_url: (b != settings.openai_base_url).then_some(b),
                                openai_model: (m != settings.openai_model).then_some(m),
                                ..Default::default()
                            };
                            if save_settings(app, patch, error, busy).await {
                                saved_note.set(true);
                            }
                        }
                    }
                },
                div { class: "grid2",
                    div { class: "field",
                        label { r#for: "ai-base", "Base URL" }
                        input {
                            id: "ai-base",
                            class: "input",
                            r#type: "url",
                            "aria-invalid": base_error.read().is_some().to_string(),
                            "aria-describedby": base_error.read().is_some().then_some("ai-base-err"),
                            initial_value: "{base}",
                            oninput: move |e| { saved_note.set(false); base_error.set(None); base.set(e.value()) },
                        }
                        if let Some(err) = *base_error.read() {
                            span { id: "ai-base-err", class: "field-error", "{err}" }
                        }
                    }
                    div { class: "field",
                        label { r#for: "ai-model", "Model" }
                        input {
                            id: "ai-model",
                            class: "input",
                            "aria-invalid": model_error.read().is_some().to_string(),
                            "aria-describedby": model_error.read().is_some().then_some("ai-model-err"),
                            initial_value: "{model}",
                            oninput: move |e| { saved_note.set(false); model_error.set(None); model.set(e.value()) },
                        }
                        if let Some(err) = *model_error.read() {
                            span { id: "ai-model-err", class: "field-error", "{err}" }
                        }
                    }
                }
                if let Some(err) = error.read().clone() {
                    span { class: "save-state error", role: "alert", "{err}" }
                }
                div { class: "row-actions",
                    button {
                        r#type: "submit",
                        class: "btn btn-primary btn-sm",
                        disabled: !dirty || *busy.read(),
                        SaveLabel { busy: *busy.read() }
                    }
                    if *saved_note.read() && !dirty {
                        span { class: "save-state", role: "status", "Saved" }
                    }
                }
            }
            div { class: "field",
                label { r#for: "ai-key", "API key" }
                if show_key_input {
                    form {
                        class: "row-actions",
                        onsubmit: move |e: FormEvent| async move {
                            e.prevent_default();
                            let k = key.peek().trim().to_string();
                            if k.is_empty() || *key_busy.peek() {
                                return;
                            }
                            let patch = SettingsPatch { openai_api_key: Some(k), ..Default::default() };
                            if save_settings(app, patch, key_error, key_busy).await {
                                key.set(String::new());
                                key_rev += 1;
                                replacing.set(false);
                            }
                        },
                        for rev in std::iter::once(key_rev()) {
                        input {
                            key: "{rev}",
                            id: "ai-key",
                            class: "input",
                            style: "flex: 1; min-width: 220px",
                            r#type: "password",
                            autocomplete: "off",
                            placeholder: "sk-…",
                            initial_value: "{key}",
                            oninput: move |e| key.set(e.value()),
                        }
                        }
                        button {
                            r#type: "submit",
                            class: "btn btn-primary btn-sm",
                            disabled: key.read().trim().is_empty() || *key_busy.read(),
                            SaveLabel { busy: *key_busy.read(), label: "Save key" }
                        }
                        if settings.has_api_key {
                            button { r#type: "button", class: "btn btn-ghost btn-sm", onclick: move |_| { key.set(String::new()); key_rev += 1; replacing.set(false) }, "Cancel" }
                        }
                    }
                    p { class: "faint", style: "margin: 0; font-size: 13px", "Sent once to the service and stored in the Mac's Keychain. It is never shown again." }
                } else {
                    div { class: "key-saved",
                        Icon { name: "check" }
                        span { class: "grow", style: "white-space: normal", "Saved in the Mac's Keychain" }
                        button { class: "btn btn-secondary btn-sm", onclick: move |_| replacing.set(true), "Replace" }
                        button { class: "btn btn-ghost btn-sm", onclick: move |_| confirm_remove.set(true), "Remove" }
                    }
                }
                if let Some(err) = key_error.read().clone() {
                    span { class: "save-state error", "{err}" }
                }
            }
        }
        if *confirm_remove.read() {
            ConfirmDialog {
                title: "Remove the API key?",
                message: "The key is deleted from the Mac's Keychain. Asking and generating stop working until you add a key again.",
                confirm_label: "Remove key",
                busy: *key_busy.read(),
                on_cancel: move |_| confirm_remove.set(false),
                on_confirm: move |_| async move {
                    let patch = SettingsPatch { openai_api_key: Some(String::new()), ..Default::default() };
                    save_settings(app, patch, key_error, key_busy).await;
                    confirm_remove.set(false);
                },
            }
        }
    }
}

/// The embedding model sources are read with: its state, download progress, and removal.
#[component]
fn SourcesSection() -> Element {
    let app = use_app();
    let mut load = use_resource(move || async move { refresh_embedding(app).await });
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let mut confirm_remove = use_signal(|| false);
    let model = app.lib.read().embedding.clone();
    let download = move || async move {
        busy.set(true);
        error.set(download_embedding(app).await.err());
        busy.set(false);
    };

    rsx! {
        section { class: "set-sec",
            h2 { "Embedding model" }
            p { class: "help", "Runs on the service Mac to find the passages of your sources that outlines and lessons draw on." }
            match (model, load.read().clone()) {
                (Some(model), _) => rsx! {
                    div { class: "kv",
                        span { "Model" }
                        span { class: "mono", "{model.repo}" }
                        span { "Status" }
                        span {
                            match &model.state {
                                EmbeddingModelState::NotDownloaded { .. } => "Not downloaded".to_string(),
                                EmbeddingModelState::Downloading { .. } => "Downloading…".to_string(),
                                EmbeddingModelState::Ready { bytes } => format!("Ready · {} on disk", human_bytes(*bytes)),
                                EmbeddingModelState::Failed { .. } => "Download failed".to_string(),
                            }
                        }
                    }
                    match model.state {
                        EmbeddingModelState::NotDownloaded { download_bytes } => rsx! {
                            div { class: "row-actions",
                                button {
                                    class: "btn btn-primary btn-sm",
                                    disabled: *busy.read(),
                                    onclick: move |_| download(),
                                    if *busy.read() { span { class: "spinner", style: "border-color: transparent; border-top-color: currentColor" } }
                                    "Download · {human_bytes(download_bytes)}"
                                }
                                span { class: "faint", style: "font-size: 13px", "Downloaded once from Hugging Face. Until then, sources wait in the queue." }
                            }
                        },
                        EmbeddingModelState::Downloading { received, total } => rsx! {
                            Progress { received, total }
                        },
                        EmbeddingModelState::Ready { .. } => rsx! {
                            div { class: "row-actions",
                                button { class: "btn btn-secondary btn-sm", onclick: move |_| confirm_remove.set(true), "Remove…" }
                            }
                        },
                        EmbeddingModelState::Failed { error: message } => rsx! {
                            ErrorNotice {
                                message: "The download failed. {message}",
                                on_retry: move |_| { spawn(download()); },
                            }
                        },
                    }
                },
                (None, Some(Err(err))) => rsx! {
                    ErrorNotice { message: err, on_retry: move |_| load.restart() }
                },
                (None, _) => rsx! {
                    div { class: "skel", style: "width: 50%" }
                },
            }
            if let Some(err) = error.read().clone() {
                span { class: "save-state error", "{err}" }
            }
        }
        if *confirm_remove.read() {
            ConfirmDialog {
                title: "Remove the embedding model?",
                message: "Its files are deleted from the service Mac. New sources wait in the queue until you download it again.",
                confirm_label: "Remove model",
                busy: *busy.read(),
                on_cancel: move |_| confirm_remove.set(false),
                on_confirm: move |_| async move {
                    let Some(api) = app.api() else { return };
                    busy.set(true);
                    match api.delete_embedding().await {
                        Ok(model) => {
                            let mut lib = app.lib;
                            lib.write().apply(Event::EmbeddingModelChanged { model });
                            error.set(None);
                        }
                        Err(e) => error.set(Some(app.failure(&e))),
                    }
                    busy.set(false);
                    confirm_remove.set(false);
                },
            }
        }
    }
}

#[component]
fn VoiceSection(settings: Settings) -> Element {
    let app = use_app();
    let mut voice = use_signal(|| settings.voice.clone().unwrap_or_default());
    let error = use_signal(|| None::<String>);
    let busy = use_signal(|| false);
    let mut saved_note = use_signal(|| false);
    let current = settings.voice.clone().unwrap_or_default();
    let dirty = voice.read().trim() != current;

    rsx! {
        section { class: "set-sec",
            h2 { "Voice" }
            p { class: "help", "The PocketTTS voice that narrates videos. Leave empty for the default voice." }
            form {
                class: "row-actions",
                onsubmit: move |e: FormEvent| async move {
                    e.prevent_default();
                    if *busy.peek() {
                        return;
                    }
                    let v = voice.peek().trim().to_string();
                    let patch = SettingsPatch {
                        voice: Some(if v.is_empty() { None } else { Some(v) }),
                        ..Default::default()
                    };
                    if save_settings(app, patch, error, busy).await {
                        saved_note.set(true);
                    }
                },
                input {
                    class: "input",
                    style: "flex: 1; min-width: 220px",
                    "aria-label": "Voice name",
                    placeholder: "Default voice",
                    initial_value: "{voice}",
                    oninput: move |e| { saved_note.set(false); voice.set(e.value()) },
                }
                button {
                    r#type: "submit",
                    class: "btn btn-primary btn-sm",
                    disabled: !dirty || *busy.read(),
                    SaveLabel { busy: *busy.read() }
                }
                if *saved_note.read() && !dirty {
                    span { class: "save-state", role: "status", "Saved" }
                }
            }
            if let Some(err) = error.read().clone() {
                span { class: "save-state error", role: "alert", "{err}" }
            }
        }
    }
}

const SIZES: &[(u32, u32, &str)] = &[
    (1280, 720, "1280 × 720 (720p)"),
    (1920, 1080, "1920 × 1080 (1080p)"),
    (2560, 1440, "2560 × 1440 (1440p)"),
    (3840, 2160, "3840 × 2160 (4K)"),
    (1080, 1920, "1080 × 1920 (vertical)"),
];
const FPS: &[u32] = &[24, 30, 60];

#[component]
fn RenderSection(settings: Settings) -> Element {
    let app = use_app();
    let mut size = use_signal(|| (settings.render_width, settings.render_height));
    let mut fps = use_signal(|| settings.render_fps);
    let error = use_signal(|| None::<String>);
    let busy = use_signal(|| false);
    let mut saved_note = use_signal(|| false);
    let dirty = *size.read() != (settings.render_width, settings.render_height)
        || *fps.read() != settings.render_fps;
    let (w, h) = *size.read();
    let known = SIZES.iter().any(|(sw, sh, _)| (*sw, *sh) == (w, h));
    let f = *fps.read();
    let known_fps = FPS.contains(&f);

    rsx! {
        section { class: "set-sec",
            h2 { "Video render" }
            div { class: "grid2",
                div { class: "field",
                    label { r#for: "render-size", "Size" }
                    select {
                        id: "render-size",
                        class: "select-input",
                        onchange: move |e| {
                            if let Some((a, b)) = e.value().split_once('x')
                                && let (Ok(a), Ok(b)) = (a.parse(), b.parse())
                            {
                                saved_note.set(false);
                                size.set((a, b));
                            }
                        },
                        if !known {
                            option { value: "{w}x{h}", selected: true, "{w} × {h}" }
                        }
                        for (sw, sh, label) in SIZES.iter().copied() {
                            option { key: "{sw}x{sh}", value: "{sw}x{sh}", selected: (sw, sh) == (w, h), "{label}" }
                        }
                    }
                }
                div { class: "field",
                    label { r#for: "render-fps", "Frame rate" }
                    select {
                        id: "render-fps",
                        class: "select-input",
                        onchange: move |e| {
                            if let Ok(v) = e.value().parse() {
                                saved_note.set(false);
                                fps.set(v);
                            }
                        },
                        if !known_fps {
                            option { value: "{f}", selected: true, "{f} fps" }
                        }
                        for v in FPS.iter().copied() {
                            option { key: "{v}", value: "{v}", selected: v == f, "{v} fps" }
                        }
                    }
                }
            }
            if let Some(err) = error.read().clone() {
                span { class: "save-state error", role: "alert", "{err}" }
            }
            div { class: "row-actions",
                button {
                    class: "btn btn-primary btn-sm",
                    disabled: !dirty || *busy.read(),
                    onclick: move |_| async move {
                        if *busy.peek() {
                            return;
                        }
                        let (w, h) = *size.peek();
                        let patch = SettingsPatch {
                            render_width: Some(w),
                            render_height: Some(h),
                            render_fps: Some(*fps.peek()),
                            ..Default::default()
                        };
                        if save_settings(app, patch, error, busy).await {
                            saved_note.set(true);
                        }
                    },
                    SaveLabel { busy: *busy.read() }
                }
                if *saved_note.read() && !dirty {
                    span { class: "save-state", role: "status", "Saved" }
                }
            }
        }
    }
}

#[component]
fn ToolsSection() -> Element {
    let app = use_app();
    let mut tools = use_resource(move || async move {
        let api = app.api().ok_or_else(|| "Not signed in.".to_string())?;
        api.tools().await.map_err(|e| app.failure(&e))
    });
    let list = tools.read().clone();

    rsx! {
        section { class: "set-sec",
            div { style: "display: flex; align-items: center; gap: 10px",
                h2 { style: "margin: 0; font-size: 17px; font-weight: 500; flex: 1", "Tools on the service Mac" }
                button {
                    class: "btn btn-ghost btn-sm",
                    onclick: move |_| tools.restart(),
                    Icon { name: "refresh" }
                    "Check again"
                }
            }
            p { class: "help", "Videos need ManimGX and the FluidAudio CLI on the Mac that runs the service. Margin never installs them for you." }
            match list {
                None => rsx! { div { class: "skel", style: "width: 50%" } },
                Some(Err(err)) => rsx! { ErrorNotice { message: err, on_retry: move |_| tools.restart() } },
                Some(Ok(list)) => rsx! {
                    div {
                        for tool in list {
                            ToolRow { key: "{tool.name}", tool: tool.clone() }
                        }
                    }
                },
            }
        }
    }
}

#[component]
fn ToolRow(tool: ToolStatus) -> Element {
    let mut copied = use_signal(|| None::<Result<(), String>>);
    // Bumped per copy, so only the latest one's timer resets the label.
    let mut copies = use_signal(|| 0u64);
    let commands = tool.install_commands.join("\n");
    rsx! {
        div { class: "tool",
            div { class: "tool-h",
                span { class: if tool.found { "dot ok" } else { "dot bad" } }
                b { "{tool.name}" }
                span { class: "tool-detail muted",
                    if tool.found {
                        "Found"
                        if let Some(v) = &tool.version { " · {v}" }
                        if let Some(p) = &tool.path { " · {p}" }
                    } else {
                        "Not found"
                    }
                }
            }
            if !tool.found && !tool.install_commands.is_empty() {
                span { class: "muted", style: "font-size: 13px", "Install it on the service Mac with:" }
                div { class: "copy-wrap",
                    pre { class: "code", style: "padding-right: 90px", code { "{commands}" } }
                    button {
                        class: "btn btn-secondary btn-sm copy",
                        onclick: {
                            let commands = commands.clone();
                            move |_| {
                                let commands = commands.clone();
                                async move {
                                    let n = *copies.peek() + 1;
                                    copies.set(n);
                                    copied.set(None);
                                    let result = copy_to_clipboard(commands).await;
                                    let ok = result.is_ok();
                                    copied.set(Some(result));
                                    if ok {
                                        // Back to "Copy" after a moment, so a second copy shows too.
                                        tokio::time::sleep(COPIED_FOR).await;
                                        if *copies.peek() == n {
                                            copied.set(None);
                                        }
                                    }
                                }
                            }
                        },
                        Icon { name: "copy" }
                        match copied.read().clone() {
                            Some(Ok(())) => "Copied",
                            _ => "Copy",
                        }
                    }
                }
                // Announced to screen readers; the button label says the same.
                span { class: "sr-only", role: "status",
                    if matches!(*copied.read(), Some(Ok(()))) { "Copied to the clipboard" }
                }
                if let Some(Err(err)) = copied.read().clone() {
                    span { class: "save-state error", role: "alert", "{err}" }
                }
            }
        }
    }
}

#[component]
fn AppearanceSection() -> Element {
    let app = use_app();
    let current = app.config.read().appearance;
    let mut error = use_signal(|| None::<String>);
    rsx! {
        section { class: "set-sec",
            h2 { "Appearance" }
            div { class: "seg", role: "radiogroup", "aria-label": "Appearance",
                for a in [Appearance::System, Appearance::Light, Appearance::Dark] {
                    button {
                        key: "{a.label()}",
                        role: "radio",
                        "aria-checked": (a == current).to_string(),
                        // One tab stop; the arrow keys move between options (margin.js).
                        tabindex: if a == current { "0" } else { "-1" },
                        onclick: move |_| {
                            if let Err(e) = app.set_config(|c| c.appearance = a) {
                                error.set(Some(format!("Changed for now, but couldn't be saved: {e}")));
                            } else {
                                error.set(None);
                            }
                        },
                        "{a.label()}"
                    }
                }
            }
            if let Some(err) = error.read().clone() {
                span { class: "save-state error", "{err}" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_problem_checks_the_address() {
        assert_eq!(base_url_problem("https://api.openai.com/v1"), None);
        assert_eq!(base_url_problem(" http://localhost:1234/v1 "), None);
        assert_eq!(base_url_problem("  "), Some("Enter the API's base URL."));
        assert_eq!(
            base_url_problem("api.openai.com"),
            Some("The base URL must start with https:// or http://.")
        );
        assert_eq!(
            base_url_problem("https://"),
            Some("That doesn't look like a web address.")
        );
        assert_eq!(
            base_url_problem("https://my host"),
            Some("That doesn't look like a web address.")
        );
    }
}
