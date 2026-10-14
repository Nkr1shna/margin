//! Settings: service, account and devices, AI provider and key, voice, render size,
//! tools on the service Mac, and appearance.

use dioxus::prelude::*;
use margin_shared::{Device, Event, Settings, SettingsPatch, ToolStatus};

use crate::config::{Appearance, normalize_service_url};
use crate::state::use_app;
use crate::ui::common::{ConfirmDialog, ErrorNotice, Icon, TopBar, ago, copy_to_clipboard, date};

#[component]
pub fn SettingsScreen() -> Element {
    let mut app = use_app();

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

    rsx! {
        TopBar { crumbs: vec!["Settings".to_string()] }
        div { class: "scroll",
            div { class: "settings",
                h1 { "Settings" }
                ServiceSection {}
                AccountSection {}
                match (settings, settings_error.read().clone()) {
                    (Some(s), _) => rsx! {
                        AiSection { settings: s.clone() }
                        VoiceSection { settings: s.clone() }
                        RenderSection { settings: s }
                    },
                    (None, Some(err)) => rsx! {
                        section { class: "set-sec",
                            h2 { "AI, voice and video" }
                            ErrorNotice { message: err, on_retry: move |_| settings_load.restart() }
                        }
                    },
                    (None, None) => rsx! {
                        section { class: "set-sec",
                            h2 { "AI, voice and video" }
                            div { class: "skel", style: "width: 60%" }
                            div { class: "skel", style: "width: 40%" }
                        }
                    },
                }
                ToolsSection {}
                AppearanceSection {}
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
                div { class: "field",
                    label { r#for: "svc-url", "Service address" }
                    input {
                        id: "svc-url",
                        class: "input",
                        r#type: "url",
                        initial_value: "{value}",
                        oninput: move |e| value.set(e.value()),
                    }
                }
                if let Some(err) = error.read().clone() {
                    span { class: "save-state error", "{err}" }
                }
                div { class: "row-actions",
                    button {
                        class: "btn btn-primary btn-sm",
                        onclick: {
                            let current = current.clone();
                            move |_| match normalize_service_url(&value.peek()) {
                                Ok(url) if url == current => editing.set(false),
                                Ok(url) => {
                                    error.set(None);
                                    confirm.set(Some(url));
                                }
                                Err(e) => error.set(Some(e)),
                            }
                        },
                        "Change service"
                    }
                    button { class: "btn btn-ghost btn-sm", onclick: move |_| editing.set(false), "Cancel" }
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

    let dirty = *base.read() != settings.openai_base_url || *model.read() != settings.openai_model;
    let show_key_input = !settings.has_api_key || *replacing.read();

    rsx! {
        section { class: "set-sec",
            h2 { "AI provider" }
            p { class: "help", "Margin uses an OpenAI-compatible Responses API for outlines, lessons and video scenes." }
            div { class: "grid2",
                div { class: "field",
                    label { r#for: "ai-base", "Base URL" }
                    input { id: "ai-base", class: "input", r#type: "url", initial_value: "{base}", oninput: move |e| { saved_note.set(false); base.set(e.value()) } }
                }
                div { class: "field",
                    label { r#for: "ai-model", "Model" }
                    input { id: "ai-model", class: "input", initial_value: "{model}", oninput: move |e| { saved_note.set(false); model.set(e.value()) } }
                }
            }
            if let Some(err) = error.read().clone() {
                span { class: "save-state error", "{err}" }
            }
            div { class: "row-actions",
                button {
                    class: "btn btn-primary btn-sm",
                    disabled: !dirty || *busy.read(),
                    onclick: {
                        let settings = settings.clone();
                        move |_| {
                            let settings = settings.clone();
                            async move {
                                let b = base.peek().trim().to_string();
                                let m = model.peek().trim().to_string();
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
                    "Save"
                }
                if *saved_note.read() && !dirty {
                    span { class: "save-state", "Saved" }
                }
            }
            div { class: "field",
                span { class: "label", "API key" }
                if show_key_input {
                    div { class: "row-actions",
                        for rev in std::iter::once(key_rev()) {
                        input {
                            key: "{rev}",
                            class: "input",
                            style: "flex: 1; min-width: 220px",
                            r#type: "password",
                            autocomplete: "off",
                            placeholder: "sk-…",
                            "aria-label": "API key",
                            initial_value: "{key}",
                            oninput: move |e| key.set(e.value()),
                        }
                        }
                        button {
                            class: "btn btn-primary btn-sm",
                            disabled: key.read().trim().is_empty() || *key_busy.read(),
                            onclick: move |_| async move {
                                let k = key.peek().trim().to_string();
                                let patch = SettingsPatch { openai_api_key: Some(k), ..Default::default() };
                                if save_settings(app, patch, key_error, key_busy).await {
                                    key.set(String::new());
                                    key_rev += 1;
                                    replacing.set(false);
                                }
                            },
                            "Save key"
                        }
                        if settings.has_api_key {
                            button { class: "btn btn-ghost btn-sm", onclick: move |_| { key.set(String::new()); key_rev += 1; replacing.set(false) }, "Cancel" }
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

#[component]
fn VoiceSection(settings: Settings) -> Element {
    let app = use_app();
    let mut voice = use_signal(|| settings.voice.clone().unwrap_or_default());
    let error = use_signal(|| None::<String>);
    let busy = use_signal(|| false);
    let current = settings.voice.clone().unwrap_or_default();
    let dirty = voice.read().trim() != current;

    rsx! {
        section { class: "set-sec",
            h2 { "Voice" }
            p { class: "help", "The PocketTTS voice that narrates videos. Leave empty for the default voice." }
            div { class: "row-actions",
                input {
                    class: "input",
                    style: "flex: 1; min-width: 220px",
                    "aria-label": "Voice name",
                    placeholder: "Default voice",
                    initial_value: "{voice}",
                    oninput: move |e| voice.set(e.value()),
                }
                button {
                    class: "btn btn-primary btn-sm",
                    disabled: !dirty || *busy.read(),
                    onclick: move |_| async move {
                        let v = voice.peek().trim().to_string();
                        let patch = SettingsPatch {
                            voice: Some(if v.is_empty() { None } else { Some(v) }),
                            ..Default::default()
                        };
                        save_settings(app, patch, error, busy).await;
                    },
                    "Save"
                }
            }
            if let Some(err) = error.read().clone() {
                span { class: "save-state error", "{err}" }
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
                span { class: "save-state error", "{err}" }
            }
            div { class: "row-actions",
                button {
                    class: "btn btn-primary btn-sm",
                    disabled: !dirty || *busy.read(),
                    onclick: move |_| async move {
                        let (w, h) = *size.peek();
                        let patch = SettingsPatch {
                            render_width: Some(w),
                            render_height: Some(h),
                            render_fps: Some(*fps.peek()),
                            ..Default::default()
                        };
                        save_settings(app, patch, error, busy).await;
                    },
                    "Save"
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
    let commands = tool.install_commands.join("\n");
    rsx! {
        div { class: "tool",
            div { class: "tool-h",
                span { class: if tool.found { "dot ok" } else { "dot bad" } }
                b { "{tool.name}" }
                span { class: "grow muted", style: "font-size: 13px",
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
                                    copied.set(Some(copy_to_clipboard(commands).await));
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
                if let Some(Err(err)) = copied.read().clone() {
                    span { class: "save-state error", "{err}" }
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
