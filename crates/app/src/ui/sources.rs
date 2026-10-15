//! Sources in the app: adding files and folders (with the platform's picker, see
//! `crate::picker`), the upload queue, rows with live status, the files attached to a
//! chat (and those its composer sends next), a topic's Sources dialog, and the notice for
//! the embedding model the service needs to read them.

use dioxus::prelude::*;
use margin_shared::{ChatPatch, EmbeddingModelState, Event, Id, Source, SourceKind};

use crate::picker;
use crate::sources::{Destination, Tone, Upload, UploadState, human_bytes, status_text};
use crate::state::{AppCtx, use_app};
use crate::store::LibraryState;
use crate::ui::common::{ConfirmDialog, Icon, js};
use crate::ui::settings::SettingsPane;

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

/// Pick files, queue them for upload to a topic or a chat's composer, and say what was
/// left out.
pub async fn add_sources(app: AppCtx, to: Destination) -> Option<String> {
    let selection = picker::pick_files().await?;
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
            .map(|c| Upload::to(c, to.clone())),
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
    let mut uploads = app.uploads;
    let progress = move |sent: u64| {
        let mut list = app.uploads;
        if let Some(u) = list.write().iter_mut().find(|u| u.key == key) {
            u.sent = sent;
        }
    };
    let result = api
        .upload_source(
            &upload.path,
            &upload.name,
            upload.folder.as_deref(),
            upload.topic_id.as_deref(),
            progress,
        )
        .await
        .map_err(|e| app.failure(&e));
    let mut lib = app.lib;
    let wanted = uploads.peek().iter().any(|u| u.key == key);
    if result.is_ok() || !wanted {
        picker::release(&upload.path);
    }
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
                u.state = UploadState::Done(Box::new(source));
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
        u.sent = 0;
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
    if upload.state != UploadState::Uploading {
        // An uploading one's copy goes when its upload finishes.
        picker::release(&upload.path);
    }
    let mut uploads = app.uploads;
    uploads.write().retain(|u| u.key != key);
    Ok(())
}

/// "Read again": the service reads the source anew, in place (for one that failed or
/// was read with a warning, once what was wrong is fixed).
pub async fn read_again(app: AppCtx, id: Id) -> Result<(), String> {
    let api = app.api().ok_or_else(|| "Not signed in.".to_string())?;
    let source = api
        .reprocess_source(&id)
        .await
        .map_err(|e| app.failure(&e))?;
    let mut lib = app.lib;
    lib.write().apply(Event::SourceChanged { source });
    Ok(())
}

/// The "Read again" button, while the request is out it shows a spinner.
#[component]
pub fn ReadAgainButton(id: Id, name: String, on_error: EventHandler<String>) -> Element {
    let app = use_app();
    let mut busy = use_signal(|| false);
    rsx! {
        button {
            class: "btn btn-secondary btn-sm",
            "aria-label": "Read {name} again",
            title: "Read it again on the service",
            disabled: *busy.read(),
            onclick: move |_| {
                let id = id.clone();
                async move {
                    if *busy.peek() {
                        return;
                    }
                    busy.set(true);
                    if let Err(e) = read_again(app, id).await {
                        on_error.call(e);
                    }
                    busy.set(false);
                }
            },
            if *busy.read() { span { class: "spinner" } }
            "Read again"
        }
    }
}

/// Fetch the embedding model's state into the store (events keep it current after).
pub async fn refresh_embedding(app: AppCtx) -> Result<(), String> {
    let api = app.api().ok_or_else(|| "Not signed in.".to_string())?;
    let model = api.embedding().await.map_err(|e| app.failure(&e))?;
    let mut lib = app.lib;
    lib.write().apply(Event::EmbeddingModelChanged { model });
    Ok(())
}

/// Start downloading the embedding model on the service.
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
        UploadState::Uploading => (Tone::Working, upload.uploading_text()),
        UploadState::Done(source) => {
            let live = lib.sources.get(&source.id).unwrap_or(source);
            status_text(&live.status)
        }
        UploadState::Failed(message) => (Tone::Failed, format!("Couldn't upload: {message}")),
    }
}

