//! Sources in the app: adding files and folders (desktop), the upload queue, rows with
//! live status, the Ask composer's attachments, a topic's Sources dialog, and the notice
//! for the embedding model the service needs to read them.

use std::collections::HashSet;

use dioxus::prelude::*;
use margin_shared::{EmbeddingModelState, Event, Id, Source, SourceKind};

use crate::sources::{
    Selection, Tone, Upload, UploadState, human_bytes, select_files, select_folder, status_text,
};
use crate::state::{AppCtx, use_app};
use crate::store::LibraryState;
use crate::ui::common::{ConfirmDialog, Icon, js};
use crate::ui::settings::SettingsPane;

/// Files and folders can be added here. The iOS and Android webviews can't pick files in
/// Dioxus 0.7.9 (its file input asks the host, which has no picker there), so adding
/// sources is desktop only; listing and deleting them works everywhere.
pub const CAN_ADD: bool = cfg!(not(any(target_os = "ios", target_os = "android")));

/// Uploads running at once.
const CONCURRENT_UPLOADS: usize = 3;

pub fn kind_icon(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Text => "note",
        SourceKind::Code => "code",
        SourceKind::Pdf => "pdf",
        SourceKind::Image => "image",
        SourceKind::Audio => "audio",
        SourceKind::Video => "video",
    }
}

/// Ask for files or a folder and choose what to upload; `None` when the picker was
/// cancelled.
#[cfg(not(any(target_os = "ios", target_os = "android")))]
async fn pick(folder: bool) -> Option<Selection> {
    let dialog = rfd::AsyncFileDialog::new();
    if folder {
        let handle = dialog.set_title("Add a folder").pick_folder().await?;
        let root = handle.path().to_path_buf();
        tokio::task::spawn_blocking(move || select_folder(&root))
            .await
            .ok()
    } else {
        let handles = dialog.set_title("Add files").pick_files().await?;
        let paths = handles.iter().map(|h| h.path().to_path_buf()).collect();
        tokio::task::spawn_blocking(move || select_files(paths))
            .await
            .ok()
    }
}

#[cfg(any(target_os = "ios", target_os = "android"))]
async fn pick(_folder: bool) -> Option<Selection> {
    None
}

/// Pick files (or a folder), queue them for upload to `topic_id` (`None`: the Ask
/// composer), and say what was left out.
pub async fn add_sources(app: AppCtx, topic_id: Option<Id>, folder: bool) -> Option<String> {
    let selection = pick(folder).await?;
    let mut note = selection.summary();
    if selection.files.is_empty() && note.is_none() {
        note = Some(match &selection.folder {
            Some(name) => format!("“{name}” has no files Margin can read."),
            None => "No files were added.".into(),
        });
    }
    let mut uploads = app.uploads;
    uploads.write().extend(
        selection
            .files
            .into_iter()
            .map(|c| Upload::new(c, topic_id.clone())),
    );
    pump(app);
    note
}

/// Start waiting uploads while fewer than `CONCURRENT_UPLOADS` run. The tasks belong to
/// the app, not a screen, so leaving the screen doesn't stop them.
fn pump(app: AppCtx) {
    let mut uploads = app.uploads;
    let start: Vec<u64> = {
        let mut list = uploads.write();
        let running = list
            .iter()
            .filter(|u| u.state == UploadState::Uploading)
            .count();
        let mut keys = Vec::new();
        for upload in list
            .iter_mut()
            .filter(|u| u.state == UploadState::Waiting)
            .take(CONCURRENT_UPLOADS.saturating_sub(running))
        {
            upload.state = UploadState::Uploading;
            keys.push(upload.key);
        }
        keys
    };
    for key in start {
        dioxus::core::spawn_forever(upload(app, key));
    }
}

