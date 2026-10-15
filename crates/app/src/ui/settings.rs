//! Settings: service, account and devices, the AI providers (the default one; OpenAI's
//! key, "Sign in with ChatGPT", Claude's key; each provider's three model slots) and the
//! retrieval pipeline's stages and budget, the embedding model for sources (and the
//! speech model a Linux service downloads), voice, render
//! size, tools on the service, trace retention, and appearance.

use dioxus::prelude::*;
use margin_shared::pipeline::{
    Budget, NamedVariant, PipelineConfig, PipelineVariants, Route, STAGE_KINDS,
};
use margin_shared::{
    CLAUDE_LISTED_MODELS, ChatGptStatus, Device, EmbeddingModel, EmbeddingModelState, Event,
    FOLDER_FILE_TYPES_TEXT, Id, ModelInfo, Provider, ReasoningEffort, ReindexProgress, RouteModels,
    SOURCE_KINDS, Settings, SettingsPatch, Source, SourceKind, SpeechModel, TRACE_RETENTION_DAYS,
    ToolStatus, UnreadableFile, Workspace, WorkspaceSync,
};

use crate::config::{APP_NAME, Appearance, PINNED_SERVICE_URL, normalize_service_url};
use crate::folders::sync::SyncState;
use crate::sources::{Tone, human_bytes, status_text};
use crate::state::{LiveStatus, use_app};
use crate::ui::common::{
    ConfirmDialog, ErrorNotice, Icon, ago, copy_to_clipboard, date, focus_first, js, open_external,
};
use crate::ui::organisation::{
    EveryonesDevices, MySettingsSection, OrganisationPane, PersonLine, ReadOnlyAdvanced,
    ReadOnlyAi, ReadOnlyVideo, devices_by_person,
};
use crate::ui::sources::kind_icon;

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
    Organisation,
    Service,
    Ai,
    Sources,
    Folders,
    Video,
    Tools,
    Advanced,
}

impl SettingsPane {
    pub const ALL: [SettingsPane; 10] = [
        SettingsPane::General,
        SettingsPane::Account,
        SettingsPane::Organisation,
        SettingsPane::Service,
        SettingsPane::Ai,
        SettingsPane::Sources,
        SettingsPane::Folders,
        SettingsPane::Video,
        SettingsPane::Tools,
        SettingsPane::Advanced,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SettingsPane::General => "General",
            SettingsPane::Account => "Account",
            SettingsPane::Organisation => "Organisation",
            SettingsPane::Service => "Service",
            SettingsPane::Ai => "AI",
            SettingsPane::Sources => "Sources",
            SettingsPane::Folders => "Folders",
            SettingsPane::Video => "Voice & video",
            SettingsPane::Tools => "Tools",
            SettingsPane::Advanced => "Advanced",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            SettingsPane::General => "half",
            SettingsPane::Account => "user",
            SettingsPane::Organisation => "users",
            SettingsPane::Service => "server",
            SettingsPane::Ai => "spark",
            SettingsPane::Sources => "clip",
            SettingsPane::Folders => "folder",
            SettingsPane::Video => "video",
            SettingsPane::Tools => "wrench",
            SettingsPane::Advanced => "gear",
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
    let admin = app.is_admin();
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
                            SettingsPane::Organisation => rsx! { OrganisationPane {} },
                            SettingsPane::Service => rsx! { ServiceSection {} },
                            // A member reads the organisation's settings; admins change them.
                            SettingsPane::Ai if !admin => service_settings(|s| rsx! {
                                ReadOnlyAi { settings: s }
                            }),
                            SettingsPane::Video if !admin => service_settings(|s| rsx! {
                                ReadOnlyVideo { settings: s }
                            }),
                            SettingsPane::Advanced if !admin => service_settings(|s| rsx! {
                                ReadOnlyAdvanced { settings: s }
                            }),
                            SettingsPane::Ai => service_settings(|s| rsx! {
                                ProvidersSection { settings: s.clone() }
                                OpenAiSection { settings: s.clone() }
                                ChatGptSection { settings: s.clone() }
                                ClaudeSection { settings: s.clone() }
                                AgentSection { settings: s.clone() }
                                PipelineSection { settings: s }
                            }),
                            SettingsPane::Sources => rsx! { SourcesSection {} },
                            SettingsPane::Folders => rsx! { FoldersSection {} },
                            SettingsPane::Video => service_settings(|s| rsx! {
                                VoiceSection { settings: s.clone() }
                                RenderSection { settings: s }
                            }),
                            SettingsPane::Tools => rsx! { ToolsSection {} },
                            SettingsPane::Advanced => service_settings(|s| rsx! {
                                TracesSection { settings: s }
                            }),
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
            p { class: "help", "Where your library lives: the Margin service." }
            if PINNED_SERVICE_URL.is_some() {
                // A release build is pinned to its service (config.rs): nothing to change.
                p { class: "help", "This build of {APP_NAME} is made for this service; a developer build can connect to any." }
                div { class: "kv",
                    span { "Address" }
                    span { class: "mono", "{current}" }
                }
            } else if *editing.read() {
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
    let my_id = me
        .as_ref()
        .and_then(|m| m.device.as_ref())
        .map(|d| d.id.clone());
    // The person's own devices here; an admin's list also has everyone else's, shown
    // below by person.
    let person = me.as_ref().and_then(|m| m.person.clone());
    let all: Vec<Device> = app
        .lib
        .read()
        .devices
        .values()
        .map(|d| Device {
            current: d.current || my_id.as_deref() == Some(d.id.as_str()),
            ..d.clone()
        })
        .collect();
    let (known, others): (Vec<Device>, Vec<Device>) =
        all.into_iter()
            .partition(|d| match (&person, &d.person_id) {
                (Some(p), Some(owner)) => *owner == p.id,
                _ => true,
            });
    let everyone = match (&person, app.is_admin()) {
        (Some(p), true) => Some(devices_by_person(
            &others,
            &p.id,
            &app.lib.read().people.values().cloned().collect::<Vec<_>>(),
        )),
        _ => None,
    };
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
            if let Some(p) = person.clone() {
                PersonLine {
                    person: p,
                    device: me.as_ref().and_then(|m| m.device.as_ref()).map(|d| d.name.clone()).unwrap_or_default(),
                    on_sign_out: move |_| confirm_signout.set(true),
                }
            } else {
                // A service before L1 (or `me` still loading).
                div { class: "kv",
                    span { "GitHub" }
                    span { {me.as_ref().map(|m| m.github_login.clone()).unwrap_or_else(|| "Loading…".into())} }
                    span { "This device" }
                    span { {me.as_ref().and_then(|m| m.device.as_ref()).map(|d| d.name.clone()).unwrap_or_else(|| "Loading…".into())} }
                }
                div { class: "row-actions",
                    button { class: "btn btn-secondary btn-sm", onclick: move |_| confirm_signout.set(true), "Sign out…" }
                }
            }
        }
        if let Some(p) = person.clone() {
            MySettingsSection { person: p }
        }
        section { class: "set-sec",
            h2 { if everyone.is_some() { "My devices" } else { "Devices" } }
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
        if let Some(groups) = everyone {
            EveryonesDevices { groups, on_revoke: move |d: Device| confirm_revoke.set(Some(d)) }
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

/// The label of a model slot (Settings → AI, each provider's block).
pub fn slot_label(route: Route) -> &'static str {
    match route {
        Route::Chat => "Model for chats",
        Route::Write => "Model for writing",
        Route::Small => "Model for small tasks",
    }
}

/// What a model slot is used for.
pub fn slot_help(route: Route) -> &'static str {
    match route {
        Route::Chat => "Answers in chats and uses the agent's tools.",
        Route::Write => "Writes lessons, video scenes and course outlines.",
        Route::Small => "Names chats and condenses long ones.",
    }
}

/// The provider block Settings → AI scrolls to when it opens (an error's "Open Settings",
/// the chat's "Set up" notice), then forgets.
pub static AI_FOCUS: GlobalSignal<Option<Provider>> = Signal::global(|| None);

/// Open Settings → AI at `provider`'s block.
pub fn open_provider_settings(app: crate::state::AppCtx, provider: Provider) {
    *AI_FOCUS.write() = Some(provider);
    let mut open = app.settings_open;
    open.set(Some(SettingsPane::Ai));
}

/// The id of a provider's block in Settings → AI: "ai-openai", "ai-chatgpt", "ai-claude".
pub fn provider_block_id(provider: Provider) -> String {
    format!("ai-{}", provider.as_str())
}

/// What a provider is, under its heading.
pub fn provider_help(provider: Provider) -> &'static str {
    match provider {
        Provider::OpenAi => {
            "OpenAI's Responses API (or an OpenAI-compatible one) with an API key you pay for by use."
        }
        Provider::ChatGpt => {
            "Margin uses your ChatGPT plan for what you ask it here. You can cap Margin's share in ChatGPT → Settings → Usage."
        }
        Provider::Claude => "Anthropic's Messages API with an API key you pay for by use.",
    }
}

/// What a provider needs before it answers, for the default-provider note and the chat's
/// notice: "Add an OpenAI API key", "Sign in with ChatGPT", "Add a Claude API key".
pub fn provider_setup(provider: Provider) -> &'static str {
    match provider {
        Provider::OpenAi => "Add an OpenAI API key",
        Provider::ChatGpt => "Sign in with ChatGPT",
        Provider::Claude => "Add a Claude API key",
    }
}

/// "1M context", "200k context": a model's window, as Settings shows it beside the id.
pub fn window_text(tokens: u32) -> String {
    if tokens >= 1_000_000 {
        // One decimal at most, rounded down: 1 050 000 is "1M", 1 500 000 "1.5M".
        let tenths = tokens / 100_000;
        if tenths.is_multiple_of(10) {
            format!("{}M context", tenths / 10)
        } else {
            format!("{}.{}M context", tenths / 10, tenths % 10)
        }
    } else if tokens >= 1000 {
        format!("{}k context", tokens / 1000)
    } else {
        format!("{tokens} context")
    }
}

/// What the service runs a model on when its provider doesn't say its window (D13).
pub const WINDOW_UNKNOWN: &str = "window unknown, using 200k";

/// The note beside a slot's model: its window when the provider's list has it, "window
/// unknown, using 200k" when the list doesn't know it; nothing before the list is here
/// or for an empty slot.
pub fn window_note(models: Option<&[ModelInfo]>, id: &str) -> Option<String> {
    let models = models?;
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    Some(
        match models
            .iter()
            .find(|m| m.id == id)
            .and_then(|m| m.context_window)
        {
            Some(window) => window_text(window),
            None => WINDOW_UNKNOWN.to_string(),
        },
    )
}

/// Whether `model` is one of `provider`'s: in its list when the list is here, else by
/// name (Claude's ids start with "claude").
pub fn model_belongs(provider: Provider, model: &str, models: Option<&[ModelInfo]>) -> bool {
    match models {
        Some(list) => list.iter().any(|m| m.id == model),
        None => model.starts_with("claude") == (provider == Provider::Claude),
    }
}

/// The model a pipeline variant pins for `route`, when it is `provider`'s: the service
/// runs that instead of the slot (`pipeline.routes`, read-only here).
pub fn slot_override(
    settings: &Settings,
    provider: Provider,
    route: Route,
    models: Option<&[ModelInfo]>,
) -> Option<String> {
    let id = settings.pipeline.routes.get(route)?.trim();
    (!id.is_empty() && model_belongs(provider, id, models)).then(|| id.to_string())
}