/// A source file: kind icon, name, size and status, then the row's actions.
#[component]
pub fn SourceLine(
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
                span { class: tone.meta_class(),
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
fn UploadActions(
    upload: Upload,
    on_remove: EventHandler<()>,
    on_error: EventHandler<String>,
) -> Element {
    let app = use_app();
    let key = upload.key;
    // An uploaded file the service couldn't read (all of): read it again.
    let read_again = match &upload.state {
        UploadState::Done(source) => app
            .lib
            .read()
            .sources
            .get(&source.id)
            .filter(|s| s.status.can_read_again())
            .map(|s| s.id.clone()),
        _ => None,
    };
    rsx! {
        if matches!(upload.state, UploadState::Failed(_)) {
            button { class: "btn btn-secondary btn-sm", onclick: move |_| retry(app, key), "Try again" }
        }
        if let Some(id) = read_again {
            ReadAgainButton { id, name: upload.name.clone(), on_error }
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

/// What a chat's composer sends with its next message: the files uploaded in it, and
/// whether any is still uploading (sending waits for those).
pub fn composer_files(uploads: &[Upload], chat_id: Option<&str>) -> (Vec<Id>, bool) {
    let mine = || uploads.iter().filter(|u| u.in_composer(chat_id));
    let ids = mine().filter_map(|u| u.source_id().cloned()).collect();
    (ids, mine().any(Upload::pending))
}

/// The composer's files went with a message: they're the chat's now (`Chat.source_ids`),
/// listed from the chat from here on.
pub fn forget_sent(app: AppCtx, chat_id: Option<&str>, sent: &[Id]) {
    let mut uploads = app.uploads;
    uploads
        .write()
        .retain(|u| !(u.in_composer(chat_id) && u.source_id().is_some_and(|id| sent.contains(id))));
}

/// The chat composer's "Attach" chip: add files for the next message, attach a folder
/// any device holds (listed, this device's first), or attach a folder of this device
/// (picked, registered and synced, then attached). A folder's files are copied to the
/// service and read there. `above`: the composer is docked at the bottom, so the menu
/// opens upwards.
#[component]
pub fn AddSourcesMenu(
    chat_id: Option<Id>,
    above: bool,
    on_note: EventHandler<Option<String>>,
) -> Element {
    let app = use_app();
    let mut open = use_signal(|| false);
    let mut picking = use_signal(|| false);
    let files_chat = chat_id.clone();
    let add = move |_| {
        open.set(false);
        if *picking.peek() {
            return;
        }
        let to = Destination::Chat(files_chat.clone());
        spawn(async move {
            picking.set(true);
            let note = add_sources(app, to).await;
            picking.set(false);
            on_note.call(note);
        });
    };
    let new_chat = chat_id.clone();
    let attach_new = move |_| {
        open.set(false);
        if *picking.peek() {
            return;
        }
        let chat = new_chat.clone();
        spawn(async move {
            picking.set(true);
            let note = match crate::folders::sync::attach(app).await {
                Ok(Some(ws)) => attach_folder(app, chat.as_deref(), ws.id).await.err(),
                Ok(None) => None,
                Err(e) => Some(e),
            };
            picking.set(false);
            on_note.call(note);
        });
    };
    // Every folder not attached to this chat yet, this device's first.
    let attached = match &chat_id {
        Some(id) => app
            .lib
            .read()
            .chats
            .get(id)
            .map(|c| c.workspace_ids.clone())
            .unwrap_or_default(),
        None => app.ask_workspaces.read().clone(),
    };
    let me = app.session.read().as_ref().and_then(|s| {
        s.me.as_ref()
            .and_then(|m| m.device.as_ref())
            .map(|d| d.id.clone())
    });
    let folders: Vec<margin_shared::Workspace> = app
        .lib
        .read()
        .workspaces_in_order(me.as_deref())
        .into_iter()
        .filter(|w| !attached.contains(&w.id))
        .cloned()
        .collect();
    // Placed against the chip, above it when the composer sits at the bottom.
    let placement = if above { "menu above" } else { "menu below" };
    rsx! {
        div { class: "pop-anchor",
            button {
                class: "chip",
                "aria-haspopup": "menu",
                "aria-expanded": open.read().to_string(),
                title: "Attach files for the answer to draw on",
                disabled: *picking.read(),
                onclick: move |_| open.toggle(),
                if *picking.read() {
                    span { class: "spinner" }
                } else {
                    Icon { name: "clip" }
                }
                "Attach"
            }
            if *open.read() {
                div { class: "pop-scrim", onclick: move |_| open.set(false) }
                div {
                    class: placement,
                    role: "menu",
                    onkeydown: move |e: KeyboardEvent| if e.key() == Key::Escape { e.stop_propagation(); open.set(false) },
                    button { role: "menuitem", onmounted: crate::ui::common::focus_on_mount, onclick: add,
                        Icon { name: "file-add" }
                        "Add files…"
                    }
                    p { class: "menu-note", {crate::sources::chat_files_text()} }
                    FolderMenuItems {
                        folders,
                        on_pick: move |id: Id| {
                            open.set(false);
                            let chat = chat_id.clone();
                            spawn(async move {
                                on_note.call(attach_folder(app, chat.as_deref(), id).await.err());
                            });
                        },
                        on_attach_new: attach_new,
                    }
                }
            }
        }
    }
}

/// The Attach menu's folders: every folder not attached yet (`folders`), then attaching
/// one of this device, and what attaching a folder does.
#[component]
pub fn FolderMenuItems(
    folders: Vec<margin_shared::Workspace>,
    on_pick: EventHandler<Id>,
    on_attach_new: EventHandler<()>,
) -> Element {
    rsx! {
        div { class: "menu-label", role: "presentation", "Folders" }
        for ws in folders {
            button {
                key: "{ws.id}",
                role: "menuitem",
                title: "{ws.location}",
                onclick: {
                    let id = ws.id.clone();
                    move |_| on_pick.call(id.clone())
                },
                Icon { name: "folder" }
                span { {folder_menu_label(&ws)} }
            }
        }
        button { role: "menuitem", onclick: move |_| on_attach_new.call(()),
            Icon { name: "folder" }
            "Attach a folder from this device…"
        }
        p { class: "menu-note", {folders_menu_text()} }
    }
}

/// A file attached to a chat, waiting for the owner to confirm removing it.
#[derive(Clone, PartialEq)]
struct Detach {
    source: Source,
    /// The topic it also belongs to (it stays there), if any.
    topic: Option<String>,
}

/// The files of a chat, under its composer: those attached to the chat (`Chat.source_ids`,
/// each removable), then those the composer sends with the next message (uploading or
/// uploaded; removing one deletes it). `chat_id` `None`: a new chat's composer.
#[component]
pub fn ChatAttachments(chat_id: Option<Id>) -> Element {
    let app = use_app();
    let mut error = use_signal(|| None::<String>);
    let mut confirm = use_signal(|| None::<Detach>);
    let mut busy = use_signal(|| false);

    let lib = app.lib.read();
    let attached: Vec<(Source, Option<String>)> = chat_id
        .as_ref()
        .and_then(|id| lib.chats.get(id))
        .map(|chat| {
            chat.source_ids
                .iter()
                .filter_map(|id| lib.sources.get(id))
                .map(|s| {
                    let topic = s
                        .topic_id
                        .as_ref()
                        .and_then(|t| lib.topics.get(t))
                        .map(|t| t.title.clone());
                    (s.clone(), topic)
                })
                .collect()
        })
        .unwrap_or_default();
    let lines: Vec<(Upload, Tone, String)> = app
        .uploads
        .read()
        .iter()
        .filter(|u| u.in_composer(chat_id.as_deref()))
        .map(|u| {
            let (tone, status) = upload_status(u, &lib);
            (u.clone(), tone, status)
        })
        .collect();
    drop(lib);
    if attached.is_empty() && lines.is_empty() && error.read().is_none() {
        return rsx! {};
    }
    let n = attached.len();
    let attached_label = match n {
        1 => "1 file attached to this chat".to_string(),
        n => format!("{n} files attached to this chat"),
    };

    rsx! {
        div { class: "attachments",
            if !attached.is_empty() {
                details { class: "collapse",
                    summary {
                        Icon { name: "chev", class: "chev" }
                        Icon { name: "clip" }
                        "{attached_label}"
                    }
                    div { role: "list", "aria-label": "Files attached to this chat",
                        for (source, topic) in attached {
                            div { key: "{source.id}", role: "listitem",
                                AttachedLine {
                                    source: source.clone(),
                                    topic: topic.clone(),
                                    on_error: move |e| error.set(Some(e)),
                                    on_remove: move |_| confirm.set(Some(Detach { source: source.clone(), topic: topic.clone() })),
                                }
                            }
                        }
                    }
                }
            }
            if !lines.is_empty() {
                div { role: "list", "aria-label": "Files to send with your message",
                    for (upload, tone, status) in lines {
                        div { key: "{upload.key}", role: "listitem",
                            SourceLine {
                                kind: upload.kind,
                                name: upload.name.clone(),
                                bytes: upload.bytes,
                                tone,
                                status,
                                UploadActions {
                                    upload: upload.clone(),
                                    on_error: move |e| error.set(Some(e)),
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
                    }
                }
            }
            if let Some(err) = error.read().clone() {
                div { class: "notice error", role: "alert",
                    Icon { name: "alert" }
                    div { class: "body", "{err}" }
                    button { class: "icon-btn sm", "aria-label": "Dismiss", onclick: move |_| error.set(None), Icon { name: "x" } }
                }
            }
        }
        if let (Some(d), Some(chat_id)) = (confirm.read().clone(), chat_id.clone()) {
            ConfirmDialog {
                title: format!("Remove “{}” from this chat?", d.source.name),
                message: match &d.topic {
                    Some(t) => format!("The agent won't use it in this chat any more. It stays in the topic “{t}”."),
                    None => "The file and the passages read from it are deleted from the service. This can't be undone.".to_string(),
                },
                confirm_label: "Remove",
                busy: *busy.read(),
                on_cancel: move |_| confirm.set(None),
                on_confirm: move |_| {
                    let chat_id = chat_id.clone();
                    let source_id = d.source.id.clone();
                    async move {
                        busy.set(true);
                        match detach(app, &chat_id, &source_id).await {
                            Ok(()) => error.set(None),
                            Err(e) => error.set(Some(e)),
                        }
                        busy.set(false);
                        confirm.set(None);
                    }
                },
            }
        }
    }
}

/// The folders attached to chat `chat_id`, or (`None`) to the new chat.
pub fn chat_folders(app: AppCtx, chat_id: Option<&str>) -> Vec<Id> {
    match chat_id {
        Some(id) => app
            .lib
            .peek()
            .chats
            .get(id)
            .map(|c| c.workspace_ids.clone())
            .unwrap_or_default(),
        None => app.ask_workspaces.peek().clone(),
    }
}

/// Set the folders of chat `chat_id` (`ChatPatch.workspace_ids`), or of the new chat.
async fn set_chat_folders(app: AppCtx, chat_id: Option<&str>, ids: Vec<Id>) -> Result<(), String> {
    let Some(chat_id) = chat_id else {
        let mut ask = app.ask_workspaces;
        ask.set(ids);
        return Ok(());
    };
    let api = app.api().ok_or_else(|| "Not signed in.".to_string())?;
    let patch = ChatPatch {
        workspace_ids: Some(ids),
        ..Default::default()
    };
    let chat = api
        .patch_chat(chat_id, &patch)
        .await
        .map_err(|e| app.failure(&e))?;
    let mut lib = app.lib;
    lib.write().upsert_chat(chat);
    Ok(())
}

/// Attach a folder (of any device) to a chat, or to the new chat.
pub async fn attach_folder(app: AppCtx, chat_id: Option<&str>, id: Id) -> Result<(), String> {
    let mut ids = chat_folders(app, chat_id);
    if ids.contains(&id) {
        return Ok(());
    }
    ids.push(id);
    set_chat_folders(app, chat_id, ids).await
}

/// Take a folder off a chat. The folder stays on its device and on the service.
pub async fn detach_folder(app: AppCtx, chat_id: Option<&str>, id: &str) -> Result<(), String> {
    let ids: Vec<Id> = chat_folders(app, chat_id)
        .into_iter()
        .filter(|w| w != id)
        .collect();
    set_chat_folders(app, chat_id, ids).await
}

/// What the Attach menu says about folders.
pub fn folders_menu_text() -> String {
    format!(
        "A folder's {} are copied to your Margin service and read there, so the agent can search inside them; other files are left alone.",
        margin_shared::FOLDER_FILE_TYPES_TEXT
    )
}

/// A folder as the Attach menu lists it: "notes · on test-mac · 12 files".
pub fn folder_menu_label(ws: &margin_shared::Workspace) -> String {
    let files = match ws.sync.files {
        1 => "1 file".to_string(),
        n => format!("{n} files"),
    };
    format!("{} · on {} · {files}", ws.name, ws.device_name)
}

/// Take a file off a chat (`ChatPatch.source_ids` without it). The service deletes it
/// unless a topic has it too.
async fn detach(app: AppCtx, chat_id: &str, source_id: &str) -> Result<(), String> {
    let api = app.api().ok_or_else(|| "Not signed in.".to_string())?;
    let Some(ids) = app
        .lib
        .peek()
        .chats
        .get(chat_id)
        .map(|c| c.source_ids.clone())
    else {
        return Ok(());
    };
    let patch = ChatPatch {
        source_ids: Some(ids.into_iter().filter(|id| id != source_id).collect()),
        ..Default::default()
    };
    let chat = api
        .patch_chat(chat_id, &patch)
        .await
        .map_err(|e| app.failure(&e))?;
    let mut lib = app.lib;
    lib.write().upsert_chat(chat);
    Ok(())
}

/// A file attached to a chat: its status, Read again when it couldn't be read, and
/// remove.
#[component]
fn AttachedLine(
    source: Source,
    topic: Option<String>,
    on_remove: EventHandler<()>,
    on_error: EventHandler<String>,
) -> Element {
    let (tone, mut status) = status_text(&source.status);
    if let Some(t) = &topic {
        status = format!("{status} · in “{t}”");
    }
    let label = source.name.clone();
    rsx! {
        SourceLine {
            kind: source.kind,
            name: label.clone(),
            bytes: source.bytes,
            tone,
            status,
            if source.status.can_read_again() {
                ReadAgainButton { id: source.id.clone(), name: label.clone(), on_error }
            }
            button {
                class: "icon-btn sm",
                "aria-label": "Remove {label} from this chat",
                title: "Remove from this chat…",
                onclick: move |_| on_remove.call(()),
                Icon { name: "x" }
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
        if *busy.peek() {
            return;
        }
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
                            "Its text, image and audio encoders run on the service and download together, once. Until then, sources wait in the queue."
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
                                        if *busy.read() { span { class: "spinner" } }
                                        "Download · {human_bytes(*download_bytes)}"
                                    }
                                },
                                EmbeddingModelState::Failed { .. } => rsx! {
                                    button {
                                        class: "btn btn-primary btn-sm",
                                        disabled: *busy.read(),
                                        onclick: download,
                                        if *busy.read() {
                                            span { class: "spinner" }
                                            "Starting…"
                                        } else {
                                            "Try again"
                                        }
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
/// files and folders.
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

    let add = move |_| {
        let topic_id = topic_id.clone();
        if *picking.peek() {
            return;
        }
        spawn(async move {
            picking.set(true);
            let added = add_sources(app, Destination::Topic(topic_id)).await;
            picking.set(false);
            note.set(added);
        });
    };

    rsx! {
        div {
            class: "modal-scrim center sheet",
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
                p {
                    "Outlines and lessons in this topic draw on the passages, pages, pictures and clips of these files that matter most."
                    if !empty { " " {margin_shared::routes::source_limits_text()} }
                }
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
                            "No sources yet. "
                            {crate::sources::kinds_text()}
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
                                on_error: move |e| error.set(Some(e)),
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
                                })), on_error: move |e| error.set(Some(e)) }
                            }
                        }
                        }
                    }
                }
                div { class: "row-actions src-foot",
                    button {
                        class: "btn btn-secondary btn-sm",
                        disabled: *picking.read(),
                        onclick: add,
                        Icon { name: "file-add" }
                        "Add files…"
                    }
                    if *picking.read() { span { class: "spinner" } }
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
fn SourceRow(
    source: Source,
    on_delete: EventHandler<Source>,
    on_error: EventHandler<String>,
) -> Element {
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
            if source.status.can_read_again() {
                ReadAgainButton { id: source.id.clone(), name: label.clone(), on_error }
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(non_snake_case)]
    fn Menu() -> Element {
        let mut lib = LibraryState::default();
        lib.replace_workspaces(crate::fake::sample_workspaces());
        let folders: Vec<margin_shared::Workspace> = lib
            .workspaces_in_order(Some("d1"))
            .into_iter()
            .filter(|w| w.id != "w2")
            .cloned()
            .collect();
        rsx! { FolderMenuItems { folders, on_pick: |_| {}, on_attach_new: |_| {} } }
    }

    #[test]
    fn the_attach_menu_lists_every_devices_folders_and_attaching_one() {
        let mut dom = VirtualDom::new(Menu);
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        let notes = html.find("notes · on test-mac · 5 files").expect(&html);
        let photos = html.find("Photos · on iPhone · 18 files").unwrap();
        let scans = html.find("Scans · on iPhone · 0 files").unwrap();
        // This device's first.
        assert!(notes < photos && photos < scans);
        assert!(!html.contains("Papers"));
        assert!(html.contains("Attach a folder from this device…"));
        assert!(html.contains(
            "A folder&#39;s text documents (.md, .txt, .rst, .tex, .html…), PDFs and images are copied to your Margin service and read there"
        ));
    }
}