async fn upload(app: AppCtx, key: u64) {
    let Some(upload) = app.uploads.peek().iter().find(|u| u.key == key).cloned() else {
        return pump(app);
    };
    let Some(api) = app.api() else { return };
    let path = upload.path.clone();
    let result = match tokio::task::spawn_blocking(move || std::fs::read(path)).await {
        Ok(Ok(bytes)) => api
            .upload_source(
                bytes,
                &upload.name,
                upload.folder.as_deref(),
                upload.topic_id.as_deref(),
            )
            .await
            .map_err(|e| app.failure(&e)),
        Ok(Err(e)) => Err(format!("Couldn't read the file ({e}).")),
        Err(_) => Err("Couldn't read the file.".into()),
    };
    let mut uploads = app.uploads;
    let mut lib = app.lib;
    let wanted = uploads.peek().iter().any(|u| u.key == key);
    match result {
        Ok(source) if wanted => {
            lib.write().apply(Event::SourceChanged {
                source: source.clone(),
            });
            let mut list = uploads.write();
            if upload.topic_id.is_some() {
                // The topic's list shows it from the store from now on.
                list.retain(|u| u.key != key);
            } else if let Some(u) = list.iter_mut().find(|u| u.key == key) {
                u.state = UploadState::Done(source);
            }
        }
        Ok(source) => {
            // Removed while it was uploading.
            if api.delete_source(&source.id).await.is_ok() {
                lib.write().apply(Event::SourceDeleted { id: source.id });
            }
        }
        Err(message) => {
            if let Some(u) = uploads.write().iter_mut().find(|u| u.key == key) {
                u.state = UploadState::Failed(message);
            }
        }
    }
    pump(app);
}

/// Try a failed upload again.
fn retry(app: AppCtx, key: u64) {
    let mut uploads = app.uploads;
    if let Some(u) = uploads.write().iter_mut().find(|u| u.key == key) {
        u.state = UploadState::Waiting;
    }
    pump(app);
}

/// Take a file out: an uploaded one is deleted on the service; one still uploading is
/// deleted when its upload finishes.
async fn remove(app: AppCtx, key: u64) -> Result<(), String> {
    let found = app.uploads.peek().iter().find(|u| u.key == key).cloned();
    let Some(upload) = found else { return Ok(()) };
    if let UploadState::Done(source) = &upload.state {
        let api = app.api().ok_or_else(|| "Not signed in.".to_string())?;
        match api.delete_source(&source.id).await {
            Ok(()) => {}
            // Already gone on the service.
            Err(e) if e.status == Some(404) => {}
            Err(e) => return Err(app.failure(&e)),
        }
        let mut lib = app.lib;
        lib.write().apply(Event::SourceDeleted {
            id: source.id.clone(),
        });
    }
    let mut uploads = app.uploads;
    uploads.write().retain(|u| u.key != key);
    Ok(())
}

/// Fetch the embedding model's state into the store (events keep it current after).
pub async fn refresh_embedding(app: AppCtx) -> Result<(), String> {
    let api = app.api().ok_or_else(|| "Not signed in.".to_string())?;
    let model = api.embedding().await.map_err(|e| app.failure(&e))?;
    let mut lib = app.lib;
    lib.write().apply(Event::EmbeddingModelChanged { model });
    Ok(())
}

/// Start downloading the embedding model on the service Mac.
pub async fn download_embedding(app: AppCtx) -> Result<(), String> {
    let api = app.api().ok_or_else(|| "Not signed in.".to_string())?;
    let model = api
        .download_embedding()
        .await
        .map_err(|e| app.failure(&e))?;
    let mut lib = app.lib;
    lib.write().apply(Event::EmbeddingModelChanged { model });
    Ok(())
}

/// How an upload's status reads: its own while uploading, the source's after.
fn upload_status(upload: &Upload, lib: &LibraryState) -> (Tone, String) {
    match &upload.state {
        UploadState::Waiting => (Tone::Waiting, "Waiting to upload".into()),
        UploadState::Uploading => (Tone::Working, "Uploading…".into()),
        UploadState::Done(source) => {
            let live = lib.sources.get(&source.id).unwrap_or(source);
            status_text(&live.status)
        }
        UploadState::Failed(message) => (Tone::Failed, format!("Couldn't upload: {message}")),
    }
}