/// The models a slot offers, in order: Claude's defaults first (Opus, Sonnet, Haiku 5.5),
/// then the rest as the provider lists them.
pub fn model_options(provider: Provider, models: &[ModelInfo]) -> Vec<ModelInfo> {
    let mut out: Vec<ModelInfo> = Vec::with_capacity(models.len());
    if provider == Provider::Claude {
        for id in CLAUDE_LISTED_MODELS {
            if let Some(m) = models.iter().find(|m| m.id == id) {
                out.push(m.clone());
            }
        }
    }
    for m in models {
        if !out.iter().any(|o| o.id == m.id) {
            out.push(m.clone());
        }
    }
    out
}

/// The patch that sets `provider`'s three slots.
pub fn models_patch(provider: Provider, models: RouteModels) -> SettingsPatch {
    let mut patch = SettingsPatch::default();
    match provider {
        Provider::OpenAi => patch.openai_models = Some(models),
        Provider::ChatGpt => patch.chatgpt_models = Some(models),
        Provider::Claude => patch.claude_models = Some(models),
    }
    patch
}

/// The patch that stores (or, with "", removes) `provider`'s API key. ChatGPT has none.
pub fn key_patch(provider: Provider, key: String) -> SettingsPatch {
    match provider {
        Provider::Claude => SettingsPatch {
            anthropic_api_key: Some(key),
            ..Default::default()
        },
        _ => SettingsPatch {
            openai_api_key: Some(key),
            ..Default::default()
        },
    }
}

/// Whether this app runs on a Mac, where "Continue with ChatGPT" opens the sign-in in
/// the browser (the service usually runs on the same Mac).
pub const OPENS_SIGN_IN_HERE: bool = cfg!(all(feature = "desktop", target_os = "macos"));

/// Fetch `provider`'s models into the store (Settings and the slots show them).
async fn load_models(app: crate::state::AppCtx, provider: Provider) -> Result<(), String> {
    let api = app.api().ok_or_else(|| "Not signed in.".to_string())?;
    match api.provider_models(provider).await {
        Ok(list) => {
            let mut lib = app.lib;
            lib.write().provider_models.insert(provider, list);
            Ok(())
        }
        Err(e) => Err(app.failure(&e)),
    }
}

/// The top of Settings → AI: which provider new chats and the jobs use.
#[component]
fn ProvidersSection(settings: Settings) -> Element {
    let app = use_app();
    let error = use_signal(|| None::<String>);
    let busy = use_signal(|| false);
    let current = settings.default_provider;
    // Scroll to the block an error or the chat asked for.
    use_effect(move || {
        if let Some(p) = *AI_FOCUS.read() {
            let id = provider_block_id(p);
            let _ = js(&format!(
                "setTimeout(function () {{ var el = document.getElementById('{id}'); if (el) {{ el.scrollIntoView({{block: 'start'}}); var f = el.querySelector('input, button'); if (f) f.focus({{preventScroll: true}}); }} }}, 50);"
            ));
            *AI_FOCUS.write() = None;
        }
    });

    rsx! {
        section { class: "set-sec",
            h2 { "Providers" }
            p { class: "help", "Margin can answer with OpenAI, your ChatGPT plan or Claude. Each chat stays on the provider it was started with; lessons, videos, outlines, chat titles and condensing use the default." }
            div { class: "field",
                span { class: "label", id: "ai-default-label", "Default provider" }
                div { class: "seg", role: "radiogroup", "aria-labelledby": "ai-default-label", "aria-busy": busy.read().to_string(),
                    for p in Provider::ALL {
                        button {
                            key: "{p.as_str()}",
                            role: "radio",
                            "aria-checked": (p == current).to_string(),
                            tabindex: if p == current { "0" } else { "-1" },
                            onclick: move |_| async move {
                                if p == current || *busy.peek() {
                                    return;
                                }
                                let patch = SettingsPatch { default_provider: Some(p), ..Default::default() };
                                save_settings(app, patch, error, busy).await;
                            },
                            {p.label()}
                        }
                    }
                }
                if !settings.has_credential(current) {
                    span { id: "ai-default-setup", class: "faint", style: "font-size: 13px",
                        "{provider_setup(current)} to use it. "
                        button {
                            class: "link-btn",
                            onclick: move |_| open_provider_settings(app, current),
                            "Set it up below"
                        }
                    }
                }
                if let Some(err) = error.read().clone() {
                    span { class: "save-state error", role: "alert", "{err}" }
                }
            }
        }
    }
}

