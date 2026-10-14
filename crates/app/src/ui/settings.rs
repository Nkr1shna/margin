//! Settings: service, account and devices, AI provider and key, the embedding model for
//! sources, voice, render size, tools on the service Mac, and appearance.

use dioxus::prelude::*;
use margin_shared::{
    Device, EmbeddingModelState, Event, FOLDER_FILE_TYPES_TEXT, Id, ReasoningEffort, SOURCE_KINDS,
    Settings, SettingsPatch, Source, ToolStatus, Workspace, WorkspaceSync,
};

use crate::config::{Appearance, normalize_service_url};
use crate::folders::sync::SyncState;
use crate::sources::{human_bytes, status_text};
use crate::state::{LiveStatus, use_app};
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
use crate::ui::sources::{
    Progress, ReadAgainButton, SourceLine, download_embedding, refresh_embedding,
};

/// The panes of the Settings dialog, in sidebar order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SettingsPane {
    General,
    Account,
    Service,
    Ai,
    Sources,
    Folders,
    Video,
    Tools,
}

impl SettingsPane {
    pub const ALL: [SettingsPane; 8] = [
        SettingsPane::General,
        SettingsPane::Account,
        SettingsPane::Service,
        SettingsPane::Ai,
        SettingsPane::Sources,
        SettingsPane::Folders,
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
            SettingsPane::Folders => "Folders",
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
            SettingsPane::Folders => "folder",
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
                        // A caption, not a heading: the nav is named "Settings" and the
                        // pane's title is the dialog's heading.
                        div { class: "set-nav-title", "aria-hidden": "true", "Settings" }
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
                            SettingsPane::General => rsx! {
                                AppearanceSection {}
                                TextSizeSection {}
                            },
                            SettingsPane::Account => rsx! { AccountSection {} },
                            SettingsPane::Service => rsx! { ServiceSection {} },
                            SettingsPane::Ai => service_settings(|s| rsx! {
                                AiSection { settings: s.clone() }
                                AgentSection { settings: s }
                            }),
                            SettingsPane::Sources => rsx! { SourcesSection {} },
                            SettingsPane::Folders => rsx! { FoldersSection {} },
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
            p { class: "help", "Where your library lives: the Margin service on the service Mac." }
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

/// The devices as Settings lists them: this one first, then the others, newest first.
pub fn devices_in_order(devices: &[Device]) -> Vec<Device> {
    let mut list = devices.to_vec();
    list.sort_by(|a, b| {
        b.current
            .cmp(&a.current)
            .then(b.created_at.cmp(&a.created_at))
            .then(a.id.cmp(&b.id))
    });
    list
}

/// Whether a device is connected now, in words for its row: "Online", or "Offline ·
/// last seen 2 h ago".
pub fn presence_text(device: &Device) -> String {
    match (device.online, device.last_seen_at) {
        (true, _) => "Online".to_string(),
        (false, Some(seen)) => format!("Offline · last seen {}", ago(&seen)),
        (false, None) => "Offline".to_string(),
    }
}

#[component]
fn AccountSection() -> Element {
    let app = use_app();
    // The list comes from the service once; DeviceChanged / DeviceDeleted keep it (and
    // who is online) current from then on.
    let mut devices = use_resource(move || async move {
        let api = app.api().ok_or_else(|| "Not signed in.".to_string())?;
        let list = api.devices().await.map_err(|e| app.failure(&e))?;
        let mut lib = app.lib;
        lib.write().replace_devices(list);
        Ok::<(), String>(())
    });
    let mut confirm_revoke = use_signal(|| None::<Device>);
    let mut confirm_signout = use_signal(|| false);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);

    let me = app.session.read().as_ref().and_then(|s| s.me.clone());
    // Presence comes from live events, and the list is fetched again on reconnecting.
    let connected = *app.live.read() == LiveStatus::Live;
    // This device is the one signed in here (events don't say which is whose).
    let my_id = me.as_ref().map(|m| m.device.id.clone());
    let known: Vec<Device> = app
        .lib
        .read()
        .devices
        .values()
        .map(|d| Device {
            current: d.current || my_id.as_deref() == Some(d.id.as_str()),
            ..d.clone()
        })
        .collect();
    let list = match devices.read().clone() {
        Some(Ok(())) => Some(Ok(devices_in_order(&known))),
        Some(Err(e)) => Some(Err(e)),
        // Known from events already: show them while the list loads.
        None if !known.is_empty() => Some(Ok(devices_in_order(&known))),
        None => None,
    };

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
                button { class: "btn btn-secondary btn-sm", onclick: move |_| confirm_signout.set(true), "Sign out…" }
            }
        }
        section { class: "set-sec",
            h2 { "Devices" }
            p { class: "help", "Every signed-in device has its own key. Revoke one to sign it out. A device is online while Margin runs on it; the agent can only use it then." }
            if !connected {
                p { class: "help", "This device isn't connected to the service, so who is online may be out of date. It updates when the connection is back." }
            }
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
                                span {
                                    class: if d.online { "dot ok" } else { "dot" },
                                    title: if d.online { "Online" } else { "Offline" },
                                    "aria-hidden": "true",
                                }
                                div { class: "grow",
                                    "{d.name}"
                                    if d.current { span { class: "badge", style: "margin-left: 8px", "This device" } }
                                    small {
                                        "{presence_text(&d)} · added {date(&d.created_at)}"
                                    }
                                }
                                if d.current {
                                    button { class: "btn btn-ghost btn-sm", onclick: move |_| confirm_signout.set(true), "Sign out…" }
                                } else {
                                    button {
                                        class: "btn btn-secondary btn-sm",
                                        onclick: {
                                            let d = d.clone();
                                            move |_| confirm_revoke.set(Some(d.clone()))
                                        },
                                        "Revoke…"
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
                                    // DeviceDeleted says the same to every device.
                                    let mut lib = app.lib;
                                    lib.write().apply(Event::DeviceDeleted { id: id.clone() });
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
                message: "This device's key is revoked and removed from this device. Your library stays on the service.",
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
            p { class: "help", "Margin uses an OpenAI-compatible Responses API for chats, outlines, lessons and video scenes." }
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
                    p { class: "faint", style: "margin: 0; font-size: 13px", "Sent once to the service and stored in the service Mac's Keychain. It is never shown again." }
                } else {
                    div { class: "key-saved",
                        Icon { name: "check" }
                        span { class: "grow", style: "white-space: normal", "Saved in the service Mac's Keychain" }
                        button { class: "btn btn-secondary btn-sm", onclick: move |_| replacing.set(true), "Replace" }
                        button { class: "btn btn-secondary btn-sm", onclick: move |_| confirm_remove.set(true), "Remove…" }
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
                message: "The key is deleted from the service Mac's Keychain. Asking and generating stop working until you add a key again.",
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

/// How hard the agent thinks, in words.
pub fn effort_label(effort: ReasoningEffort) -> &'static str {
    match effort {
        // An older build's "minimal" is sent as low.
        ReasoningEffort::Minimal | ReasoningEffort::Low => "Low",
        ReasoningEffort::Medium => "Medium",
        ReasoningEffort::High => "High",
        ReasoningEffort::XHigh => "XHigh",
        ReasoningEffort::Max => "Max",
    }
}

/// The option Settings shows as chosen: one of `ReasoningEffort::ALL` (a "minimal" stored
/// by an older build reads as Low, which is what it is sent as).
pub fn shown_effort(effort: ReasoningEffort) -> ReasoningEffort {
    match effort {
        ReasoningEffort::Minimal => ReasoningEffort::Low,
        e => e,
    }
}

/// The model chats use (when not the one above) and how hard it thinks before answering.
#[component]
fn AgentSection(settings: Settings) -> Element {
    let app = use_app();
    let current = settings.agent_model.clone().unwrap_or_default();
    let mut model = use_signal(|| current.clone());
    let error = use_signal(|| None::<String>);
    let busy = use_signal(|| false);
    let effort_error = use_signal(|| None::<String>);
    let effort_busy = use_signal(|| false);
    let mut saved_note = use_signal(|| false);
    let dirty = model.read().trim() != current;
    let effort = shown_effort(settings.reasoning_effort);

    rsx! {
        section { class: "set-sec",
            h2 { "Chats" }
            p { class: "help", "The model that answers in chats, and how much it reasons first. More reasoning answers harder questions better but takes longer and costs more." }
            form {
                class: "field",
                onsubmit: move |e: FormEvent| async move {
                    e.prevent_default();
                    if *busy.peek() {
                        return;
                    }
                    let m = model.peek().trim().to_string();
                    let patch = SettingsPatch {
                        agent_model: Some((!m.is_empty()).then_some(m)),
                        ..Default::default()
                    };
                    if save_settings(app, patch, error, busy).await {
                        saved_note.set(true);
                    }
                },
                label { r#for: "ai-agent-model", "Model for chats" }
                div { class: "row-actions",
                    input {
                        id: "ai-agent-model",
                        class: "input",
                        style: "flex: 1; min-width: 220px",
                        placeholder: "Same as above ({settings.openai_model})",
                        initial_value: "{model}",
                        oninput: move |e| { saved_note.set(false); model.set(e.value()) },
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
            div { class: "field",
                span { class: "label", id: "ai-effort-label", "Reasoning" }
                div { class: "seg", role: "radiogroup", "aria-labelledby": "ai-effort-label", "aria-busy": effort_busy.read().to_string(),
                    for e in ReasoningEffort::ALL {
                        button {
                            key: "{e.as_str()}",
                            role: "radio",
                            "aria-checked": (e == effort).to_string(),
                            // One tab stop; the arrow keys move between options (margin.js).
                            tabindex: if e == effort { "0" } else { "-1" },
                            onclick: move |_| async move {
                                if e == effort || *effort_busy.peek() {
                                    return;
                                }
                                let patch = SettingsPatch { reasoning_effort: Some(e), ..Default::default() };
                                save_settings(app, patch, effort_error, effort_busy).await;
                            },
                            {effort_label(e)}
                        }
                    }
                }
                if let Some(err) = effort_error.read().clone() {
                    span { class: "save-state error", role: "alert", "{err}" }
                }
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
            p { class: "help",
                "EmbeddingGemma 2 runs on the service Mac. It reads your sources ({SOURCE_KINDS}) and finds the passages, pages, pictures and clips that outlines and lessons draw on."
            }
            match (model, load.read().clone()) {
                (Some(model), _) => rsx! {
                    div { class: "kv",
                        span { "Model" }
                        span { class: "mono", "{model.repo}" }
                        span { "Parts" }
                        span { "Text, image and audio encoders" }
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
                                    if *busy.read() { span { class: "spinner" } }
                                    "Download · {human_bytes(download_bytes)}"
                                }
                                span { class: "faint", style: "font-size: 13px", "All three parts download together, once, from Hugging Face. Until then, sources wait in the queue." }
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

/// What the Folders pane and the menus say about what a folder sync takes.
pub fn folders_help() -> String {
    format!(
        "A folder's {FOLDER_FILE_TYPES_TEXT} are copied to your Margin service and read there (text, pages and pictures), so the agent can search inside them. Other files are left alone; nothing on your device is changed. Files are sent again when they change, and removed from the service when they're deleted or the folder is removed."
    )
}

/// What this platform adds to the help ("macOS may ask once…").
pub fn folders_platform_help() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        Some("macOS may ask once before Margin reads Desktop, Documents or Downloads.")
    } else if cfg!(target_os = "ios") {
        Some(
            "Files that iCloud hasn't downloaded to this device are downloaded as they're sent. Sending happens while Margin is open.",
        )
    } else if cfg!(target_os = "android") {
        Some("Sending happens while Margin is open.")
    } else {
        None
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// "12 files · 1.2 GB on the service".
pub fn folder_size_text(sync: &WorkspaceSync) -> String {
    format!(
        "{} · {} on the service",
        plural(sync.files as u64, "file", "files"),
        human_bytes(sync.bytes)
    )
}

/// The live line of a folder this device holds: what its sync is doing now, else what
/// the service is doing with its files, else when it last synced.
pub fn folder_sync_line(state: Option<&SyncState>, sync: &WorkspaceSync) -> String {
    match state {
        Some(SyncState::Scanning) => return "Scanning…".into(),
        Some(SyncState::Uploading {
            done,
            total,
            bytes_done,
            bytes_total,
        }) => {
            return format!(
                "Sending {} of {total} ({} of {})…",
                (done + 1).min(*total),
                human_bytes(*bytes_done),
                human_bytes(*bytes_total)
            );
        }
        Some(SyncState::Paused) => return "Paused — open Margin to continue".into(),
        Some(SyncState::Failed(message)) => return message.clone(),
        Some(SyncState::Idle { .. }) | None => {}
    }
    if sync.reading > 0 {
        return format!("Reading {}…", plural(sync.reading as u64, "file", "files"));
    }
    let last = match state {
        Some(SyncState::Idle { last }) => Some(*last).max(sync.last_synced_at),
        _ => sync.last_synced_at,
    };
    match last {
        Some(t) => format!("Synced {}", ago(&t)),
        None => "Not synced yet".into(),
    }
}

/// The line of another device's folder: when it last synced, or that it waits for that
/// device.
pub fn other_folder_line(ws: &Workspace, online: bool) -> String {
    let presence = if online { "Online" } else { "Offline" };
    let state = if ws.sync.uploading > 0 && !online {
        "Paused".to_string()
    } else if ws.sync.uploading > 0 {
        format!(
            "Sending {}",
            plural(ws.sync.uploading as u64, "file", "files")
        )
    } else {
        match ws.sync.last_synced_at {
            Some(t) => format!("Synced {}", ago(&t)),
            None => "Not synced yet".into(),
        }
    };
    format!(
        "{} · on {} · {} · {state} · {presence}",
        ws.location,
        ws.device_name,
        plural(ws.sync.files as u64, "file", "files")
    )
}

/// Settings → Folders: the folders this device holds (with their syncs, files and
/// removal) and those of other devices.
#[component]
fn FoldersSection() -> Element {
    let app = use_app();
    // The list again (the store is current from events after), with who is online.
    let mut load = use_resource(move || async move {
        let api = app.api().ok_or_else(|| "Not signed in.".to_string())?;
        let list = api.workspaces().await.map_err(|e| app.failure(&e))?;
        let ids = list.iter().map(|w| w.id.clone()).collect();
        let mut lib = app.lib;
        lib.write().replace_workspaces(list);
        crate::folders::sync::prune_held(ids).await;
        if let Ok(devices) = api.devices().await {
            lib.write().replace_devices(devices);
        }
        Ok::<(), String>(())
    });
    let mut attaching = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);

    let me = app
        .session
        .read()
        .as_ref()
        .and_then(|s| s.me.as_ref().map(|m| m.device.id.clone()));
    let lib = app.lib.read();
    let all: Vec<Workspace> = lib
        .workspaces_in_order(me.as_deref())
        .into_iter()
        .cloned()
        .collect();
    let online: Vec<bool> = all
        .iter()
        .map(|w| {
            Some(w.device_id.as_str()) == me.as_deref()
                || lib.devices.get(&w.device_id).is_some_and(|d| d.online)
        })
        .collect();
    drop(lib);
    // Each folder, and whether its device is online.
    type Listed = Vec<(Workspace, bool)>;
    let (mine, others): (Listed, Listed) = all
        .into_iter()
        .zip(online)
        .partition(|(w, _)| Some(w.device_id.as_str()) == me.as_deref());
    let files: u64 = mine
        .iter()
        .chain(others.iter())
        .map(|(w, _)| w.sync.files as u64)
        .sum();
    let bytes: u64 = mine
        .iter()
        .chain(others.iter())
        .map(|(w, _)| w.sync.bytes)
        .sum();
    let any = !mine.is_empty() || !others.is_empty();
    let loading = load.read().is_none() && !any;
    let load_error = match load.read().as_ref() {
        Some(Err(e)) => Some(e.clone()),
        _ => None,
    };
    let attach = move |_| {
        if *attaching.peek() {
            return;
        }
        spawn(async move {
            attaching.set(true);
            match crate::folders::sync::attach(app).await {
                Ok(_) => error.set(None),
                Err(e) => error.set(Some(e)),
            }
            attaching.set(false);
        });
    };

    rsx! {
        section { class: "set-sec",
            h2 { "This device" }
            p { class: "help", {folders_help()} }
            if let Some(more) = folders_platform_help() {
                p { class: "help", "{more}" }
            }
            if let Some(err) = load_error {
                ErrorNotice { message: err, on_retry: move |_| load.restart() }
            }
            if let Some(err) = error.read().clone() {
                ErrorNotice { message: err }
            }
            if loading {
                div { class: "skel", style: "width: 50%" }
            } else if mine.is_empty() {
                p { class: "faint", "No folder of this device is attached yet." }
            }
            div { role: "list", "aria-label": "Folders on this device",
                for (ws, _) in mine {
                    div { key: "{ws.id}", role: "listitem",
                        HeldFolderRow { ws: ws.clone() }
                    }
                }
            }
            div { class: "row-actions",
                button {
                    class: "btn btn-primary btn-sm",
                    disabled: *attaching.read(),
                    onclick: attach,
                    if *attaching.read() { span { class: "spinner" } } else { Icon { name: "folder" } }
                    "Attach a folder…"
                }
            }
        }
        if !others.is_empty() {
            section { class: "set-sec",
                h2 { "Other devices" }
                div { role: "list", "aria-label": "Folders on other devices",
                    for (ws, online) in others {
                        div { key: "{ws.id}", role: "listitem", class: "device folder-row",
                            span {
                                class: if online { "dot ok" } else { "dot" },
                                title: if online { "Online" } else { "Offline" },
                                "aria-hidden": "true",
                            }
                            Icon { name: "folder" }
                            div { class: "grow",
                                "{ws.name}"
                                small { {other_folder_line(&ws, online)} }
                                small { class: "faint", "Remove it on {ws.device_name}." }
                            }
                        }
                    }
                }
            }
        }
        if any {
            p { class: "help folders-total",
                "All folders: {plural(files, \"file\", \"files\")} · {human_bytes(bytes)} on the service."
            }
        }
    }
}

/// A folder this device holds, in the Folders pane.
#[component]
fn HeldFolderRow(ws: Workspace) -> Element {
    let app = use_app();
    // Open again if its list was opened before this session.
    let mut show_files = use_signal(|| app.lib.peek().folder_files.contains_key(&ws.id));
    let mut confirm = use_signal(|| false);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let state = app.folder_sync.read().get(&ws.id).cloned();
    let held = app.folders.read().holds(&ws.id);
    let revoked = app.folders.read().revoked(&ws.id).map(str::to_string);
    let chats = app.lib.read().chats_with_workspace(&ws.id);
    let line = folder_sync_line(state.as_ref(), &ws.sync);
    let failed_line = matches!(state, Some(SyncState::Failed(_)));
    let running = matches!(
        state,
        Some(SyncState::Scanning | SyncState::Uploading { .. })
    );
    let id = ws.id.clone();
    let name = ws.name.clone();

    rsx! {
        div { class: "device folder-row",
            Icon { name: "folder" }
            div { class: "grow",
                b { "{ws.name}" }
                small { class: "mono", "{ws.location}" }
                small { {folder_size_text(&ws.sync)} }
                small {
                    class: if failed_line { "folder-live bad" } else { "folder-live" },
                    role: "status",
                    if running { span { class: "spinner xs" } }
                    "{line}"
                }
                if ws.sync.failed > 0 {
                    small { class: "folder-live warn",
                        {plural(ws.sync.failed as u64, "file couldn't be read", "files couldn't be read")}
                        " · "
                        button {
                            class: "link-btn",
                            onclick: move |_| show_files.set(true),
                            "Show"
                        }
                    }
                }
                small { class: "faint",
                    match chats {
                        0 => "In no chat yet".to_string(),
                        1 => "In 1 chat".to_string(),
                        n => format!("In {n} chats"),
                    }
                }
                if revoked.is_some() {
                    span { class: "badge bad", title: revoked.clone().unwrap_or_default(), "No longer accessible — attach it again" }
                } else if !held {
                    span { class: "badge bad", "No access on this device — remove it" }
                }
                if let Some(err) = error.read().clone() {
                    span { class: "save-state error", "{err}" }
                }
                div { class: "row-actions",
                    if held {
                        button {
                            class: "btn btn-secondary btn-sm",
                            disabled: running,
                            onclick: {
                                let id = id.clone();
                                move |_| crate::folders::sync::start(&id)
                            },
                            Icon { name: "refresh" }
                            "Sync now"
                        }
                    }
                    button {
                        class: "btn btn-secondary btn-sm",
                        "aria-expanded": show_files.read().to_string(),
                        onclick: move |_| show_files.toggle(),
                        "Files…"
                    }
                    button {
                        class: "btn btn-ghost btn-sm",
                        onclick: move |_| confirm.set(true),
                        "Remove…"
                    }
                }
                if *show_files.read() {
                    FolderFiles { workspace_id: ws.id.clone() }
                }
            }
        }
        if *confirm.read() {
            ConfirmDialog {
                title: "Remove the folder “{name}”?",
                message: "Its files are deleted from the Margin service and chats that had it attached lose it. Nothing on this device changes.",
                confirm_label: "Remove",
                busy: *busy.read(),
                on_cancel: move |_| confirm.set(false),
                on_confirm: move |_| {
                    let id = id.clone();
                    async move {
                        busy.set(true);
                        match crate::folders::sync::remove(app, &id).await {
                            Ok(()) => error.set(None),
                            Err(e) => error.set(Some(e)),
                        }
                        busy.set(false);
                        confirm.set(false);
                    }
                },
            }
        }
    }
}

/// A folder's files as the service has them, each with its status; those that couldn't
/// be read can be read again.
#[component]
fn FolderFiles(workspace_id: Id) -> Element {
    let app = use_app();
    let ws = workspace_id.clone();
    let mut load = use_resource(move || {
        let ws = ws.clone();
        async move {
            let api = app.api().ok_or_else(|| "Not signed in.".to_string())?;
            let files = api
                .workspace_files(&ws)
                .await
                .map_err(|e| app.failure(&e))?;
            let mut lib = app.lib;
            lib.write().replace_folder_files(&ws, files);
            Ok::<(), String>(())
        }
    });
    let mut error = use_signal(|| None::<String>);
    let files: Option<Vec<Source>> = app
        .lib
        .read()
        .folder_files_in(&workspace_id)
        .map(|list| list.into_iter().cloned().collect());
    let load_error = match load.read().as_ref() {
        Some(Err(e)) => Some(e.clone()),
        _ => None,
    };
    rsx! {
        div { class: "folder-files",
            if let Some(err) = load_error {
                ErrorNotice { message: err, on_retry: move |_| load.restart() }
            }
            if let Some(err) = error.read().clone() {
                ErrorNotice { message: err }
            }
            match files {
                None => rsx! { div { class: "skel", style: "width: 60%" } },
                Some(files) if files.is_empty() => rsx! {
                    p { class: "faint", "The service has none of this folder's files yet." }
                },
                Some(files) => rsx! {
                    div { role: "list", "aria-label": "Files of the folder",
                        for f in files {
                            div { key: "{f.id}", role: "listitem",
                                {
                                    let (tone, status) = status_text(&f.status);
                                    rsx! {
                                        SourceLine {
                                            kind: f.kind,
                                            name: f.name.clone(),
                                            bytes: f.bytes,
                                            tone,
                                            status,
                                            if f.status.can_read_again() {
                                                ReadAgainButton {
                                                    id: f.id.clone(),
                                                    name: f.name.clone(),
                                                    on_error: move |e| error.set(Some(e)),
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
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
            div { class: "set-sec-head",
                h2 { "Tools on the service Mac" }
                button {
                    class: "btn btn-ghost btn-sm",
                    onclick: move |_| tools.restart(),
                    Icon { name: "refresh" }
                    "Check again"
                }
            }
            p { class: "help", "ManimGX renders videos. The FluidAudio CLI (fluidaudiocli) voices their narration and transcribes audio and video sources; without it, recordings are read without a transcript. Both run on the service Mac, and Margin never installs them for you." }
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

/// How large everything in the window is drawn (page zoom), on every platform. The Mac
/// and Linux also have View > Zoom In / Zoom Out / Actual Size; on iOS it multiplies
/// the system text size.
#[component]
fn TextSizeSection() -> Element {
    let app = use_app();
    let current = crate::zoom::clamp(app.config.read().zoom);
    let mut error = use_signal(|| None::<String>);
    rsx! {
        section { class: "set-sec",
            h2 { "Text size" }
            p { class: "help",
                if cfg!(all(feature = "desktop", target_os = "macos")) {
                    "Makes text and controls larger or smaller. View › Zoom In (⌘+) and Zoom Out (⌘−) change it too."
                } else if cfg!(feature = "desktop") {
                    "Makes text and controls larger or smaller. View › Zoom In (Ctrl++) and Zoom Out (Ctrl+−) change it too."
                } else {
                    "Makes text and controls larger or smaller, on top of the system text size."
                }
            }
            div { class: "field", style: "justify-self: start; min-width: 160px",
                label { r#for: "text-size", "Size" }
                select {
                    id: "text-size",
                    class: "select-input",
                    onchange: move |e| {
                        let Ok(level) = e.value().parse::<u32>() else { return };
                        if let Err(e) = app.set_config(|c| c.zoom = level) {
                            error.set(Some(format!("Changed for now, but couldn't be saved: {e}")));
                        } else {
                            error.set(None);
                        }
                    },
                    for level in crate::zoom::STEPS.iter().copied() {
                        option {
                            key: "{level}",
                            value: "{level}",
                            selected: level == current,
                            if level == crate::zoom::DEFAULT { "{level}% (default)" } else { "{level}%" }
                        }
                    }
                }
            }
            if let Some(err) = error.read().clone() {
                span { class: "save-state error", "{err}" }
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
    use chrono::Utc;

    fn folders_lib() -> crate::store::LibraryState {
        let mut lib = crate::store::LibraryState::from_library(crate::fake::sample_library());
        lib.replace_workspaces(crate::fake::sample_workspaces());
        lib.replace_devices(crate::fake::FakeState::new().devices);
        lib
    }

    #[test]
    fn the_folders_pane_lists_this_devices_folders_and_the_others() {
        use crate::folders::store::{FolderStatus, LocalFolders};
        use crate::ui::test_render::{Extra, render_with};
        let mut lib = folders_lib();
        // A folder of this device the store doesn't hold.
        let mut lost = lib.workspaces["w2"].clone();
        lost.id = "w5".into();
        lost.name = "Old notes".into();
        lib.workspaces.insert("w5".into(), lost);
        // A chat with notes attached; its file list was opened before.
        lib.chats.insert(
            "c9".into(),
            margin_shared::Chat {
                workspace_ids: vec!["w1".into()],
                ..crate::fake::sample_chats()[0].0.clone()
            },
        );
        lib.replace_folder_files("w1", crate::fake::sample_folder_files());
        let extra = Extra {
            folders: LocalFolders {
                held: [
                    ("w1".to_string(), FolderStatus::Ok),
                    (
                        "w2".to_string(),
                        FolderStatus::Revoked("macOS didn't let Margin read “Papers”.".into()),
                    ),
                ]
                .into(),
            },
            folder_sync: [(
                "w1".to_string(),
                SyncState::Uploading {
                    done: 2,
                    total: 12,
                    bytes_done: 41 * 1024 * 1024,
                    bytes_total: 230 * 1024 * 1024,
                },
            )]
            .into(),
            ..Default::default()
        };
        let html = render_with("/", lib, Some(SettingsPane::Folders), extra);
        assert!(html.contains("<h2>This device</h2>"), "{html}");
        assert!(html.contains("<h2>Other devices</h2>"));
        assert!(html.contains("are copied to your Margin service and read there"));
        assert!(html.contains("5 files · 14 MB on the service"));
        assert!(html.contains("Sending 3 of 12 (41 MB of 230 MB)…"));
        assert!(html.contains("1 file couldn&#39;t be read"), "{html}");
        assert!(html.contains(">Show<"));
        assert!(html.contains("In 1 chat"));
        assert!(html.contains("No longer accessible — attach it again"));
        assert!(html.contains("No access on this device — remove it"));
        assert!(html.contains(">Sync now<"));
        assert!(html.contains(">Files…<"));
        assert!(html.contains(">Remove…<"));
        assert!(html.contains(">Attach a folder…<"));
        // The file list: each file with its status, Read again for the one that failed.
        assert!(html.contains("ch1/intro.md"));
        assert!(html.contains("Processing 3/20"));
        assert!(html.contains("Failed: The PDF is encrypted."));
        assert!(html.contains(r#"aria-label="Read old.pdf again""#));
        // Other devices: where, on which device, synced or not, online or offline.
        assert!(html.contains("iCloud Drive/Photos · on iPhone · 18 files · Synced"));
        assert!(
            html.contains("On My iPhone/Scans · on iPhone · 0 files · Not synced yet · Offline")
        );
        assert!(html.contains("Remove it on iPhone."));
        assert!(html.contains("All folders: 107 files"));
    }

    #[test]
    fn folder_lines_read_well() {
        let sync = WorkspaceSync {
            files: 1,
            bytes: 2048,
            ..Default::default()
        };
        assert_eq!(folder_size_text(&sync), "1 file · 2.0 KB on the service");
        assert_eq!(folder_sync_line(None, &sync), "Not synced yet");
        assert_eq!(
            folder_sync_line(Some(&SyncState::Scanning), &sync),
            "Scanning…"
        );
        assert_eq!(
            folder_sync_line(Some(&SyncState::Paused), &sync),
            "Paused — open Margin to continue"
        );
        assert_eq!(
            folder_sync_line(
                Some(&SyncState::Failed("Couldn't send “a.md”.".into())),
                &sync
            ),
            "Couldn't send “a.md”."
        );
        let reading = WorkspaceSync {
            reading: 4,
            ..sync.clone()
        };
        assert_eq!(folder_sync_line(None, &reading), "Reading 4 files…");
        let synced = WorkspaceSync {
            last_synced_at: Some(Utc::now() - chrono::Duration::minutes(5)),
            ..sync
        };
        assert_eq!(folder_sync_line(None, &synced), "Synced 5 min ago");
        assert_eq!(
            folder_sync_line(Some(&SyncState::Idle { last: Utc::now() }), &synced),
            "Synced just now"
        );
        let mut ws = crate::fake::sample_workspaces()[0].clone();
        assert!(other_folder_line(&ws, false).contains("· Paused · Offline"));
        assert!(other_folder_line(&ws, true).contains("· Sending 3 files · Online"));
        ws.sync.uploading = 0;
        assert!(other_folder_line(&ws, true).contains("· Synced"));
    }

    #[test]
    fn ai_pane_sets_the_chat_model_and_reasoning() {
        use crate::store::LibraryState;
        let mut lib = LibraryState::from_library(crate::fake::sample_library());
        let mut settings = crate::fake::FakeState::new().settings;
        settings.reasoning_effort = ReasoningEffort::High;
        lib.settings = Some(settings);
        let html = crate::ui::test_render::render_with_settings("/", lib, Some(SettingsPane::Ai));
        assert!(html.contains("Model for chats"), "{html}");
        assert!(html.contains("Same as above (gpt-5)"));
        for e in ReasoningEffort::ALL {
            assert!(html.contains(effort_label(e)));
        }
    }

    #[test]
    fn effort_from_an_older_build_shows_as_low() {
        use crate::store::LibraryState;
        assert_eq!(shown_effort(ReasoningEffort::Minimal), ReasoningEffort::Low);
        assert_eq!(shown_effort(ReasoningEffort::Max), ReasoningEffort::Max);
        let mut lib = LibraryState::from_library(crate::fake::sample_library());
        let mut settings = crate::fake::FakeState::new().settings;
        settings.reasoning_effort = ReasoningEffort::Minimal;
        lib.settings = Some(settings);
        let html = crate::ui::test_render::render_with_settings("/", lib, Some(SettingsPane::Ai));
        // Exactly one option is chosen, and it's Low; Minimal is never offered.
        assert_eq!(html.matches(r#"aria-checked="true""#).count(), 1, "{html}");
        assert!(
            html.contains(r#"aria-checked="true" tabindex="0">Low<"#),
            "{html}"
        );
        assert!(!html.contains("Minimal"));
        assert_eq!(ReasoningEffort::ALL.len(), 5);
    }

    fn device(id: &str, name: &str, online: bool, current: bool) -> Device {
        use chrono::TimeZone;
        Device {
            id: id.into(),
            name: name.into(),
            created_at: chrono::Utc.timestamp_opt(1_760_000_000, 0).unwrap(),
            last_seen_at: None,
            current,
            online,
        }
    }

    #[test]
    fn devices_show_who_is_online() {
        use crate::store::LibraryState;
        let mut lib = LibraryState::from_library(crate::fake::sample_library());
        // Known from events: `current` isn't set there; this device is the signed-in one.
        lib.merge_devices(vec![
            device("d2", "iPhone", false, false),
            device("d1", "test-mac", true, false),
            device("d3", "iPad", true, false),
        ]);
        let html =
            crate::ui::test_render::render_with_settings("/", lib, Some(SettingsPane::Account));
        assert!(html.contains(r#"class="dot ok""#), "{html}");
        assert!(html.contains("Online · added"));
        assert!(html.contains("Offline · added"));
        // This device first, with its badge.
        let mac = html.find(">test-mac").unwrap();
        let phone = html.find(">iPhone").unwrap();
        assert!(mac < phone);
        assert!(html.contains(">This device<"));
        let order: Vec<String> = devices_in_order(&[
            device("a", "old", false, false),
            Device {
                created_at: chrono::Utc::now(),
                ..device("b", "new", false, false)
            },
            device("c", "me", false, true),
        ])
        .into_iter()
        .map(|d| d.id)
        .collect();
        assert_eq!(order, ["c", "b", "a"]);
        let mut seen = device("x", "x", false, false);
        assert_eq!(presence_text(&seen), "Offline");
        seen.last_seen_at = Some(chrono::Utc::now());
        assert_eq!(presence_text(&seen), "Offline · last seen just now");
    }

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
