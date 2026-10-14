//! All notes: a searchable list grouped by topic, and an editor with a Markdown
//! preview that autosaves (debounced PATCH).

use std::time::Duration;

use dioxus::prelude::*;
use margin_shared::{Event, Id, NewNote, Note, NotePatch};

use crate::markdown;
use crate::routes::Route;
use crate::state::{LoadStatus, use_app};
use crate::ui::common::{ConfirmDialog, CrumbLink, Icon, Loading, TopBar, ago, render_math};

/// A topic breadcrumb and its notes, each with the title of the lesson it came from.
type NoteGroup = (String, Vec<(Note, Option<String>)>);

const AUTOSAVE_DELAY: Duration = Duration::from_millis(700);

#[component]
pub fn NotesScreen(topic: Option<Id>, selected: Option<Id>) -> Element {
    let app = use_app();
    let nav = navigator();
    let mut query = use_signal(String::new);
    let mut creating = use_signal(|| false);
    let mut new_topic = use_signal(|| None::<Id>);
    let mut create_error = use_signal(|| None::<String>);
    let mut item_menu = use_signal(|| None::<Id>);
    let mut confirm_note = use_signal(|| None::<(Id, String)>);
    let mut del_busy = use_signal(|| false);
    let mut del_error = use_signal(|| None::<String>);

    let lib = app.lib.read();
    let loading = *app.lib_status.read() == LoadStatus::Loading && lib.topics.is_empty();
    let topic_title = topic
        .as_ref()
        .and_then(|t| lib.topics.get(t).map(|t| t.title.clone()));
    let q = query.read().trim().to_lowercase();
    let in_scope = |n: &Note| match &topic {
        Some(t) => lib.topic_path(&n.topic_id).iter().any(|p| &p.id == t),
        None => true,
    };
    let matches = |n: &Note| {
        q.is_empty()
            || n.title.to_lowercase().contains(&q)
            || n.body_md.to_lowercase().contains(&q)
            || n.quote
                .as_deref()
                .is_some_and(|x| x.to_lowercase().contains(&q))
    };
    // Group by topic breadcrumb, groups ordered by their newest note.
    let mut groups: Vec<(String, Vec<Note>)> = Vec::new();
    for note in lib.all_notes() {
        if !in_scope(note) || !matches(note) {
            continue;
        }
        let crumb = lib.topic_breadcrumb(&note.topic_id);
        match groups.iter_mut().find(|(c, _)| *c == crumb) {
            Some((_, list)) => list.push(note.clone()),
            None => groups.push((crumb, vec![note.clone()])),
        }
    }
    let total: usize = groups.iter().map(|(_, l)| l.len()).sum();
    let topics: Vec<(Id, String)> = lib
        .topics
        .values()
        .map(|t| (t.id.clone(), lib.topic_breadcrumb(&t.id)))
        .collect::<Vec<_>>();
    let mut topics = topics;
    topics.sort_by_key(|t| t.1.to_lowercase());
    let lesson_titles = |n: &Note| {
        n.lesson_id
            .as_ref()
            .and_then(|l| lib.lessons.get(l).map(|l| l.title.clone()))
    };
    let rows: Vec<NoteGroup> = groups
        .into_iter()
        .map(|(c, list)| {
            let with = list
                .into_iter()
                .map(|n| {
                    let lt = lesson_titles(&n);
                    (n, lt)
                })
                .collect();
            (c, with)
        })
        .collect();
    let selected_title = selected
        .as_ref()
        .and_then(|s| lib.notes.get(s).map(|n| n.title.clone()));
    let selected_exists = selected_title.is_some();
    // Whether a new note in topic `t` belongs in this list (else it opens in All notes).
    let scope_of_new = |t: &str| {
        topic
            .clone()
            .filter(|scope| lib.topic_path(t).iter().any(|p| &p.id == scope))
    };
    let scopes: Vec<(Id, Option<Id>)> = topics
        .iter()
        .map(|(id, _)| (id.clone(), scope_of_new(id)))
        .collect();
    drop(lib);

    let heading = match &topic_title {
        Some(t) => format!("{t} · notes"),
        None => "All notes".to_string(),
    };
    // "<list> / <note>" while a note is open; the list crumb leads back to the list.
    let (crumbs, crumb_links) = match &selected_title {
        Some(title) => (
            vec![heading.clone(), title.clone()],
            vec![
                Some(CrumbLink::Route(Route::notes_list(topic.clone()))),
                None,
            ],
        ),
        None => (vec![heading.clone()], Vec::new()),
    };
    let default_topic = topic
        .clone()
        .or_else(|| topics.first().map(|(id, _)| id.clone()));

    let create = move |_| {
        let scopes = scopes.clone();
        async move {
            let chosen = new_topic.peek().clone();
            let Some(topic_id) = chosen else {
                create_error.set(Some("Choose a topic for the note.".into()));
                return;
            };
            let Some(api) = app.api() else { return };
            create_error.set(None);
            let new = NewNote {
                topic_id,
                lesson_id: None,
                title: "Untitled note".into(),
                body_md: String::new(),
                quote: None,
            };
            match api.create_note(&new).await {
                Ok(note) => {
                    let id = note.id.clone();
                    let scope = scopes
                        .iter()
                        .find(|(t, _)| *t == note.topic_id)
                        .and_then(|(_, s)| s.clone());
                    let mut lib = app.lib;
                    lib.write().apply(Event::NoteChanged { note });
                    creating.set(false);
                    nav.push(Route::note_in(scope, id));
                }
                Err(e) => create_error.set(Some(app.failure(&e))),
            }
        }
    };

    rsx! {
        TopBar { crumbs, links: crumb_links }
        div { class: "scroll", style: "overflow: hidden",
            div { class: if selected_exists { "notes-wrap has-selection" } else { "notes-wrap" },
                div { class: "note-list",
                    div { class: "note-list-head",
                        b { "{heading}" }
                        button {
                            class: "icon-btn",
                            "aria-label": "New note",
                            title: "New note",
                            onclick: move |_| {
                                new_topic.set(default_topic.clone());
                                create_error.set(None);
                                creating.toggle();
                            },
                            Icon { name: "plus" }
                        }
                    }
                    if *creating.read() {
                        div { class: "note-card", style: "gap: 10px",
                            if topics.is_empty() {
                                p { class: "muted", style: "margin: 0", "Create a topic in the sidebar first; every note belongs to a topic." }
                            } else {
                                div { class: "field",
                                    label { r#for: "new-note-topic", "Topic" }
                                    select {
                                        id: "new-note-topic",
                                        class: "select-input",
                                        onchange: move |e| new_topic.set(Some(e.value())),
                                        for (id, name) in topics.iter().cloned() {
                                            option { key: "{id}", value: "{id}", selected: new_topic.read().as_deref() == Some(id.as_str()), "{name}" }
                                        }
                                    }
                                }
                                if let Some(err) = create_error.read().clone() {
                                    span { class: "save-state error", "{err}" }
                                }
                                div { class: "row-actions",
                                    button { class: "btn btn-primary btn-sm", onclick: create, "Create note" }
                                    button { class: "btn btn-ghost btn-sm", onclick: move |_| creating.set(false), "Cancel" }
                                }
                            }
                        }
                    }
                    input {
                        class: "input",
                        r#type: "search",
                        placeholder: "Filter notes",
                        "aria-label": "Filter notes",
                        initial_value: "{query}",
                        oninput: move |e| query.set(e.value()),
                    }
                    if loading {
                        Loading {}
                    } else if total == 0 {
                        div { class: "empty", style: "padding: 32px 12px",
                            if q.is_empty() {
                                p { "No notes yet. Select text in a lesson to add a margin note, or press + to write one." }
                            } else {
                                p { "No notes match “{query}”." }
                            }
                        }
                    }
                    for (crumb, list) in rows {
                        div { key: "{crumb}", class: "note-group",
                            if topic.is_none() {
                                h4 { "{crumb}" }
                            }
                            for (note, lesson_title) in list {
                                div {
                                    key: "{note.id}",
                                    class: "note-row",
                                    "data-long-press": "true",
                                    // Right-click: the same actions as the row's buttons.
                                    oncontextmenu: {
                                        let id = note.id.clone();
                                        move |e: MouseEvent| {
                                            e.prevent_default();
                                            item_menu.set(Some(id.clone()));
                                        }
                                    },
                                    button {
                                        class: "note-item",
                                        "aria-current": (selected.as_deref() == Some(note.id.as_str())).to_string(),
                                        onclick: {
                                            let route = Route::note_in(topic.clone(), note.id.clone());
                                            move |_| { nav.push(route.clone()); }
                                        },
                                        b { "{note.title}" }
                                        span {
                                            "{ago(&note.updated_at)}"
                                            if let Some(t) = lesson_title { " · from {t}" }
                                        }
                                    }
                                    button {
                                        class: "icon-btn sm note-del",
                                        "aria-label": "Delete {note.title}",
                                        title: "Delete note",
                                        onclick: {
                                            let target = (note.id.clone(), note.title.clone());
                                            move |_| {
                                                del_error.set(None);
                                                confirm_note.set(Some(target.clone()));
                                            }
                                        },
                                        Icon { name: "trash" }
                                    }
                                    if item_menu.read().as_deref() == Some(note.id.as_str()) {
                                        div { class: "pop-scrim", onclick: move |_| item_menu.set(None) }
                                        div {
                                            class: "menu right",
                                            role: "menu",
                                            onkeydown: move |e: KeyboardEvent| if e.key() == Key::Escape { e.stop_propagation(); item_menu.set(None) },
                                            button {
                                                role: "menuitem",
                                                autofocus: true,
                                                onclick: {
                                                    let route = Route::note_in(topic.clone(), note.id.clone());
                                                    move |_| {
                                                        item_menu.set(None);
                                                        nav.push(route.clone());
                                                    }
                                                },
                                                Icon { name: "note" }
                                                "Open"
                                            }
                                            button {
                                                role: "menuitem",
                                                class: "danger",
                                                onclick: {
                                                    let target = (note.id.clone(), note.title.clone());
                                                    move |_| {
                                                        item_menu.set(None);
                                                        del_error.set(None);
                                                        confirm_note.set(Some(target.clone()));
                                                    }
                                                },
                                                Icon { name: "trash" }
                                                "Delete…"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                div { class: "note-doc",
                    match selected.clone() {
                        Some(id) if selected_exists => rsx! {
                            for id in std::iter::once(id) {
                                NoteEditor { key: "{id}", id: id.clone(), scope: topic.clone() }
                            }
                        },
                        Some(_) if !loading => rsx! {
                            div { class: "empty",
                                h2 { "This note isn't here any more" }
                                p { "It may have been deleted on another device." }
                            }
                        },
                        _ => rsx! {
                            div { class: "empty",
                                h2 { "Pick a note" }
                                p { "Choose a note on the left, or press + to start a new one." }
                            }
                        },
                    }
                }
            }
        }
        if let Some((id, title)) = confirm_note.read().clone() {
            ConfirmDialog {
                title: "Delete “{title}”?",
                message: "This note will be deleted. This can't be undone.",
                confirm_label: "Delete note",
                busy: *del_busy.read(),
                error: del_error.read().clone(),
                on_cancel: move |_| confirm_note.set(None),
                on_confirm: {
                    let topic = topic.clone();
                    let selected = selected.clone();
                    move |_| {
                        let id = id.clone();
                        let topic = topic.clone();
                        let was_open = selected.as_deref() == Some(id.as_str());
                        async move {
                            let Some(api) = app.api() else { return };
                            del_busy.set(true);
                            del_error.set(None);
                            match api.delete_note(&id).await {
                                Ok(()) => {
                                    let mut lib = app.lib;
                                    lib.write().apply(Event::NoteDeleted { id });
                                    confirm_note.set(None);
                                    if was_open {
                                        nav.replace(Route::notes_list(topic));
                                    }
                                }
                                Err(e) => del_error.set(Some(app.failure(&e))),
                            }
                            del_busy.set(false);
                        }
                    }
                },
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum SaveState {
    Saved,
    Pending,
    Saving,
}

/// The open note. `scope` is the list it was opened in (a topic's, or All notes), where
/// "Back to the list" and deleting it return.
#[component]
fn NoteEditor(id: Id, scope: Option<Id>) -> Element {
    let app = use_app();
    let nav = navigator();
    let initial = use_hook(|| app.lib.peek().notes.get(&id).cloned());
    let Some(initial) = initial else {
        return rsx! {};
    };
    let mut title = use_signal(|| initial.title.clone());
    let mut body = use_signal(|| initial.body_md.clone());
    // What the service has, as far as we know (to detect unsaved edits).
    let mut saved = use_signal(|| (initial.title.clone(), initial.body_md.clone()));
    let mut state = use_signal(|| SaveState::Saved);
    let mut error = use_signal(|| None::<String>);
    let mut generation = use_signal(|| 0u64);
    let mut preview = use_signal(|| false);
    // Bumped when another device's edit replaces the text, to remount the fields.
    let mut remote_rev = use_signal(|| 0u64);
    let mut confirm = use_signal(|| false);
    let mut deleting = use_signal(|| false);

    // Follow edits from other devices while there are no local unsaved changes.
    let follow_id = id.clone();
    use_effect(move || {
        let remote = app
            .lib
            .read()
            .notes
            .get(&follow_id)
            .map(|n| (n.title.clone(), n.body_md.clone()));
        let Some(remote) = remote else { return };
        let clean = (title.peek().clone(), body.peek().clone()) == *saved.peek();
        if clean && remote != *saved.peek() {
            title.set(remote.0.clone());
            body.set(remote.1.clone());
            saved.set(remote);
            remote_rev += 1;
        }
    });

    let schedule = {
        let id = id.clone();
        move || {
            let id = id.clone();
            let next = *generation.peek() + 1;
            generation.set(next);
            state.set(SaveState::Pending);
            spawn(async move {
                tokio::time::sleep(AUTOSAVE_DELAY).await;
                if *generation.peek() != next {
                    return; // a newer edit is waiting
                }
                let t = title.peek().clone();
                let b = body.peek().clone();
                let (st, sb) = saved.peek().clone();
                if (t.clone(), b.clone()) == (st.clone(), sb.clone()) {
                    state.set(SaveState::Saved);
                    return;
                }
                let Some(api) = app.api() else { return };
                state.set(SaveState::Saving);
                let patch = NotePatch {
                    title: (t != st).then(|| {
                        if t.trim().is_empty() {
                            "Untitled note".to_string()
                        } else {
                            t.clone()
                        }
                    }),
                    body_md: (b != sb).then(|| b.clone()),
                };
                match api.patch_note(&id, &patch).await {
                    Ok(note) => {
                        saved.set((t, b));
                        error.set(None);
                        let mut lib = app.lib;
                        lib.write().apply(Event::NoteChanged { note });
                        if *generation.peek() == next {
                            state.set(SaveState::Saved);
                        }
                    }
                    Err(e) => {
                        error.set(Some(app.failure(&e)));
                        state.set(SaveState::Pending);
                    }
                }
            });
        }
    };
    let schedule_title = schedule.clone();
    let mut schedule_retry = schedule.clone();
    let schedule_body = schedule;

    let note = app.lib.read().notes.get(&id).cloned().unwrap_or(initial);
    let lesson = note.lesson_id.as_ref().and_then(|l| {
        app.lib
            .read()
            .lessons
            .get(l)
            .map(|l| (l.id.clone(), l.title.clone()))
    });
    let html = markdown::render(&body.read());
    let showing_preview = *preview.read();
    use_effect(use_reactive!(|showing_preview, html| {
        let _ = html;
        if showing_preview {
            render_math("note-preview");
        }
    }));

    let state_text = match (*state.read(), error.read().clone()) {
        (_, Some(err)) => err,
        (SaveState::Saved, None) => format!("Saved · edited {}", ago(&note.updated_at)),
        (SaveState::Pending, None) => "Unsaved changes".into(),
        (SaveState::Saving, None) => "Saving…".into(),
    };

    rsx! {
        div { class: "doc",
            div { class: "meta",
                button {
                    class: "icon-btn back-btn",
                    "aria-label": "Back to the list",
                    // The list the note belongs to, whatever screen came before (Search,
                    // a lesson's link, a fresh launch).
                    onclick: {
                        let list = Route::notes_list(scope.clone());
                        move |_| { nav.replace(list.clone()); }
                    },
                    Icon { name: "back" }
                }
                if let Some((lid, ltitle)) = lesson {
                    button {
                        class: "link-chip",
                        onclick: move |_| { nav.push(Route::LessonRead { id: lid.clone() }); },
                        Icon { name: "book" }
                        "{ltitle}"
                    }
                }
                span { class: if error.read().is_some() { "save-state error" } else { "save-state" }, role: "status", "{state_text}" }
                if error.read().is_some() {
                    button { class: "btn btn-ghost btn-sm", onclick: move |_| schedule_retry(), "Retry" }
                }
                div { class: "note-tools",
                    div { class: "seg", role: "tablist", "aria-label": "Editor mode",
                        button { role: "tab", "aria-selected": (!showing_preview).to_string(), tabindex: if showing_preview { "-1" } else { "0" }, onclick: move |_| preview.set(false), "Write" }
                        button { role: "tab", "aria-selected": showing_preview.to_string(), tabindex: if showing_preview { "0" } else { "-1" }, onclick: move |_| preview.set(true), "Preview" }
                    }
                    button {
                        class: "icon-btn",
                        "aria-label": "Delete note",
                        title: "Delete note",
                        onclick: move |_| confirm.set(true),
                        Icon { name: "trash" }
                    }
                }
            }
            for rev in std::iter::once(remote_rev()) {
            input {
                key: "t{rev}",
                class: "title-input",
                "aria-label": "Title",
                placeholder: "Untitled note",
                initial_value: "{title}",
                oninput: {
                    let mut schedule_title = schedule_title.clone();
                    move |e: FormEvent| {
                        title.set(e.value());
                        schedule_title();
                    }
                },
            }
            }
            if let Some(q) = &note.quote {
                blockquote { class: "md", style: "margin: 0; padding: 0 0 0 12px; border-left: 2px solid var(--line-2); background: none; color: var(--fg-2); font-style: italic; font-size: 15px",
                    "“{q}”"
                }
            }
            if showing_preview {
                div { id: "note-preview", class: "md", dangerous_inner_html: "{html}" }
            } else {
                for rev in std::iter::once(remote_rev()) {
                textarea {
                    key: "b{rev}",
                    class: "body-input",
                    "aria-label": "Note body (Markdown)",
                    placeholder: "Write in Markdown…",
                    initial_value: "{body}",
                    oninput: {
                        let mut schedule_body = schedule_body.clone();
                        move |e: FormEvent| {
                            body.set(e.value());
                            schedule_body();
                        }
                    },
                }
                }
            }
        }
        if *confirm.read() {
            ConfirmDialog {
                title: "Delete “{note.title}”?",
                message: "This note will be deleted. This can't be undone.",
                confirm_label: "Delete note",
                busy: *deleting.read(),
                on_cancel: move |_| confirm.set(false),
                on_confirm: {
                    let id = id.clone();
                    let list = Route::notes_list(scope.clone());
                    move |_| {
                        let id = id.clone();
                        let list = list.clone();
                        async move {
                            let Some(api) = app.api() else { return };
                            deleting.set(true);
                            match api.delete_note(&id).await {
                                Ok(()) => {
                                    // Stop any pending autosave.
                                    let next = *generation.peek() + 1;
                                    generation.set(next);
                                    let mut lib = app.lib;
                                    lib.write().apply(Event::NoteDeleted { id });
                                    confirm.set(false);
                                    nav.replace(list);
                                }
                                Err(e) => {
                                    error.set(Some(app.failure(&e)));
                                    confirm.set(false);
                                }
                            }
                            deleting.set(false);
                        }
                    }
                },
            }
        }
    }
}