/// OpenAI: the base URL, the API key and the three slots.
#[component]
fn OpenAiSection(settings: Settings) -> Element {
    let app = use_app();
    let mut base = use_signal(|| settings.openai_base_url.clone());
    let error = use_signal(|| None::<String>);
    let busy = use_signal(|| false);
    let mut saved_note = use_signal(|| false);
    let mut base_error = use_signal(|| None::<&'static str>);
    let dirty = base.read().trim() != settings.openai_base_url;

    rsx! {
        section { id: provider_block_id(Provider::OpenAi), class: "set-sec",
            h2 { "OpenAI" }
            p { class: "help", {provider_help(Provider::OpenAi)} }
            form {
                class: "row-actions",
                style: "align-items: flex-end",
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
                            if b == settings.openai_base_url {
                                return;
                            }
                            let problem = base_url_problem(&b);
                            base_error.set(problem);
                            if problem.is_some() {
                                focus_first("#ai-base");
                                return;
                            }
                            let patch = SettingsPatch { openai_base_url: Some(b), ..Default::default() };
                            if save_settings(app, patch, error, busy).await {
                                saved_note.set(true);
                            }
                        }
                    }
                },
                div { class: "field", style: "flex: 1; min-width: 220px",
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
                button {
                    r#type: "submit",
                    class: "btn btn-secondary btn-sm",
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
            ApiKeyField { provider: Provider::OpenAi, has_key: settings.has_api_key }
            // Mounted again when the credential comes or goes or the list went stale
            // (another key, base URL or account), so the list is fetched then.
            ModelSlots { key: "{app.lib.read().slots_key(&settings, Provider::OpenAi)}", provider: Provider::OpenAi, settings: settings.clone() }
        }
    }
}

/// Claude: the API key and the three slots.
#[component]
fn ClaudeSection(settings: Settings) -> Element {
    let app = use_app();
    rsx! {
        section { id: provider_block_id(Provider::Claude), class: "set-sec",
            h2 { "Claude" }
            p { class: "help", {provider_help(Provider::Claude)} }
            ApiKeyField { provider: Provider::Claude, has_key: settings.has_claude_api_key }
            // Mounted again when the credential comes or goes or the list went stale
            // (another key, base URL or account), so the list is fetched then.
            ModelSlots { key: "{app.lib.read().slots_key(&settings, Provider::Claude)}", provider: Provider::Claude, settings: settings.clone() }
        }
    }
}

/// A provider's API key: stored on the service (never on this device), replaced, removed.
#[component]
fn ApiKeyField(provider: Provider, has_key: bool) -> Element {
    let app = use_app();
    let mut key = use_signal(String::new);
    // Bumped when the key field is cleared by code, to remount it empty.
    let mut key_rev = use_signal(|| 0u64);
    let mut replacing = use_signal(|| false);
    let mut confirm_remove = use_signal(|| false);
    let key_error = use_signal(|| None::<String>);
    let key_busy = use_signal(|| false);
    let show_key_input = !has_key || *replacing.read();
    let id = format!("ai-key-{}", provider.as_str());
    let placeholder = match provider {
        Provider::Claude => "sk-ant-…",
        _ => "sk-…",
    };

    rsx! {
        div { class: "field",
            label { r#for: "{id}", "API key" }
            if show_key_input {
                form {
                    class: "row-actions",
                    onsubmit: move |e: FormEvent| async move {
                        e.prevent_default();
                        let k = key.peek().trim().to_string();
                        if k.is_empty() || *key_busy.peek() {
                            return;
                        }
                        if save_settings(app, key_patch(provider, k), key_error, key_busy).await {
                            // Another key may see other models.
                            let mut lib = app.lib;
                            lib.write().forget_models(provider);
                            key.set(String::new());
                            key_rev += 1;
                            replacing.set(false);
                        }
                    },
                    for rev in std::iter::once(key_rev()) {
                    input {
                        key: "{rev}",
                        id: "{id}",
                        class: "input",
                        style: "flex: 1; min-width: 220px",
                        r#type: "password",
                        autocomplete: "off",
                        placeholder,
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
                    if has_key {
                        button { r#type: "button", class: "btn btn-ghost btn-sm", onclick: move |_| { key.set(String::new()); key_rev += 1; replacing.set(false) }, "Cancel" }
                    }
                }
                p { class: "faint", style: "margin: 0; font-size: 13px", "Sent once to the service and kept in its secret store (the Keychain on a Mac, a private file on Linux), never on this device. It is never shown again." }
            } else {
                div { class: "key-saved",
                    Icon { name: "check" }
                    span { class: "grow", style: "white-space: normal", "Saved in the service's secret store" }
                    button { class: "btn btn-secondary btn-sm", "aria-label": "Replace the {provider.label()} key", onclick: move |_| replacing.set(true), "Replace" }
                    button { class: "btn btn-secondary btn-sm", "aria-label": "Remove the {provider.label()} key", onclick: move |_| confirm_remove.set(true), "Remove…" }
                }
            }
            if let Some(err) = key_error.read().clone() {
                span { class: "save-state error", "{err}" }
            }
        }
        if *confirm_remove.read() {
            ConfirmDialog {
                title: "Remove the {provider.label()} API key?",
                message: "The key is deleted from the service's secret store. Chats on {provider.label()} stop answering, and so do lessons and videos while it is the default, until you add a key again.",
                confirm_label: "Remove key",
                busy: *key_busy.read(),
                on_cancel: move |_| confirm_remove.set(false),
                on_confirm: move |_| async move {
                    save_settings(app, key_patch(provider, String::new()), key_error, key_busy).await;
                    confirm_remove.set(false);
                },
            }
        }
    }
}

/// A provider's three model slots: chats, writing, small tasks. Each is a combobox over
/// the provider's models once they are here (free text otherwise), with the model's
/// window beside it and, when a pipeline variant pins the route, the model it runs.
#[component]
fn ModelSlots(provider: Provider, settings: Settings) -> Element {
    let app = use_app();
    let has_credential = settings.has_credential(provider);
    // The list, fetched while the provider has a credential (again when it gets one).
    let mut list_load = use_resource(move || async move {
        if has_credential {
            load_models(app, provider).await
        } else {
            Ok(())
        }
    });
    let list_error = match &*list_load.read() {
        Some(Err(e)) => Some(e.clone()),
        _ => None,
    };
    let models: Option<Vec<ModelInfo>> = app.lib.read().provider_models.get(&provider).cloned();
    let current = settings.models(provider).clone();
    let current_slots = Route::ALL.map(|r| current.get(r).to_string());
    let mut values = use_signal(|| current_slots.clone());
    let mut problems = use_signal(|| [None::<&'static str>; 3]);
    let error = use_signal(|| None::<String>);
    let busy = use_signal(|| false);
    let mut saved_note = use_signal(|| false);
    let dirty = values
        .read()
        .iter()
        .zip(current_slots.iter())
        .any(|(v, c)| v.trim() != c);
    let p = provider.as_str();
    let list_id = format!("ai-models-{p}");
    let options = models
        .as_deref()
        .map(|m| model_options(provider, m))
        .unwrap_or_default();

    rsx! {
        form {
            class: "slots",
            style: "display: grid; gap: 14px",
            novalidate: true,
            "aria-label": "{provider.label()} models",
            onsubmit: move |e: FormEvent| async move {
                e.prevent_default();
                if *busy.peek() {
                    return;
                }
                let wanted = values.peek().clone();
                let found = wanted.clone().map(|v| v.trim().is_empty().then_some("Enter a model id."));
                problems.set(found);
                if let Some(i) = found.iter().position(Option::is_some) {
                    focus_first(&format!("#ai-slot-{p}-{}", Route::ALL[i].as_str()));
                    return;
                }
                let models = RouteModels {
                    chat: wanted[0].trim().to_string(),
                    write: wanted[1].trim().to_string(),
                    small: wanted[2].trim().to_string(),
                };
                if save_settings(app, models_patch(provider, models), error, busy).await {
                    saved_note.set(true);
                }
            },
            if !options.is_empty() {
                datalist { id: "{list_id}",
                    for m in options.iter() {
                        option { key: "{m.id}", value: "{m.id}",
                            {m.display_name.clone().unwrap_or_default()}
                        }
                    }
                }
            }
            div { style: "display: grid; gap: 14px",
                for (i, route) in Route::ALL.into_iter().enumerate() {
                    {
                        let slot_id = format!("ai-slot-{p}-{}", route.as_str());
                        let value = values.read()[i].clone();
                        let note = window_note(models.as_deref(), &value);
                        let pinned = slot_override(&settings, provider, route, models.as_deref());
                        let problem = problems.read()[i];
                        rsx! {
                            div { key: "{route.as_str()}", class: "field",
                                label { r#for: "{slot_id}", {slot_label(route)} }
                                input {
                                    id: "{slot_id}",
                                    class: "input",
                                    autocomplete: "off",
                                    spellcheck: "false",
                                    list: if !options.is_empty() { list_id.clone() },
                                    "aria-invalid": problem.is_some().to_string(),
                                    "aria-describedby": "{slot_id}-help",
                                    placeholder: "A model id",
                                    initial_value: "{current_slots[i]}",
                                    oninput: move |e| {
                                        saved_note.set(false);
                                        problems.write()[i] = None;
                                        values.write()[i] = e.value();
                                    },
                                }
                                span { id: "{slot_id}-help", class: "faint", style: "font-size: 13px",
                                    {slot_help(route)}
                                    if let Some(note) = note {
                                        " · "
                                        span { class: "slot-window", "{note}" }
                                    }
                                }
                                if let Some(problem) = problem {
                                    span { class: "field-error", "{problem}" }
                                }
                                if let Some(id) = pinned {
                                    span { class: "slot-pinned", "Overridden by the pipeline variant: "
                                        span { class: "mono", "{id}" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if !has_credential {
                p { class: "faint", style: "margin: 0; font-size: 13px",
                    "{provider_setup(provider)} to choose from {provider.label()}'s models; until then type a model id."
                }
            }
            if let Some(err) = list_error {
                ErrorNotice { message: format!("Couldn't load {}'s models. {err}", provider.label()), on_retry: move |_| list_load.restart() }
            }
            if let Some(err) = error.read().clone() {
                span { class: "save-state error", role: "alert", "{err}" }
            }
            div { class: "row-actions",
                button {
                    r#type: "submit",
                    class: "btn btn-primary btn-sm",
                    disabled: !dirty || *busy.read(),
                    SaveLabel { busy: *busy.read(), label: "Save models" }
                }
                if *saved_note.read() && !dirty {
                    span { class: "save-state", role: "status", "Saved" }
                }
            }
        }
    }
}

/// Ask the service where the ChatGPT sign-in is, and keep asking every 2 s while it
/// waits for the browser. One poller at a time.
async fn poll_chatgpt(app: crate::state::AppCtx, mut polling: Signal<bool>) {
    if *polling.peek() {
        return;
    }
    polling.set(true);
    loop {
        let Some(api) = app.api() else { break };
        let status = match api.chatgpt_status().await {
            Ok(status) => status,
            Err(e) => {
                app.failure(&e);
                break;
            }
        };
        let waiting = matches!(status, ChatGptStatus::Waiting { .. });
        let mut lib = app.lib;
        lib.write().chatgpt = Some(status);
        if !waiting {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    polling.set(false);
}

/// ChatGPT: "Continue with ChatGPT" (the sign-in runs on the service's Mac), its state,
/// sign out, and the three slots from the account's models.
#[component]
fn ChatGptSection(settings: Settings) -> Element {
    let app = use_app();
    let polling = use_signal(|| false);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let mut confirm_signout = use_signal(|| false);
    let mut copies = use_signal(|| 0u32);
    let mut copied = use_signal(|| None::<Result<(), String>>);
    let mut open_error = use_signal(|| None::<String>);
    // Where the sign-in is now (and, while it waits, again every 2 s).
    use_future(move || poll_chatgpt(app, polling));
    let status = app.lib.read().chatgpt_status();

    let start = move |_| async move {
        if *busy.peek() {
            return;
        }
        let Some(api) = app.api() else { return };
        busy.set(true);
        error.set(None);
        open_error.set(None);
        match api.start_chatgpt().await {
            Ok(started) => {
                let mut lib = app.lib;
                lib.write().chatgpt = Some(ChatGptStatus::Waiting {
                    authorize_url: started.authorize_url.clone(),
                });
                if OPENS_SIGN_IN_HERE {
                    open_error.set(open_external(&started.authorize_url).err());
                }
                spawn(poll_chatgpt(app, polling));
            }
            Err(e) => error.set(Some(app.failure(&e))),
        }
        busy.set(false);
    };
    let sign_out = move || async move {
        let Some(api) = app.api() else { return };
        busy.set(true);
        match api.sign_out_chatgpt().await {
            Ok(()) => {
                error.set(None);
                let mut lib = app.lib;
                lib.write().chatgpt = Some(ChatGptStatus::SignedOut);
                if let Ok(settings) = api.settings().await {
                    lib.write().apply(Event::SettingsChanged { settings });
                }
            }
            Err(e) => error.set(Some(app.failure(&e))),
        }
        busy.set(false);
        confirm_signout.set(false);
    };

    rsx! {
        section { id: provider_block_id(Provider::ChatGpt), class: "set-sec",
            h2 { "ChatGPT" }
            p { class: "help", {provider_help(Provider::ChatGpt)} }
            match status.clone() {
                ChatGptStatus::SignedOut => rsx! {
                    div { class: "row-actions",
                        button {
                            class: "btn btn-primary btn-sm",
                            disabled: *busy.read(),
                            onclick: start,
                            if *busy.read() { span { class: "spinner" } }
                            "Continue with ChatGPT"
                        }
                    }
                    p { class: "faint", style: "margin: 0; font-size: 13px",
                        "You finish signing in in a browser on the Mac that runs Margin's service. Margin never sees your password, and no token is kept on this device."
                    }
                },
                ChatGptStatus::Waiting { authorize_url } => rsx! {
                    div { class: "chatgpt-wait", role: "status",
                        span { class: "spinner xs", "aria-hidden": "true" }
                        span { "Waiting for you to finish signing in…" }
                    }
                    p { class: "faint", style: "margin: 0; font-size: 13px",
                        if OPENS_SIGN_IN_HERE {
                            "The sign-in opened in your browser. If the service runs on another Mac, open this link on that Mac instead:"
                        } else {
                            "Open this link on the Mac that runs Margin's service:"
                        }
                    }
                    div { class: "copy-wrap",
                        pre { class: "code", style: "padding-right: 90px; white-space: pre-wrap; word-break: break-all", code { id: "chatgpt-link", "{authorize_url}" } }
                        button {
                            class: "btn btn-secondary btn-sm copy",
                            "aria-label": "Copy the sign-in link",
                            onclick: {
                                let url = authorize_url.clone();
                                move |_| {
                                    let url = url.clone();
                                    async move {
                                        let n = *copies.peek() + 1;
                                        copies.set(n);
                                        copied.set(None);
                                        let result = copy_to_clipboard(url).await;
                                        let ok = result.is_ok();
                                        copied.set(Some(result));
                                        if ok {
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
                    span { class: "sr-only", role: "status",
                        if matches!(*copied.read(), Some(Ok(()))) { "Copied to the clipboard" }
                    }
                    if let Some(Err(err)) = copied.read().clone() {
                        span { class: "save-state error", role: "alert", "{err}" }
                    }
                    if let Some(err) = open_error.read().clone() {
                        span { class: "save-state error", role: "alert", "{err}" }
                    }
                    div { class: "row-actions",
                        if OPENS_SIGN_IN_HERE {
                            button {
                                class: "btn btn-secondary btn-sm",
                                onclick: {
                                    let url = authorize_url.clone();
                                    move |_| open_error.set(open_external(&url).err())
                                },
                                "Open in browser"
                            }
                        }
                        button {
                            class: "btn btn-ghost btn-sm",
                            disabled: *busy.read(),
                            onclick: move |_| sign_out(),
                            "Cancel"
                        }
                    }
                },
                ChatGptStatus::SignedIn { account, last_error } => rsx! {
                    div { class: "key-saved",
                        Icon { name: "check" }
                        span { class: "grow", style: "white-space: normal",
                            "Signed in as "
                            b { "{account.email}" }
                            span { class: "faint", " · Signed in {date(&account.signed_in_at)}" }
                        }
                        button { class: "btn btn-secondary btn-sm", onclick: move |_| confirm_signout.set(true), "Sign out…" }
                    }
                    if let Some(message) = last_error {
                        p { class: "faint chatgpt-last-error", role: "status", style: "margin: 0; font-size: 13px",
                            "The latest sign-in attempt didn't work; the sign-in above still holds. {message}"
                        }
                    }
                },
                ChatGptStatus::Failed { message } => rsx! {
                    div { class: "notice error", role: "alert",
                        Icon { name: "alert" }
                        div { class: "body",
                            span { "Signing in with ChatGPT didn't work. {message}" }
                            div { class: "row-actions",
                                button {
                                    class: "btn btn-secondary btn-sm",
                                    disabled: *busy.read(),
                                    onclick: start,
                                    "Try again"
                                }
                            }
                        }
                    }
                },
            }
            if let Some(err) = error.read().clone() {
                span { class: "save-state error", role: "alert", "{err}" }
            }
            // Mounted again when the credential comes or goes or the list went stale
            // (another key, base URL or account), so the list is fetched then.
            ModelSlots { key: "{app.lib.read().slots_key(&settings, Provider::ChatGpt)}", provider: Provider::ChatGpt, settings: settings.clone() }
        }
        if *confirm_signout.read() {
            ConfirmDialog {
                title: "Sign out of ChatGPT?",
                message: "The service deletes its ChatGPT sign-in. Chats on ChatGPT stop answering, and so do lessons and videos while it is the default, until you sign in again.",
                confirm_label: "Sign out",
                busy: *busy.read(),
                on_cancel: move |_| confirm_signout.set(false),
                on_confirm: move |_| sign_out(),
            }
        }
    }
}

/// How hard the agent thinks before answering in chats.
#[component]
fn AgentSection(settings: Settings) -> Element {
    let app = use_app();
    let effort_error = use_signal(|| None::<String>);
    let effort_busy = use_signal(|| false);
    let effort = shown_effort(settings.reasoning_effort);

    rsx! {
        section { class: "set-sec",
            h2 { "Chats" }
            p { class: "help", "How much the chat model reasons before answering. More reasoning answers harder questions better but takes longer and costs more." }
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

/// The numbers of the pipeline's `Budget`, as Settings edits them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BudgetField {
    Share,
    MinScore,
    Images,
    PassagesMin,
}

impl BudgetField {
    pub const ALL: [BudgetField; 4] = [
        BudgetField::Share,
        BudgetField::MinScore,
        BudgetField::Images,
        BudgetField::PassagesMin,
    ];

    fn id(self) -> &'static str {
        match self {
            BudgetField::Share => "pl-share",
            BudgetField::MinScore => "pl-score",
            BudgetField::Images => "pl-images",
            BudgetField::PassagesMin => "pl-min",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BudgetField::Share => "Share of the model's window",
            BudgetField::MinScore => "Leave out passages below",
            BudgetField::Images => "Pictures per call, at most",
            BudgetField::PassagesMin => "Passages to keep, at least",
        }
    }

    pub fn help(self) -> &'static str {
        match self {
            BudgetField::Share => "Percent of the model's context window passages may fill.",
            BudgetField::MinScore => {
                "Percentile of the best passages' scores; passages under it are dropped."
            }
            BudgetField::Images => "Pages, figures and frames sent with one model call.",
            BudgetField::PassagesMin => "Kept even when they score under the cut-off.",
        }
    }

    fn unit(self) -> Option<&'static str> {
        match self {
            BudgetField::Share => Some("%"),
            BudgetField::MinScore => Some("th percentile"),
            BudgetField::Images | BudgetField::PassagesMin => None,
        }
    }

    fn range(self) -> (&'static str, Option<&'static str>) {
        match self {
            BudgetField::Share => ("1", Some("100")),
            BudgetField::MinScore => ("0", Some("100")),
            BudgetField::Images | BudgetField::PassagesMin => ("0", None),
        }
    }
}

/// `budget`'s numbers as the form shows them, in `BudgetField::ALL` order (the share as
/// a percentage).
pub fn budget_fields(budget: &Budget) -> [String; 4] {
    let pct = budget.share_of_window * 100.0;
    let share = if (pct - pct.round()).abs() < 0.01 {
        format!("{}", pct.round() as i64)
    } else {
        format!("{pct:.1}")
    };
    [
        share,
        budget.min_score_pct.to_string(),
        budget.images_max.to_string(),
        budget.passages_min.to_string(),
    ]
}

/// The budget the form's numbers make, or the first field that's wrong and why.
pub fn parse_budget(fields: &[String; 4]) -> Result<Budget, (BudgetField, &'static str)> {
    let [share, score, images, min] = fields;
    let share = share
        .trim()
        .trim_end_matches('%')
        .trim()
        .parse::<f32>()
        .ok()
        .filter(|v| v.is_finite() && (1.0..=100.0).contains(v))
        .ok_or((BudgetField::Share, "Enter a share from 1 to 100%."))?;
    let min_score_pct = score
        .trim()
        .parse::<u8>()
        .ok()
        .filter(|v| *v <= 100)
        .ok_or((BudgetField::MinScore, "Enter a percentile from 0 to 100."))?;
    let whole = |v: &str, field| {
        v.trim()
            .parse::<u32>()
            .map_err(|_| (field, "Enter a whole number, 0 or more."))
    };
    Ok(Budget {
        share_of_window: share / 100.0,
        min_score_pct,
        images_max: whole(images, BudgetField::Images)?,
        passages_min: whole(min, BudgetField::PassagesMin)?,
    })
}

/// `cfg` with its stages, prompts and budget back to the defaults; the model slots (its
/// routes) are kept (an eval variant's override), and so is the embedder (the service's).
pub fn reset_pipeline(cfg: &PipelineConfig) -> PipelineConfig {
    PipelineConfig {
        routes: cfg.routes.clone(),
        embedder: cfg.embedder.clone(),
        ..PipelineConfig::default()
    }
}

/// The pipeline's stages as Settings lists them: (label, value).
pub fn pipeline_rows(cfg: &PipelineConfig) -> Vec<(&'static str, String)> {
    vec![
        ("Chunker", cfg.chunker.name.clone()),
        ("Embedder", cfg.embedder.name.clone()),
        ("Retriever", cfg.retriever.name.clone()),
        ("Fusion", cfg.fusion.name.clone()),
        ("Reranker", cfg.reranker.name.clone()),
        ("Packer", cfg.packer.name.clone()),
        ("Prompts", cfg.prompts.clone()),
        ("Fingerprint", cfg.hash()),
    ]
}

/// What a kind of stage (`STAGE_KINDS`) is called in Settings.
pub fn stage_label(kind: &str) -> &'static str {
    match kind {
        "chunker" => "Chunker",
        "embedder" => "Embedder",
        "retriever" => "Retriever",
        "fusion" => "Fusion",
        "reranker" => "Reranker",
        "packer" => "Packer",
        "prompts" => "Prompts",
        _ => "Stage",
    }
}

/// What a kind of stage does, under its picker.
pub fn stage_help(kind: &str) -> &'static str {
    match kind {
        "chunker" => {
            "How sources are cut into passages. A new chunker applies as sources are read again."
        }
        "embedder" => {
            "The model passages are embedded with. It is set in the service's config (embedder in service.toml), not here; changing it there re-indexes every source once."
        }
        "retriever" => "How passages are found: by meaning, by words, or both.",
        "fusion" => "How the meaning and word results are merged into one list.",
        "reranker" => "Whether the merged list is reordered before packing.",
        "packer" => "How the best passages are chosen to fit the budget below.",
        "prompts" => "The version of the instructions lessons and outlines are written with.",
        _ => "",
    }
}

/// The named variant whose stages and prompts `cfg` has (routes and budget aside).
pub fn current_variant<'a>(
    cfg: &PipelineConfig,
    variants: &'a PipelineVariants,
) -> Option<&'a NamedVariant> {
    variants
        .variants
        .iter()
        .find(|v| cfg.has_stages_of(&v.config))
}

/// The choices of one kind's picker, (value, label): the names the service registers,
/// and `current` too when it doesn't (so the picker shows what is set).
pub fn stage_options(
    kind: &str,
    current: &str,
    variants: &PipelineVariants,
) -> Vec<(String, String)> {
    let known = variants.stages.get(kind).cloned().unwrap_or_default();
    let mut out: Vec<(String, String)> = known.iter().map(|n| (n.clone(), n.clone())).collect();
    if !known.iter().any(|n| n == current) {
        out.insert(
            0,
            (
                current.to_string(),
                format!("{current} (not on this service)"),
            ),
        );
    }
    out
}

/// A stage-only fingerprint of `cfg` (routes and budget left out), so the stage picker
/// starts over when the stages change elsewhere but not when the budget or a model slot
/// is saved.
fn stages_key(cfg: &PipelineConfig) -> String {
    PipelineConfig::default().with_stages_of(cfg).hash()
}

/// What saving the stage picker sends: the saved pipeline (its routes and budget as they
/// are now) with `draft`'s stages and prompts; `None` when that changes nothing.
pub fn stages_patch(saved: &PipelineConfig, draft: &PipelineConfig) -> Option<SettingsPatch> {
    let pipeline = saved.with_stages_of(draft);
    (pipeline != *saved).then(|| SettingsPatch {
        pipeline: Some(pipeline),
        ..Default::default()
    })
}

/// The stages of the pipeline: read-only until the service's choices load, then a picker
/// of named variants and of each stage (`GET /api/pipeline/variants`).
#[component]
fn StagePicker(cfg: PipelineConfig) -> Element {
    let app = use_app();
    let mut fetch = use_resource(move || async move {
        let api = app.api().ok_or_else(|| "Not signed in.".to_string())?;
        match api.pipeline_variants().await {
            Ok(v) => {
                let mut lib = app.lib;
                lib.write().pipeline_variants = Some(v);
                Ok(())
            }
            Err(e) => Err(app.failure(&e)),
        }
    });
    let failed = match &*fetch.read() {
        Some(Err(e)) => Some(e.clone()),
        _ => None,
    };
    let variants = app.lib.read().pipeline_variants.clone();
    let saved_note = use_signal(|| false);
    let rows = pipeline_rows(&cfg);
    rsx! {
        match variants {
            Some(variants) => rsx! {
                if let Some(err) = failed {
                    ErrorNotice { message: format!("Couldn't check the service's stages again: {err}"), on_retry: move |_| fetch.restart() }
                }
                div {
                    StageForm { key: "{stages_key(&cfg)}", cfg: cfg.clone(), variants, saved_note }
                }
                div { class: "kv", "aria-label": "Fingerprint",
                    span { "Fingerprint" }
                    span { class: "mono", "{cfg.hash()}" }
                }
            },
            None => rsx! {
                div { class: "kv", "aria-label": "Stages",
                    for (label, value) in rows {
                        span { key: "{label}", "{label}" }
                        span { class: "mono", "{value}" }
                    }
                }
                match failed {
                    Some(err) => rsx! { ErrorNotice { message: err, on_retry: move |_| fetch.restart() } },
                    None => rsx! { div { class: "skel", style: "width: 50%", "aria-label": "Loading the stages" } },
                }
            },
        }
    }
}

/// The variant and stage pickers over a draft of `cfg`'s stages; Save sends them.
#[component]
fn StageForm(cfg: PipelineConfig, variants: PipelineVariants, saved_note: Signal<bool>) -> Element {
    let app = use_app();
    let mut draft = use_signal(|| cfg.clone());
    let error = use_signal(|| None::<String>);
    let busy = use_signal(|| false);
    let d = draft.read().clone();
    let dirty = !cfg.has_stages_of(&d);
    let current = current_variant(&d, &variants).map(|v| v.name.clone());
    let named = variants.variants.clone();
    rsx! {
        form {
            style: "display: grid; gap: 14px",
            novalidate: true,
            onsubmit: move |e: FormEvent| {
                e.prevent_default();
                async move {
                    if *busy.peek() {
                        return;
                    }
                    let Some(settings) = app.lib.peek().settings.clone() else { return };
                    let Some(patch) = stages_patch(&settings.pipeline, &draft.peek()) else {
                        return;
                    };
                    if save_settings(app, patch, error, busy).await {
                        saved_note.set(true);
                    }
                }
            },
            div { class: "field",
                label { r#for: "pl-variant", "Variant" }
                select {
                    id: "pl-variant",
                    class: "select-input",
                    "aria-describedby": "pl-variant-help",
                    onchange: {
                        let named = named.clone();
                        move |e: FormEvent| {
                            let name = e.value();
                            if let Some(v) = named.iter().find(|v| v.name == name && v.unavailable.is_none()) {
                                let next = draft.peek().with_stages_of(&v.config);
                                draft.set(next);
                                saved_note.set(false);
                            }
                        }
                    },
                    if current.is_none() {
                        option { value: "", selected: true, "Custom" }
                    }
                    for v in named.iter() {
                        option {
                            key: "{v.name}",
                            value: "{v.name}",
                            selected: current.as_deref() == Some(v.name.as_str()),
                            disabled: v.unavailable.is_some(),
                            title: v.unavailable.clone().unwrap_or_default(),
                            if v.unavailable.is_some() { "{v.name} (not on this service)" } else { "{v.name}" }
                        }
                    }
                }
                span { id: "pl-variant-help", class: "faint", style: "font-size: 13px",
                    "Sets every stage at once, as the evals compare them. The models above and the budget below stay."
                }
            }
            div { class: "grid2",
                for kind in STAGE_KINDS {
                    if kind == "embedder" {
                        div { key: "{kind}", class: "field",
                            span { class: "label", {stage_label(kind)} }
                            span {
                                id: "pl-stage-{kind}",
                                class: "mono",
                                "aria-describedby": "pl-stage-{kind}-help",
                                {d.stage_name(kind).unwrap_or_default().to_string()}
                            }
                            span { id: "pl-stage-{kind}-help", class: "faint", style: "font-size: 13px", {stage_help(kind)} }
                        }
                    } else {
                    div { key: "{kind}", class: "field",
                        label { r#for: "pl-stage-{kind}", {stage_label(kind)} }
                        select {
                            id: "pl-stage-{kind}",
                            class: "select-input",
                            "aria-describedby": "pl-stage-{kind}-help",
                            onchange: move |e: FormEvent| {
                                let next = draft.peek().with_stage(kind, &e.value());
                                draft.set(next);
                                saved_note.set(false);
                            },
                            for (value, label) in stage_options(kind, d.stage_name(kind).unwrap_or_default(), &variants) {
                                option {
                                    key: "{value}",
                                    value: "{value}",
                                    selected: d.stage_name(kind) == Some(value.as_str()),
                                    "{label}"
                                }
                            }
                        }
                        span { id: "pl-stage-{kind}-help", class: "faint", style: "font-size: 13px", {stage_help(kind)} }
                    }
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
                    SaveLabel { busy: *busy.read(), label: "Save stages".to_string() }
                }
                button {
                    r#type: "button",
                    class: "btn btn-secondary btn-sm",
                    disabled: !dirty || *busy.read(),
                    onclick: {
                        let cfg = cfg.clone();
                        move |_| draft.set(cfg.clone())
                    },
                    "Undo changes"
                }
                if *saved_note.read() && !dirty {
                    span { class: "save-state", role: "status", "Saved" }
                }
            }
        }
    }
}

/// The retrieval pipeline: its stages, its budget (editable), and Reset to default.
#[component]
fn PipelineSection(settings: Settings) -> Element {
    let app = use_app();
    let cfg = settings.pipeline.clone();
    let initial = budget_fields(&cfg.budget);
    let mut fields = use_signal(|| initial.clone());
    // Bumped when the fields are set by code, to remount them with the new numbers.
    let mut rev = use_signal(|| 0u64);
    let mut problem = use_signal(|| None::<(BudgetField, &'static str)>);
    let error = use_signal(|| None::<String>);
    let busy = use_signal(|| false);
    let mut saved_note = use_signal(|| false);
    let mut confirm_reset = use_signal(|| false);
    let reset_error = use_signal(|| None::<String>);
    let reset_busy = use_signal(|| false);
    let dirty = *fields.read() != initial;
    let is_default = reset_pipeline(&cfg) == cfg;

    rsx! {
        section { class: "set-sec",
            h2 { "Pipeline" }
            p { class: "help", "How the service finds passages in your sources and packs them for lessons, outlines and the agent's search. The budget is a share of each model's context window, not a fixed size." }
            StagePicker { cfg: cfg.clone() }
            form {
                style: "display: grid; gap: 14px",
                novalidate: true,
                onsubmit: {
                    let cfg = cfg.clone();
                    move |e: FormEvent| {
                        e.prevent_default();
                        let cfg = cfg.clone();
                        async move {
                            if *busy.peek() {
                                return;
                            }
                            let budget = match parse_budget(&fields.peek()) {
                                Ok(b) => b,
                                Err((field, message)) => {
                                    problem.set(Some((field, message)));
                                    focus_first(&format!("#{}", field.id()));
                                    return;
                                }
                            };
                            problem.set(None);
                            if budget == cfg.budget {
                                return;
                            }
                            let patch = SettingsPatch {
                                pipeline: Some(PipelineConfig { budget, ..cfg }),
                                ..Default::default()
                            };
                            if save_settings(app, patch, error, busy).await {
                                // The numbers as the service keeps them.
                                fields.set(budget_fields(&budget));
                                rev += 1;
                                saved_note.set(true);
                            }
                        }
                    }
                },
                div { class: "grid2",
                    for (i, field) in BudgetField::ALL.into_iter().enumerate() {
                        div { key: "{field.id()}-{rev}", class: "field",
                            label { r#for: "{field.id()}", {field.label()} }
                            div { class: "row-actions", style: "flex-wrap: nowrap",
                                input {
                                    id: "{field.id()}",
                                    class: "input",
                                    style: "flex: 1; min-width: 80px",
                                    r#type: "number",
                                    inputmode: if field == BudgetField::Share { "decimal" } else { "numeric" },
                                    min: field.range().0,
                                    max: field.range().1,
                                    step: if field == BudgetField::Share { "any" } else { "1" },
                                    "aria-invalid": problem.read().is_some_and(|(f, _)| f == field).to_string(),
                                    "aria-describedby": "{field.id()}-help",
                                    initial_value: "{fields.peek()[i]}",
                                    oninput: move |e| {
                                        saved_note.set(false);
                                        problem.set(None);
                                        fields.write()[i] = e.value();
                                    },
                                }
                                if let Some(unit) = field.unit() {
                                    span { class: "faint", "{unit}" }
                                }
                            }
                            match *problem.read() {
                                Some((f, message)) if f == field => rsx! {
                                    span { id: "{field.id()}-help", class: "field-error", "{message}" }
                                },
                                _ => rsx! {
                                    span { id: "{field.id()}-help", class: "faint", style: "font-size: 13px", {field.help()} }
                                },
                            }
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
                    button {
                        r#type: "button",
                        class: "btn btn-secondary btn-sm",
                        disabled: is_default || *reset_busy.read(),
                        title: if is_default { "The pipeline is at its defaults" } else { "" },
                        onclick: move |_| confirm_reset.set(true),
                        "Reset to default…"
                    }
                    if *saved_note.read() && !dirty {
                        span { class: "save-state", role: "status", "Saved" }
                    }
                }
                if let Some(err) = reset_error.read().clone() {
                    span { class: "save-state error", role: "alert", "{err}" }
                }
            }
        }
        if *confirm_reset.read() {
            ConfirmDialog {
                title: "Reset the pipeline to its defaults?",
                message: "Its stages, prompts and budget go back to Margin's defaults. The models chosen above stay. Lessons written from now on use the defaults.",
                confirm_label: "Reset",
                danger: false,
                busy: *reset_busy.read(),
                on_cancel: move |_| confirm_reset.set(false),
                on_confirm: {
                    let cfg = cfg.clone();
                    move |_| {
                        let cfg = cfg.clone();
                        async move {
                            let patch = SettingsPatch {
                                pipeline: Some(reset_pipeline(&cfg)),
                                ..Default::default()
                            };
                            if save_settings(app, patch, reset_error, reset_busy).await {
                                fields.set(budget_fields(&Budget::default()));
                                problem.set(None);
                                rev += 1;
                            }
                            confirm_reset.set(false);
                        }
                    }
                },
            }
        }
    }
}

/// The number of days the retention field holds, or why it can't be saved.
pub fn parse_retention(input: &str) -> Result<u32, &'static str> {
    input
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|d| (1..=3650).contains(d))
        .ok_or("Enter a number of days from 1 to 3650.")
}

/// Settings → Advanced: how long the service keeps its trace rows.
#[component]
fn TracesSection(settings: Settings) -> Element {
    let app = use_app();
    let current = settings.trace_retention_days;
    let mut days = use_signal(|| current.to_string());
    let mut problem = use_signal(|| None::<&'static str>);
    let error = use_signal(|| None::<String>);
    let busy = use_signal(|| false);
    let mut saved_note = use_signal(|| false);
    let dirty = days.read().trim() != current.to_string();

    rsx! {
        section { class: "set-sec",
            h2 { "Traces" }
            p { class: "help", "The service records a trace of every model call, search, tool call and job: when, how long, which model, tokens and cost, never what was said. Evaluations and reports outside the app read them. Each day, traces older than this are deleted; the hourly and daily totals stay." }
            form {
                class: "field",
                novalidate: true,
                onsubmit: move |e: FormEvent| async move {
                    e.prevent_default();
                    if *busy.peek() {
                        return;
                    }
                    let d = match parse_retention(&days.peek()) {
                        Ok(d) => d,
                        Err(message) => {
                            problem.set(Some(message));
                            focus_first("#trace-days");
                            return;
                        }
                    };
                    problem.set(None);
                    if d == current {
                        return;
                    }
                    let patch = SettingsPatch { trace_retention_days: Some(d), ..Default::default() };
                    if save_settings(app, patch, error, busy).await {
                        saved_note.set(true);
                    }
                },
                label { r#for: "trace-days", "Keep traces for" }
                div { class: "row-actions", style: "justify-content: flex-start",
                    input {
                        id: "trace-days",
                        class: "input",
                        style: "width: 110px",
                        r#type: "number",
                        inputmode: "numeric",
                        min: "1",
                        max: "3650",
                        step: "1",
                        "aria-invalid": problem.read().is_some().to_string(),
                        "aria-describedby": "trace-days-help",
                        initial_value: "{days}",
                        oninput: move |e| {
                            saved_note.set(false);
                            problem.set(None);
                            days.set(e.value());
                        },
                    }
                    span { "days" }
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
                match *problem.read() {
                    Some(message) => rsx! {
                        span { id: "trace-days-help", class: "field-error", "{message}" }
                    },
                    None => rsx! {
                        span { id: "trace-days-help", class: "faint", style: "font-size: 13px", "From 1 to 3650 days; {TRACE_RETENTION_DAYS} unless changed." }
                    },
                }
                if let Some(err) = error.read().clone() {
                    span { class: "save-state error", role: "alert", "{err}" }
                }
            }
        }
    }
}

/// What a member reads where an admin has the model's Download and Remove controls.
pub const MODELS_ADMIN_ONLY: &str =
    "Only an organisation admin downloads or removes the models the service runs.";

/// The embedding model sources are read with: its state, download progress, and removal.
#[component]
fn SourcesSection() -> Element {
    let app = use_app();
    let mut load = use_resource(move || async move { refresh_embedding(app).await });
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let mut confirm_remove = use_signal(|| false);
    let model = app.lib.read().embedding.clone();
    // The models are the organisation's: a member reads their state, an admin changes it
    // (the service answers 403 `admin_required` otherwise).
    let admin = app.is_admin();
    let download = move || async move {
        busy.set(true);
        error.set(download_embedding(app).await.err());
        busy.set(false);
    };

    rsx! {
        section { class: "set-sec",
            h2 { "Embedding model" }
            p { class: "help",
                "EmbeddingGemma 2 runs on the service. It reads your sources ({SOURCE_KINDS}) and finds the passages, pages, pictures and clips that outlines and lessons draw on."
            }
            match (model, load.read().clone()) {
                (Some(model), _) => rsx! {
                    div { class: "kv",
                        span { "Model" }
                        span { class: "mono", "{model.repo}" }
                        span { "Runs on" }
                        span { {model_runtime(&model.repo)} }
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
                        EmbeddingModelState::NotDownloaded { download_bytes } if admin => rsx! {
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
                        EmbeddingModelState::Ready { .. } if admin => rsx! {
                            div { class: "row-actions",
                                button { class: "btn btn-secondary btn-sm", onclick: move |_| confirm_remove.set(true), "Remove…" }
                            }
                        },
                        EmbeddingModelState::Failed { error: message } if admin => rsx! {
                            ErrorNotice {
                                message: "The download failed. {message}",
                                on_retry: move |_| { spawn(download()); },
                            }
                        },
                        EmbeddingModelState::Failed { error: message } => rsx! {
                            p { class: "help", "The download failed. {message} {MODELS_ADMIN_ONLY}" }
                        },
                        _ => rsx! {
                            p { class: "help", "{MODELS_ADMIN_ONLY}" }
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
            if let Some(progress) = app.lib.read().embedding.as_ref().and_then(|m| m.reindex) {
                ReindexLine { progress }
            }
        }
        if let Some(speech) = app.lib.read().embedding.as_ref().and_then(|m| m.speech.clone()) {
            SpeechModelSection { speech }
        }
        if *confirm_remove.read() {
            ConfirmDialog {
                title: "Remove the embedding model?",
                message: "Its files are deleted from the service. New sources wait in the queue until you download it again.",
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

/// The one pass that re-embeds every source after the service's embedder changed.
#[component]
fn ReindexLine(progress: ReindexProgress) -> Element {
    let ReindexProgress { done, total } = progress;
    let pct = (done.min(total) * 100).checked_div(total).unwrap_or(0);
    rsx! {
        div { class: "progress", "aria-label": "Re-indexing",
            div {
                class: "bar wide",
                role: "progressbar",
                "aria-label": "Re-indexing the sources with the new embedder",
                "aria-valuemin": "0",
                "aria-valuemax": "100",
                "aria-valuenow": "{pct}",
                "aria-valuetext": "{done} of {total} documents",
                i { style: "width: {pct}%" }
            }
            span { class: "faint", "Re-indexing {done} of {total} documents with the new embedder. Until a document is done, search finds it by its words." }
        }
    }
}

/// Shows the model a request answered with, or why it failed.
fn apply_model(
    app: crate::state::AppCtx,
    mut error: Signal<Option<String>>,
    result: crate::api::ApiResult<EmbeddingModel>,
) {
    match result {
        Ok(model) => {
            let mut lib = app.lib;
            lib.write().apply(Event::EmbeddingModelChanged { model });
            error.set(None);
        }
        Err(e) => error.set(Some(app.failure(&e))),
    }
}

/// The speech model a Linux service transcribes recordings with: its state, download
/// progress, and removal (`EmbeddingModel.speech`; a Mac service has none).
#[component]
fn SpeechModelSection(speech: SpeechModel) -> Element {
    let app = use_app();
    let mut busy = use_signal(|| false);
    let error = use_signal(|| None::<String>);
    let mut confirm_remove = use_signal(|| false);
    // Admins download and remove it; a member reads its state.
    let admin = app.is_admin();
    let download = move || async move {
        let Some(api) = app.api() else { return };
        busy.set(true);
        apply_model(app, error, api.download_speech().await);
        busy.set(false);
    };
    rsx! {
        section { class: "set-sec",
            h2 { "Speech model" }
            p { class: "help",
                "Parakeet runs on the service and writes the transcripts of audio and video sources, word by word, so lessons can quote what's said. Without it, recordings are read without a transcript."
            }
            div { class: "kv",
                span { "Model" }
                span { class: "mono", "{speech.repo}" }
                span { "Runs on" }
                span { "ONNX Runtime, on the CPU" }
                span { "Status" }
                span {
                    match &speech.state {
                        EmbeddingModelState::NotDownloaded { .. } => "Not downloaded".to_string(),
                        EmbeddingModelState::Downloading { .. } => "Downloading…".to_string(),
                        EmbeddingModelState::Ready { bytes } => format!("Ready · {} on disk", human_bytes(*bytes)),
                        EmbeddingModelState::Failed { .. } => "Download failed".to_string(),
                    }
                }
            }
            match speech.state.clone() {
                EmbeddingModelState::NotDownloaded { download_bytes } if admin => rsx! {
                    div { class: "row-actions",
                        button {
                            class: "btn btn-primary btn-sm",
                            disabled: *busy.read(),
                            onclick: move |_| download(),
                            if *busy.read() { span { class: "spinner" } }
                            "Download · {human_bytes(download_bytes)}"
                        }
                        span { class: "faint", style: "font-size: 13px", "Downloads once, from Hugging Face. Recordings read before it's there can be read again after." }
                    }
                },
                EmbeddingModelState::Downloading { received, total } => rsx! {
                    Progress { received, total, label: "Downloading the speech model".to_string() }
                },
                EmbeddingModelState::Ready { .. } if admin => rsx! {
                    div { class: "row-actions",
                        button { class: "btn btn-secondary btn-sm", onclick: move |_| confirm_remove.set(true), "Remove…" }
                    }
                },
                EmbeddingModelState::Failed { error: message } if admin => rsx! {
                    ErrorNotice {
                        message: "The download failed. {message}",
                        on_retry: move |_| { spawn(download()); },
                    }
                },
                EmbeddingModelState::Failed { error: message } => rsx! {
                    p { class: "help", "The download failed. {message} {MODELS_ADMIN_ONLY}" }
                },
                _ => rsx! {
                    p { class: "help", "{MODELS_ADMIN_ONLY}" }
                },
            }
            if let Some(err) = error.read().clone() {
                span { class: "save-state error", "{err}" }
            }
        }
        if *confirm_remove.read() {
            ConfirmDialog {
                title: "Remove the speech model?",
                message: "Its files are deleted from the service. Recordings are read without a transcript until you download it again.",
                confirm_label: "Remove model",
                busy: *busy.read(),
                on_cancel: move |_| confirm_remove.set(false),
                on_confirm: move |_| async move {
                    let Some(api) = app.api() else { return };
                    busy.set(true);
                    apply_model(app, error, api.delete_speech().await);
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

    let me = app.session.read().as_ref().and_then(|s| {
        s.me.as_ref()
            .and_then(|m| m.device.as_ref())
            .map(|d| d.id.clone())
    });
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
                    FolderFiles { workspace_id: ws.id.clone(), unreadable: ws.unreadable.clone() }
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
/// be read can be read again. Files the holding device couldn't read on its last pass
/// (`Workspace::unreadable`) are greyed out with the reason, whether or not the service
/// has a copy of them.
#[component]
fn FolderFiles(workspace_id: Id, unreadable: Vec<UnreadableFile>) -> Element {
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
                Some(files) if files.is_empty() && unreadable.is_empty() => rsx! {
                    p { class: "faint", "The service has none of this folder's files yet." }
                },
                Some(files) => rsx! {
                    div { role: "list", "aria-label": "Files of the folder",
                        for f in unreadable.iter().filter(|u| !files.iter().any(|f| f.name == u.path)).cloned() {
                            div {
                                key: "unreadable-{f.path}",
                                role: "listitem",
                                class: "unreadable",
                                "aria-disabled": "true",
                                div { class: "src-row",
                                    Icon {
                                        name: kind_icon(SourceKind::for_name(&f.path).unwrap_or(SourceKind::Text)),
                                        class: "src-ic",
                                    }
                                    div { class: "src-main",
                                        span { class: "src-name", title: "{f.path}", "{f.path}" }
                                        span { class: "src-meta", "{f.reason.message()}" }
                                    }
                                }
                            }
                        }
                        for f in files {
                            div {
                                key: "{f.id}",
                                role: "listitem",
                                class: if unreadable.iter().any(|u| u.path == f.name) { "unreadable" },
                                "aria-disabled": unreadable.iter().any(|u| u.path == f.name).then_some("true"),
                                {
                                    let (tone, status) = match unreadable.iter().find(|u| u.path == f.name) {
                                        Some(u) => (Tone::Waiting, u.reason.message().to_string()),
                                        None => status_text(&f.status),
                                    };
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
                h2 { "Tools on the service" }
                button {
                    class: "btn btn-ghost btn-sm",
                    onclick: move |_| tools.restart(),
                    Icon { name: "refresh" }
                    "Check again"
                }
            }
            p { class: "help", "The programs the service runs to render videos, voice their narration and read sources. Each one says what it does; a missing one turns off only that part. They run on the service machine, and Margin never installs them for you." }
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

/// The runtime the service's embedding model repo is for: the Core ML conversion runs on
/// a Mac's Neural Engine, the ONNX export on the CPU with ONNX Runtime (a Linux service).
fn model_runtime(repo: &str) -> &'static str {
    if repo.to_ascii_lowercase().contains("onnx") {
        "ONNX Runtime, on the CPU"
    } else {
        "Core ML, on the Neural Engine and GPU"
    }
}

/// What a tool the service reports (Settings → Tools) is for, by its name.
fn tool_purpose(name: &str) -> Option<&'static str> {
    Some(match name {
        "manimgx" => "Renders lesson videos.",
        "fluidaudiocli" => {
            "Voices video narration and transcribes audio and video sources; without it, recordings are read without a transcript."
        }
        "margin-tts" => "Voices video narration.",
        "tesseract" => {
            "Reads text in images and scanned PDF pages; without it, they are read without their text."
        }
        "ffmpeg" => {
            "Reads the sound and frames of audio and video sources (with ffprobe, from the same package)."
        }
        "bwrap" => {
            "Runs the scene code of a video in a sandbox with no network; without it, videos don't render."
        }
        "pdfium" => "Reads PDF text and draws PDF pages.",
        _ => return None,
    })
}

#[component]
fn ToolRow(tool: ToolStatus) -> Element {
    let mut copied = use_signal(|| None::<Result<(), String>>);
    // Bumped per copy, so only the latest one's timer resets the label.
    let mut copies = use_signal(|| 0u64);
    let commands = tool.install_commands.join("\n");
    let purpose = tool_purpose(&tool.name);
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
            if let Some(purpose) = purpose {
                span { class: "muted", style: "font-size: 13px", "{purpose}" }
            }
            if !tool.found && !tool.install_commands.is_empty() {
                span { class: "muted", style: "font-size: 13px", "Install it on the service machine with:" }
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
        // Files the Mac couldn't read on its last pass: greyed out, with the reason; one
        // the service has a copy of, one it never received.
        assert!(html.contains("figure.png"));
        assert!(
            html.contains("Not on this device — download it to include it"),
            "{html}"
        );
        assert!(html.contains("drafts/locked.md"));
        assert!(html.contains("Can&#39;t be read"));
        assert_eq!(html.matches(r#"class="unreadable""#).count(), 2, "{html}");
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
        assert!(html.contains(r#"value="gpt-5""#));
        for e in ReasoningEffort::ALL {
            assert!(html.contains(effort_label(e)));
        }
    }

    fn ai_pane(settings: Settings) -> String {
        use crate::store::LibraryState;
        let mut lib = LibraryState::from_library(crate::fake::sample_library());
        lib.settings = Some(settings);
        crate::ui::test_render::render_with_settings("/", lib, Some(SettingsPane::Ai))
    }

    #[test]
    fn sources_pane_shows_the_speech_model_and_the_reindex_where_the_service_has_them() {
        use crate::store::LibraryState;
        let sources = |model: EmbeddingModel| {
            let mut lib = LibraryState::from_library(crate::fake::sample_library());
            lib.embedding = Some(model);
            crate::ui::test_render::render_with_settings("/", lib, Some(SettingsPane::Sources))
        };
        let mac = EmbeddingModel {
            repo: crate::fake::EMBEDDING_REPO.into(),
            state: EmbeddingModelState::Ready { bytes: 1_000 },
            speech: None,
            reindex: None,
        };
        let html = sources(mac.clone());
        assert!(html.contains("<h2>Embedding model</h2>"), "{html}");
        assert!(!html.contains("Speech model"));
        assert!(!html.contains("Re-indexing"));

        let linux = EmbeddingModel {
            speech: Some(SpeechModel {
                repo: crate::fake::SPEECH_REPO.into(),
                state: EmbeddingModelState::NotDownloaded {
                    download_bytes: crate::fake::SPEECH_DOWNLOAD_BYTES,
                },
            }),
            reindex: Some(ReindexProgress {
                done: 212,
                total: 1480,
            }),
            ..mac
        };
        let html = sources(linux.clone());
        assert!(html.contains("<h2>Speech model</h2>"), "{html}");
        assert!(html.contains(crate::fake::SPEECH_REPO));
        let size = human_bytes(crate::fake::SPEECH_DOWNLOAD_BYTES);
        assert!(html.contains(&format!("Download · {size}")), "{html}");
        assert!(html.contains("Re-indexing 212 of 1480 documents"), "{html}");
        assert!(html.contains(r#"aria-valuenow="14""#), "{html}");
        let downloading = EmbeddingModel {
            speech: Some(SpeechModel {
                repo: crate::fake::SPEECH_REPO.into(),
                state: EmbeddingModelState::Downloading {
                    received: 335_000_000,
                    total: crate::fake::SPEECH_DOWNLOAD_BYTES,
                },
            }),
            ..linux.clone()
        };
        assert!(sources(downloading).contains(r#"aria-label="Downloading the speech model""#));
        let ready = EmbeddingModel {
            speech: Some(SpeechModel {
                repo: crate::fake::SPEECH_REPO.into(),
                state: EmbeddingModelState::Ready { bytes: 670_000_000 },
            }),
            ..linux
        };
        let html = sources(ready);
        assert!(
            html.contains(&format!("Ready · {}", human_bytes(670_000_000))),
            "{html}"
        );
        assert_eq!(html.matches("Remove…").count(), 2, "{html}");
    }

    #[test]
    fn ai_pane_has_three_model_slots_per_provider() {
        let mut settings = crate::fake::FakeState::new().settings;
        settings.openai_models.chat = "gpt-6.1-sol".into();
        let html = ai_pane(settings.clone());
        for (provider, heading) in [
            (Provider::OpenAi, "<h2>OpenAI</h2>"),
            (Provider::ChatGpt, "<h2>ChatGPT</h2>"),
            (Provider::Claude, "<h2>Claude</h2>"),
        ] {
            assert!(html.contains(heading), "{heading}");
            assert!(html.contains(&format!(r#"id="{}""#, provider_block_id(provider))));
            for route in Route::ALL {
                let id = format!(r#"id="ai-slot-{}-{}""#, provider.as_str(), route.as_str());
                assert!(html.contains(&id), "{id}");
            }
        }
        for route in Route::ALL {
            assert!(html.contains(slot_label(route)));
            assert!(html.contains(&slot_help(route).replace('\'', "&#39;")));
        }
        // Each slot shows the model it names.
        assert!(html.contains(r#"value="gpt-6.1-sol""#));
        assert!(html.contains(r#"value="claude-opus-5-5""#));
        assert!(html.contains(r#"value="claude-haiku-5-5""#));
        // Without the lists: free text, no window, and what to set up to get them.
        assert!(!html.contains("<datalist"));
        assert!(!html.contains("slot-window"), "{html}");
        assert!(html.contains("Add a Claude API key to choose from Claude&#39;s models"));
        assert!(html.contains("Sign in with ChatGPT to choose from ChatGPT&#39;s models"));
        // The default provider, and the keys: OpenAI's saved, Claude's to add.
        assert!(html.contains(r#"aria-labelledby="ai-default-label""#));
        for p in Provider::ALL {
            assert!(html.contains(&format!(">{}<", p.label())));
        }
        assert!(html.contains(r#"aria-label="Replace the OpenAI key""#));
        assert!(html.contains(r#"id="ai-key-claude""#));
        assert!(html.contains(r#"placeholder="sk-ant-…""#));
        assert!(html.contains("never on this device"));
        assert!(html.contains(">Continue with ChatGPT<"));
        assert!(html.contains("You can cap Margin&#39;s share in ChatGPT → Settings → Usage."));
        assert!(!html.contains("Set it up below"));

        // A default provider that isn't set up says so.
        settings.default_provider = Provider::Claude;
        let html = ai_pane(settings.clone());
        assert!(html.contains("Add a Claude API key to use it."), "{html}");
        assert!(html.contains(">Set it up below<"));
    }

    #[test]
    fn slots_list_the_providers_models_with_their_windows_and_the_override() {
        use crate::store::LibraryState;
        let mut settings = crate::fake::FakeState::new().settings;
        settings.has_claude_api_key = true;
        settings.claude_models.small = "claude-instant-9".into();
        settings.pipeline.routes.write = Some("claude-sonnet-5-5".into());
        let mut lib = LibraryState::from_library(crate::fake::sample_library());
        lib.settings = Some(settings.clone());
        for p in [Provider::OpenAi, Provider::Claude] {
            lib.provider_models.insert(p, crate::fake::fake_models(p));
        }
        let html = crate::ui::test_render::render_with_settings("/", lib, Some(SettingsPane::Ai));
        // The lists, Claude's defaults first; the inputs pick from them.
        assert!(
            html.contains(r#"<datalist id="ai-models-claude">"#),
            "{html}"
        );
        assert!(html.contains(r#"list="ai-models-openai""#));
        let claude = &html[html.find(r#"<datalist id="ai-models-claude">"#).unwrap()..];
        let at = |id: &str| claude.find(&format!(r#"value="{id}""#)).unwrap();
        assert!(at("claude-opus-5-5") < at("claude-sonnet-5-5"));
        assert!(at("claude-sonnet-5-5") < at("claude-haiku-5-5"));
        assert!(at("claude-haiku-5-5") < at("claude-opus-4-1"));
        // Windows beside known models; the fallback named for the others.
        assert!(html.contains(">1M context<"), "{html}");
        assert!(html.contains(">window unknown, using 200k<"));
        // gpt-5 (OpenAI's writing slot here) has no window in the list.
        assert_eq!(settings.openai_models.write, "gpt-5");
        // The variant's route override, read-only under Claude's writing slot only.
        assert_eq!(
            html.matches("Overridden by the pipeline variant: ").count(),
            1,
            "{html}"
        );
        assert!(html.contains(r#"<span class="mono">claude-sonnet-5-5</span>"#));
    }

    #[test]
    fn slot_helpers() {
        assert_eq!(window_text(1_050_000), "1M context");
        assert_eq!(window_text(1_500_000), "1.5M context");
        assert_eq!(window_text(200_000), "200k context");
        assert_eq!(window_text(512), "512 context");
        let models = crate::fake::fake_models(Provider::OpenAi);
        assert_eq!(window_note(None, "gpt-6-luna"), None);
        assert_eq!(window_note(Some(&models), " "), None);
        assert_eq!(
            window_note(Some(&models), "gpt-6-luna").as_deref(),
            Some("1M context")
        );
        assert_eq!(
            window_note(Some(&models), "gpt-5").as_deref(),
            Some(WINDOW_UNKNOWN)
        );
        assert_eq!(
            window_note(Some(&models), "mine").as_deref(),
            Some(WINDOW_UNKNOWN)
        );
        // An override applies to the provider whose model it names.
        assert!(model_belongs(Provider::Claude, "claude-x", None));
        assert!(!model_belongs(Provider::OpenAi, "claude-x", None));
        assert!(model_belongs(Provider::ChatGpt, "gpt-6", None));
        assert!(!model_belongs(Provider::OpenAi, "gpt-7", Some(&models)));
        let mut settings = crate::fake::FakeState::new().settings;
        settings.pipeline.routes.small = Some("gpt-6-luna".into());
        assert_eq!(
            slot_override(&settings, Provider::OpenAi, Route::Small, Some(&models)).as_deref(),
            Some("gpt-6-luna")
        );
        assert_eq!(
            slot_override(&settings, Provider::Claude, Route::Small, None),
            None
        );
        assert_eq!(
            slot_override(&settings, Provider::OpenAi, Route::Chat, None),
            None
        );
        // Patches.
        let m = RouteModels::claude_default();
        assert_eq!(
            models_patch(Provider::Claude, m.clone()).claude_models,
            Some(m.clone())
        );
        assert_eq!(
            models_patch(Provider::ChatGpt, m.clone()).chatgpt_models,
            Some(m.clone())
        );
        assert_eq!(
            models_patch(Provider::OpenAi, m.clone()).openai_models,
            Some(m)
        );
        assert_eq!(
            key_patch(Provider::Claude, "k".into())
                .anthropic_api_key
                .as_deref(),
            Some("k")
        );
        assert_eq!(
            key_patch(Provider::OpenAi, String::new())
                .openai_api_key
                .as_deref(),
            Some("")
        );
    }

    #[test]
    fn chatgpt_block_in_every_state() {
        use crate::store::LibraryState;
        let pane = |status: Option<ChatGptStatus>, signed_in: bool| {
            let mut settings = crate::fake::FakeState::new().settings;
            if signed_in {
                settings.chatgpt = Some(crate::fake::fake_account("owner@example.com"));
            }
            let mut lib = LibraryState::from_library(crate::fake::sample_library());
            lib.settings = Some(settings);
            lib.chatgpt = status;
            crate::ui::test_render::render_with_settings("/", lib, Some(SettingsPane::Ai))
        };
        // Signed out (as the settings say before the status arrives).
        let html = pane(None, false);
        assert!(html.contains(">Continue with ChatGPT<"));
        assert!(html.contains("in a browser on the Mac that runs Margin&#39;s service"));
        // Waiting: the link, to open on the service's Mac, with Copy and Cancel.
        let url = "https://auth.openai.com/api/accounts/authorize?state=st-1";
        let html = pane(
            Some(ChatGptStatus::Waiting {
                authorize_url: url.into(),
            }),
            false,
        );
        assert!(
            html.contains("Waiting for you to finish signing in…"),
            "{html}"
        );
        assert!(html.contains(url));
        assert!(html.contains(r#"aria-label="Copy the sign-in link""#));
        assert!(html.contains(">Cancel<"));
        if OPENS_SIGN_IN_HERE {
            assert!(html.contains(">Open in browser<"));
        } else {
            assert!(html.contains("Open this link on the Mac that runs Margin&#39;s service:"));
        }
        // Signed in: the account, when, Sign out; the slots on the account's models.
        let html = pane(None, true);
        assert!(html.contains("<b>owner@example.com</b>"), "{html}");
        assert!(html.contains(" · Signed in "));
        assert!(html.contains(">Sign out…<"));
        assert!(!html.contains(">Continue with ChatGPT<"));
        assert!(!html.contains("Sign in with ChatGPT to choose"));
        assert!(html.contains(r#"value="gpt-6.1-sol""#));
        assert!(!html.contains("The latest sign-in attempt didn"));
        // Signed in after a failed attempt: still signed in, with the note.
        let html = pane(
            Some(ChatGptStatus::SignedIn {
                account: crate::fake::fake_account("owner@example.com"),
                last_error: Some("The owner declined.".into()),
            }),
            true,
        );
        assert!(html.contains(">Sign out…<"));
        assert!(
            html.contains("The latest sign-in attempt didn&#39;t work; the sign-in above still holds. The owner declined."),
            "{html}"
        );
        assert!(!html.contains(">Try again<"));
        // Failed: why, and Try again.
        let html = pane(
            Some(ChatGptStatus::Failed {
                message: "The sign-in was cancelled.".into(),
            }),
            false,
        );
        assert!(
            html.contains("Signing in with ChatGPT didn&#39;t work. The sign-in was cancelled."),
            "{html}"
        );
        assert!(html.contains(">Try again<"));
    }

    #[test]
    fn ai_pane_shows_the_pipeline_and_its_budget() {
        let mut settings = crate::fake::FakeState::new().settings;
        let html = ai_pane(settings.clone());
        assert!(html.contains("<h2>Pipeline</h2>"), "{html}");
        for (label, value) in pipeline_rows(&settings.pipeline) {
            assert!(html.contains(&format!(">{label}<")), "{label}");
            assert!(html.contains(&format!(">{value}<")), "{value}");
        }
        for field in BudgetField::ALL {
            assert!(html.contains(&field.label().replace('\'', "&#39;")));
            assert!(html.contains(&field.help().replace('\'', "&#39;")));
        }
        assert!(html.contains(r#"id="pl-share""#));
        assert!(html.contains(r#"value="50""#));
        // At the defaults, Reset has nothing to do.
        let reset = html.find("Reset to default…").unwrap();
        let button = &html[html[..reset].rfind("<button").unwrap()..reset];
        assert!(button.contains("disabled"), "{button}");
        // Changed: Reset is offered.
        settings.pipeline.budget.images_max = 2;
        let html = ai_pane(settings);
        assert!(html.contains(r#"value="2""#));
        let reset = html.find("Reset to default…").unwrap();
        let button = &html[html[..reset].rfind("<button").unwrap()..reset];
        assert!(!button.contains("disabled"), "{button}");
    }

    #[test]
    fn ai_pane_picks_the_pipelines_stages() {
        use crate::store::LibraryState;
        let variants = crate::fake::fake_pipeline_variants();
        let mut settings = crate::fake::FakeState::new().settings;
        let mut lib = LibraryState::from_library(crate::fake::sample_library());
        lib.settings = Some(settings.clone());
        lib.pipeline_variants = Some(variants.clone());
        let html =
            crate::ui::test_render::render_with_settings("/", lib.clone(), Some(SettingsPane::Ai));
        // The variant picker, at the default's "baseline"; one the service can't build is
        // offered but disabled.
        assert!(html.contains(r#"id="pl-variant""#), "{html}");
        assert!(
            html.contains(r#"<option value="baseline" selected"#),
            "{html}"
        );
        assert!(html.contains(r#"<option value="hybrid""#));
        let listwise = html.find(r#"<option value="listwise""#).unwrap();
        let tag = &html[listwise..listwise + html[listwise..].find('>').unwrap()];
        assert!(tag.contains("disabled"), "{tag}");
        assert!(html.contains("listwise (not on this service)"));
        assert!(!html.contains(">Custom<"));
        // A picker per stage with every registered name, the current one chosen.
        for kind in STAGE_KINDS {
            assert!(html.contains(&format!(r#"id="pl-stage-{kind}""#)), "{kind}");
            assert!(html.contains(&format!(">{}<", stage_label(kind))));
            assert!(html.contains(&stage_help(kind).replace('\'', "&#39;")));
            if kind == "embedder" {
                // The service's own, shown without a picker.
                assert!(!html.contains(r#"<select id="pl-stage-embedder""#));
                assert!(html.contains(&format!(">{}<", settings.pipeline.embedder.name)));
                continue;
            }
            for name in &variants.stages[kind] {
                assert!(
                    html.contains(&format!(r#"<option value="{name}""#)),
                    "{name}"
                );
            }
        }
        assert!(html.contains(r#"<option value="hybrid-rrf" selected"#));
        assert!(html.contains(&settings.pipeline.hash()));
        // Nothing picked yet: Save and Undo have nothing to do.
        let save = html.find("Save stages").unwrap();
        let button = &html[html[..save].rfind("<button").unwrap()..save];
        assert!(button.contains("disabled"), "{button}");

        // Stages no variant has: "Custom", and a name the service lacks is still shown.
        settings.pipeline = settings
            .pipeline
            .with_stage("packer", "take-k")
            .with_stage("reranker", "cross-v9");
        lib.settings = Some(settings.clone());
        let html = crate::ui::test_render::render_with_settings("/", lib, Some(SettingsPane::Ai));
        assert!(html.contains(r#"<option value="" selected"#), "{html}");
        assert!(html.contains(">Custom<"));
        assert!(html.contains(r#"<option value="take-k" selected"#));
        assert!(html.contains("cross-v9 (not on this service)"));
    }

    #[test]
    fn stage_choices_and_what_save_sends() {
        let variants = crate::fake::fake_pipeline_variants();
        let mut saved = PipelineConfig::default();
        saved.budget.images_max = 3;
        saved.routes.small = Some("gpt-6-luna".into());
        assert_eq!(
            current_variant(&saved, &variants).map(|v| v.name.as_str()),
            Some("baseline")
        );
        let hybrid = variants
            .variants
            .iter()
            .find(|v| v.name == "hybrid")
            .unwrap();
        let draft = saved.with_stages_of(&hybrid.config);
        assert_eq!(
            current_variant(&draft, &variants).map(|v| v.name.as_str()),
            Some("hybrid")
        );
        // Save sends the variant's stages (with their params) and keeps routes and budget.
        let patch = stages_patch(&saved, &draft).unwrap();
        let sent = patch.pipeline.unwrap();
        assert_eq!(sent.retriever, hybrid.config.retriever);
        assert_eq!(sent.retriever.params["dense_weight"], 2.0);
        assert_eq!(sent.budget, saved.budget);
        assert_eq!(sent.routes, saved.routes);
        assert_eq!(stages_patch(&saved, &saved.clone()), None);
        // A budget saved since the draft was made isn't undone by saving the stages.
        let mut newer = saved.clone();
        newer.budget.images_max = 9;
        assert_eq!(
            stages_patch(&newer, &draft)
                .unwrap()
                .pipeline
                .unwrap()
                .budget
                .images_max,
            9
        );
        // Options: registered names, plus the current one when the service lacks it.
        let opts = stage_options("packer", "mmr-v1", &variants);
        assert_eq!(
            opts.iter().map(|(v, _)| v.as_str()).collect::<Vec<_>>(),
            ["mmr-v1", "take-k"]
        );
        let opts = stage_options("reranker", "cross-v9", &variants);
        assert_eq!(
            opts[0],
            (
                "cross-v9".to_string(),
                "cross-v9 (not on this service)".to_string()
            )
        );
        // The picker's key ignores routes and budget.
        assert_eq!(stages_key(&saved), stages_key(&PipelineConfig::default()));
        assert_ne!(stages_key(&saved), stages_key(&draft));
    }

    #[test]
    fn budget_numbers_are_checked() {
        let ok = |a: &str, b: &str, c: &str, d: &str| parse_budget(&[a, b, c, d].map(String::from));
        assert_eq!(ok("50", "25", "8", "0").unwrap(), Budget::default());
        let b = ok(" 12.5 ", "0", "0", "3").unwrap();
        assert_eq!(b.share_of_window, 0.125);
        assert_eq!((b.min_score_pct, b.images_max, b.passages_min), (0, 0, 3));
        assert_eq!(budget_fields(&b)[0], "12.5");
        assert_eq!(
            ok("100%", "100", "1000", "500").unwrap().share_of_window,
            1.0
        );
        // No fixed small cap on pictures or passages.
        assert_eq!(
            ok("50", "25", "4000000", "0").unwrap().images_max,
            4_000_000
        );
        assert_eq!(ok("0", "25", "8", "0").unwrap_err().0, BudgetField::Share);
        assert_eq!(ok("101", "25", "8", "0").unwrap_err().0, BudgetField::Share);
        assert_eq!(
            ok("half", "25", "8", "0").unwrap_err().0,
            BudgetField::Share
        );
        assert_eq!(
            ok("50", "101", "8", "0").unwrap_err().0,
            BudgetField::MinScore
        );
        assert_eq!(
            ok("50", "25", "-1", "0").unwrap_err().0,
            BudgetField::Images
        );
        assert_eq!(
            ok("50", "25", "8", "2.5").unwrap_err().0,
            BudgetField::PassagesMin
        );
        // Reset keeps the model slots and nothing else.
        let mut cfg = PipelineConfig::default();
        cfg.routes.write = Some("gpt-6".into());
        cfg.retriever.name = "dense".into();
        cfg.budget.images_max = 1;
        let reset = reset_pipeline(&cfg);
        assert_eq!(reset.routes, cfg.routes);
        assert_eq!(reset.retriever.name, "hybrid-rrf");
        assert_eq!(reset.budget, Budget::default());
    }

    #[test]
    fn advanced_pane_sets_trace_retention() {
        use crate::store::LibraryState;
        assert!(SettingsPane::ALL.contains(&SettingsPane::Advanced));
        let mut lib = LibraryState::from_library(crate::fake::sample_library());
        let mut settings = crate::fake::FakeState::new().settings;
        settings.trace_retention_days = 45;
        lib.settings = Some(settings);
        let html =
            crate::ui::test_render::render_with_settings("/", lib, Some(SettingsPane::Advanced));
        assert!(html.contains("<h1>Advanced</h1>"), "{html}");
        assert!(html.contains("<h2>Traces</h2>"));
        assert!(html.contains("Keep traces for"));
        assert!(html.contains(r#"id="trace-days""#));
        assert!(html.contains(r#"value="45""#));
        assert!(html.contains("never what was said"));
        assert_eq!(parse_retention(" 30 "), Ok(30));
        assert_eq!(parse_retention("3650"), Ok(3650));
        assert!(parse_retention("0").is_err());
        assert!(parse_retention("3651").is_err());
        assert!(parse_retention("a week").is_err());
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
        let html = &html[html.find(r#"aria-labelledby="ai-effort-label""#).unwrap()..];
        let html = &html[..html.find("</div>").unwrap()];
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
            person_id: None,
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