/// A source file: kind icon, name, size and status, then the row's actions.
#[component]
fn SourceLine(
    kind: SourceKind,
    name: String,
    bytes: u64,
    tone: Tone,
    status: String,
    children: Element,
) -> Element {
    rsx! {
        div { class: "src-row",
            Icon { name: kind_icon(kind), class: "src-ic" }
            div { class: "src-main",
                span { class: "src-name", title: "{name}", "{name}" }
                span { class: if tone == Tone::Failed { "src-meta bad" } else { "src-meta" },
                    span { class: tone.dot_class() }
                    span { "{human_bytes(bytes)} · {status}" }
                }
            }
            {children}
        }
    }
}

/// Actions on an upload that isn't done: Retry when it failed, and remove.
#[component]
fn UploadActions(upload: Upload, on_remove: EventHandler<()>) -> Element {
    let app = use_app();
    let key = upload.key;
    rsx! {
        if matches!(upload.state, UploadState::Failed(_)) {
            button { class: "btn btn-ghost btn-sm", onclick: move |_| retry(app, key), "Retry" }
        }
        button {
            class: "icon-btn sm",
            "aria-label": "Remove {upload.name}",
            title: "Remove",
            onclick: move |_| on_remove.call(()),
            Icon { name: "x" }
        }
    }
}

/// The Ask composer's "Sources" chip: add files or a folder.
#[component]
pub fn AddSourcesMenu(on_note: EventHandler<Option<String>>) -> Element {
    let app = use_app();
    let mut open = use_signal(|| false);
    let mut picking = use_signal(|| false);
    let mut add = move |folder: bool| {
        open.set(false);
        if *picking.peek() {
            return;
        }
        spawn(async move {
            picking.set(true);
            let note = add_sources(app, None, folder).await;
            picking.set(false);
            on_note.call(note);
        });
    };
    rsx! {
        div { class: "pop-anchor",
            button {
                class: "chip",
                "aria-haspopup": "menu",
                "aria-expanded": open.read().to_string(),
                title: "Add files or a folder for the lessons to draw on",
                disabled: *picking.read(),
                onclick: move |_| open.toggle(),
                if *picking.read() {
                    span { class: "spinner", style: "width: 14px; height: 14px" }
                } else {
                    Icon { name: "clip" }
                }
                "Sources"
            }
            if *open.read() {
                div { class: "pop-scrim", onclick: move |_| open.set(false) }
                div {
                    class: "menu below",
                    role: "menu",
                    onkeydown: move |e: KeyboardEvent| if e.key() == Key::Escape { open.set(false) },
                    button { role: "menuitem", onmounted: crate::ui::common::focus_on_mount, onclick: move |_| add(false),
                        Icon { name: "note" }
                        "Add files…"
                    }
                    button { role: "menuitem", onclick: move |_| add(true),
                        Icon { name: "folder" }
                        "Add folder…"
                    }
                }
            }
        }
    }
}

/// One line in the composer's list, with its status worked out.
#[derive(Clone, PartialEq)]
struct Line {
    upload: Upload,
    tone: Tone,
    status: String,
}

#[derive(Clone, PartialEq)]
enum Entry {
    File(Box<Line>),
    Folder(String, Vec<Line>),
}

/// "12 files · 9 ready · 2 in progress · 1 failed", and the dot for the whole folder.
fn folder_status(lines: &[Line]) -> (Tone, String) {
    let count = |tone: Tone| lines.iter().filter(|l| l.tone == tone).count();
    let (ready, failed) = (count(Tone::Ready), count(Tone::Failed));
    let working = count(Tone::Working) + count(Tone::Waiting);
    let mut parts = vec![if lines.len() == 1 {
        "1 file".to_string()
    } else {
        format!("{} files", lines.len())
    }];
    if ready > 0 {
        parts.push(format!("{ready} ready"));
    }
    if working > 0 {
        parts.push(format!("{working} in progress"));
    }
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    let tone = if working > 0 {
        Tone::Working
    } else if failed > 0 {
        Tone::Failed
    } else {
        Tone::Ready
    };
    (tone, parts.join(" · "))
}

/// The files added in the Ask composer, under it: loose files, and folders that open to
/// show their files. Each can be removed; removing an uploaded one deletes it.
#[component]
pub fn AskAttachments() -> Element {
    let app = use_app();
    let mut expanded = use_signal(HashSet::<String>::new);
    let mut error = use_signal(|| None::<String>);

    let lib = app.lib.read();
    let mut entries: Vec<Entry> = Vec::new();
    for upload in app.uploads.read().iter().filter(|u| u.topic_id.is_none()) {
        let (tone, status) = upload_status(upload, &lib);
        let line = Line {
            upload: upload.clone(),
            tone,
            status,
        };
        match &upload.folder {
            None => entries.push(Entry::File(Box::new(line))),
            Some(folder) => {
                let existing = entries
                    .iter_mut()
                    .find(|e| matches!(e, Entry::Folder(name, _) if name == folder));
                match existing {
                    Some(Entry::Folder(_, lines)) => lines.push(line),
                    _ => entries.push(Entry::Folder(folder.clone(), vec![line])),
                }
            }
        }
    }
    drop(lib);
    if entries.is_empty() {
        return rsx! {};
    }

    let mut remove_all = move |keys: Vec<u64>| {
        error.set(None);
        spawn(async move {
            for key in keys {
                if let Err(e) = remove(app, key).await {
                    error.set(Some(e));
                    break;
                }
            }
        });
    };

    rsx! {
        div { class: "attachments", role: "list", "aria-label": "Sources",
            for entry in entries {
                match entry {
                    Entry::File(line) => rsx! {
                        div { key: "{line.upload.key}", role: "listitem",
                            SourceLine {
                                kind: line.upload.kind,
                                name: line.upload.name.clone(),
                                bytes: line.upload.bytes,
                                tone: line.tone,
                                status: line.status.clone(),
                                UploadActions {
                                    upload: line.upload.clone(),
                                    on_remove: move |_| remove_all(vec![line.upload.key]),
                                }
                            }
                        }
                    },
                    Entry::Folder(name, lines) => {
                        let open = expanded.read().contains(&name);
                        let (tone, status) = folder_status(&lines);
                        let bytes: u64 = lines.iter().map(|l| l.upload.bytes).sum();
                        let keys: Vec<u64> = lines.iter().map(|l| l.upload.key).collect();
                        let toggle_name = name.clone();
                        let toggle_label = if open {
                            format!("Hide the files in {name}")
                        } else {
                            format!("Show the files in {name}")
                        };
                        rsx! {
                            div { key: "folder-{name}", role: "listitem", class: if open { "src-group" } else { "src-group collapsed" },
                                div { class: "src-row",
                                    button {
                                        class: "src-toggle",
                                        "aria-expanded": open.to_string(),
                                        "aria-label": toggle_label,
                                        onclick: move |_| {
                                            let mut set = expanded.write();
                                            if !set.remove(&toggle_name) {
                                                set.insert(toggle_name.clone());
                                            }
                                        },
                                        Icon { name: "chev", class: "chev" }
                                        Icon { name: "folder", class: "src-ic" }
                                    }
                                    div { class: "src-main",
                                        span { class: "src-name", title: "{name}", "{name}" }
                                        span { class: if tone == Tone::Failed { "src-meta bad" } else { "src-meta" },
                                            span { class: tone.dot_class() }
                                            span { "{human_bytes(bytes)} · {status}" }
                                        }
                                    }
                                    button {
                                        class: "icon-btn sm",
                                        "aria-label": "Remove the folder {name}",
                                        title: "Remove folder",
                                        onclick: move |_| remove_all(keys.clone()),
                                        Icon { name: "x" }
                                    }
                                }
                                if open {
                                    div { class: "src-nested",
                                        for line in lines {
                                            SourceLine {
                                                key: "{line.upload.key}",
                                                kind: line.upload.kind,
                                                name: line.upload.name.clone(),
                                                bytes: line.upload.bytes,
                                                tone: line.tone,
                                                status: line.status.clone(),
                                                UploadActions {
                                                    upload: line.upload.clone(),
                                                    on_remove: move |_| remove_all(vec![line.upload.key]),
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(err) = error.read().clone() {
            div { class: "notice error", role: "alert",
                Icon { name: "alert" }
                div { class: "body", "{err}" }
            }
        }
    }
}

/// Shown while the service's embedding model isn't ready: sources wait for it. Offers the
/// download (with its progress) and the Sources settings.
#[component]
pub fn EmbeddingNotice() -> Element {
    let mut app = use_app();
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let Some(model) = app.lib.read().embedding.clone() else {
        return rsx! {};
    };
    let download = move |_| async move {
        busy.set(true);
        error.set(download_embedding(app).await.err());
        busy.set(false);
    };
    rsx! {
        match model.state {
            EmbeddingModelState::Ready { .. } => rsx! {},
            state => rsx! {
                div { class: "notice", role: "status",
                    Icon { name: "clip" }
                    div { class: "body",
                        b { class: "notice-title", "Sources need the local embedding model" }
                        span { class: "muted",
                            "It runs on the service Mac and is downloaded once. Until then, sources wait in the queue."
                        }
                        match &state {
                            EmbeddingModelState::Downloading { received, total } => rsx! {
                                Progress { received: *received, total: *total }
                            },
                            EmbeddingModelState::Failed { error: message } => rsx! {
                                span { class: "save-state error", "The download failed: {message}" }
                            },
                            _ => rsx! {},
                        }
                        div { class: "row-actions",
                            match &state {
                                EmbeddingModelState::NotDownloaded { download_bytes } => rsx! {
                                    button {
                                        class: "btn btn-primary btn-sm",
                                        disabled: *busy.read(),
                                        onclick: download,
                                        if *busy.read() { span { class: "spinner", style: "border-color: transparent; border-top-color: currentColor" } }
                                        "Download · {human_bytes(*download_bytes)}"
                                    }
                                },
                                EmbeddingModelState::Failed { .. } => rsx! {
                                    button {
                                        class: "btn btn-primary btn-sm",
                                        disabled: *busy.read(),
                                        onclick: download,
                                        "Try again"
                                    }
                                },
                                _ => rsx! {},
                            }
                            button {
                                class: "btn btn-secondary btn-sm",
                                onclick: move |_| app.settings_open.set(Some(SettingsPane::Sources)),
                                Icon { name: "gear" }
                                "Open Settings"
                            }
                        }
                        if let Some(err) = error.read().clone() {
                            span { class: "save-state error", "{err}" }
                        }
                    }
                }
            },
        }
    }
}

/// A download's progress bar with "received of total".
#[component]
pub fn Progress(
    received: u64,
    total: u64,
    /// What is downloading, the bar's accessible name.
    #[props(default = "Downloading the embedding model".to_string())]
    label: String,
) -> Element {
    let pct = (received.min(total) * 100).checked_div(total).unwrap_or(0);
    rsx! {
        div { class: "progress",
            div {
                class: "bar wide",
                role: "progressbar",
                "aria-label": "{label}",
                "aria-valuemin": "0",
                "aria-valuemax": "100",
                "aria-valuenow": "{pct}",
                "aria-valuetext": "{human_bytes(received)} of {human_bytes(total)}",
                i { style: "width: {pct}%" }
            }
            span { class: "faint", "{human_bytes(received)} of {human_bytes(total)}" }
        }
    }
}

/// What a delete in the Sources dialog removes, waiting for confirmation.
#[derive(Clone, PartialEq)]
struct PendingDelete {
    title: String,
    message: String,
    ids: Vec<Id>,
}

/// A topic's sources, grouped by folder, with their status and size: delete them, or add
/// files and folders (desktop).
#[component]
pub fn TopicSourcesDialog(topic_id: Id, on_close: EventHandler<()>) -> Element {
    let app = use_app();
    let mut note = use_signal(|| None::<String>);
    let mut picking = use_signal(|| false);
    let mut confirm = use_signal(|| None::<PendingDelete>);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);

    use_future(move || async move {
        let _ = refresh_embedding(app).await;
    });
    // Focus the dialog so Esc reaches it.
    use_effect(|| {
        let _ = js("document.getElementById('sources-dialog')?.focus();");
    });

    let lib = app.lib.read();
    let title = lib
        .topics
        .get(&topic_id)
        .map(|t| t.title.clone())
        .unwrap_or_default();
    let mut groups: Vec<(Option<String>, Vec<Source>)> = Vec::new();
    for source in lib.sources_in_topic(&topic_id) {
        match groups.last_mut() {
            Some((folder, list)) if *folder == source.folder => list.push(source.clone()),
            _ => groups.push((source.folder.clone(), vec![source.clone()])),
        }
    }
    let pending: Vec<Upload> = app
        .uploads
        .read()
        .iter()
        .filter(|u| u.topic_id.as_deref() == Some(topic_id.as_str()))
        .cloned()
        .collect();
    let lines: Vec<(Upload, Tone, String)> = pending
        .into_iter()
        .map(|u| {
            let (tone, status) = upload_status(&u, &lib);
            (u, tone, status)
        })
        .collect();
    drop(lib);
    let empty = groups.is_empty() && lines.is_empty();

    let add = move |folder: bool| {
        let topic_id = topic_id.clone();
        move |_| {
            let topic_id = topic_id.clone();
            if *picking.peek() {
                return;
            }
            spawn(async move {
                picking.set(true);
                let added = add_sources(app, Some(topic_id), folder).await;
                picking.set(false);
                note.set(added);
            });
        }
    };

    rsx! {
        div {
            class: "modal-scrim center",
            onclick: move |_| on_close.call(()),
            div {
                id: "sources-dialog",
                class: "modal sources-modal",
                role: "dialog",
                "aria-modal": "true",
                "aria-label": "Sources for {title}",
                tabindex: "-1",
                onclick: move |e| e.stop_propagation(),
                onkeydown: move |e: KeyboardEvent| {
                    if e.key() == Key::Escape {
                        on_close.call(());
                    }
                },
                div { class: "src-head",
                    div { class: "grow",
                        h2 { "Sources" }
                        small { "{title}" }
                    }
                    button {
                        class: "icon-btn",
                        "aria-label": "Close",
                        title: "Close (Esc)",
                        onclick: move |_| on_close.call(()),
                        Icon { name: "x" }
                    }
                }
                p { "Outlines and lessons in this topic draw on the passages of these files that matter most." }
                if !empty {
                    EmbeddingNotice {}
                }
                if let Some(text) = note.read().clone() {
                    div { class: "notice", role: "status",
                        Icon { name: "alert" }
                        div { class: "body", "{text}" }
                        button { class: "icon-btn sm", "aria-label": "Dismiss", onclick: move |_| note.set(None), Icon { name: "x" } }
                    }
                }
                if let Some(err) = error.read().clone() {
                    div { class: "notice error", role: "alert",
                        Icon { name: "alert" }
                        div { class: "body", "{err}" }
                    }
                }
                div { class: "src-list",
                    if empty {
                        p { class: "faint",
                            if CAN_ADD {
                                "No sources yet. Add text, code or PDF files, or a whole folder."
                            } else {
                                "No sources yet."
                            }
                        }
                    }
                    for (upload, tone, status) in lines {
                        SourceLine {
                            key: "u{upload.key}",
                            kind: upload.kind,
                            name: upload.name.clone(),
                            bytes: upload.bytes,
                            tone,
                            status,
                            UploadActions {
                                upload: upload.clone(),
                                on_remove: move |_| {
                                    let key = upload.key;
                                    spawn(async move {
                                        if let Err(e) = remove(app, key).await {
                                            error.set(Some(e));
                                        }
                                    });
                                },
                            }
                        }
                    }
                    for (folder, sources) in groups {
                        div { key: "{folder:?}",
                        if let Some(name) = folder.clone() {
                            div { class: "src-folder",
                                Icon { name: "folder", class: "src-ic" }
                                span { class: "grow", title: "{name}", "{name}" }
                                span { class: "count", "{sources.len()}" }
                                button {
                                    class: "icon-btn sm",
                                    "aria-label": "Delete the folder {name}",
                                    title: "Delete folder…",
                                    onclick: {
                                        let ids: Vec<Id> = sources.iter().map(|s| s.id.clone()).collect();
                                        let name = name.clone();
                                        move |_| confirm.set(Some(PendingDelete {
                                            title: format!("Delete the folder “{name}”?"),
                                            message: format!(
                                                "Its {} from this topic and the passages read from them are deleted from the service. This can't be undone.",
                                                if ids.len() == 1 { "file".to_string() } else { format!("{} files", ids.len()) }
                                            ),
                                            ids: ids.clone(),
                                        }))
                                    },
                                    Icon { name: "trash" }
                                }
                            }
                        }
                        div { class: if folder.is_some() { "src-nested" } else { "" },
                            for source in sources {
                                SourceRow { key: "{source.id}", source: source.clone(), on_delete: move |s: Source| confirm.set(Some(PendingDelete {
                                    title: format!("Delete “{}”?", s.name),
                                    message: "The file and the passages read from it are deleted from the service. This can't be undone.".into(),
                                    ids: vec![s.id],
                                })) }
                            }
                        }
                        }
                    }
                }
                div { class: "row-actions src-foot",
                    if CAN_ADD {
                        button {
                            class: "btn btn-secondary btn-sm",
                            disabled: *picking.read(),
                            onclick: add(false),
                            Icon { name: "note" }
                            "Add files…"
                        }
                        button {
                            class: "btn btn-secondary btn-sm",
                            disabled: *picking.read(),
                            onclick: add(true),
                            Icon { name: "folder" }
                            "Add folder…"
                        }
                        if *picking.read() { span { class: "spinner" } }
                    } else {
                        span { class: "faint", style: "font-size: 13px", "Add sources from Margin on your computer." }
                    }
                    span { class: "grow" }
                    button { class: "btn btn-primary btn-sm", onclick: move |_| on_close.call(()), "Done" }
                }
            }
        }
        if let Some(pending) = confirm.read().clone() {
            ConfirmDialog {
                title: pending.title.clone(),
                message: pending.message.clone(),
                confirm_label: "Delete",
                busy: *busy.read(),
                on_cancel: move |_| confirm.set(None),
                on_confirm: move |_| {
                    let ids = pending.ids.clone();
                    async move {
                        let Some(api) = app.api() else { return };
                        busy.set(true);
                        error.set(None);
                        for id in ids {
                            match api.delete_source(&id).await {
                                Ok(()) => {
                                    let mut lib = app.lib;
                                    lib.write().apply(Event::SourceDeleted { id });
                                }
                                Err(e) => {
                                    error.set(Some(app.failure(&e)));
                                    break;
                                }
                            }
                        }
                        busy.set(false);
                        confirm.set(None);
                    }
                },
            }
        }
    }
}

/// A source in a topic's list, with its delete button.
#[component]
fn SourceRow(source: Source, on_delete: EventHandler<Source>) -> Element {
    let (tone, status) = status_text(&source.status);
    // Inside a folder, the path below it is the name.
    let label = source.name.clone();
    rsx! {
        SourceLine {
            kind: source.kind,
            name: label.clone(),
            bytes: source.bytes,
            tone,
            status,
            button {
                class: "icon-btn sm",
                "aria-label": "Delete {label}",
                title: "Delete…",
                onclick: move |_| on_delete.call(source.clone()),
                Icon { name: "trash" }
            }
        }
    }
}
