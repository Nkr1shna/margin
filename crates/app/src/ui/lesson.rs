//! Lesson: top bar with breadcrumbs and Read / Notes / Video; Read with margin notes
//! taken from text selection; this lesson's notes; and the video studio.

use std::time::Duration;

use chrono::{Local, Utc};
use dioxus::prelude::*;
use margin_shared::{
    Event, Id, LessonPatch, LessonStatus, NewNote, Note, NotePatch, Video, VideoStatus, WrittenWith,
};
use serde::Deserialize;

use crate::api::ApiFailure;
use crate::citations;
use crate::markdown;
use crate::routes::Route;
use crate::state::{LoadStatus, use_app};
use crate::ui::audience::FollowsLine;
use crate::ui::citations::{CitationList, SourceViewerDialog, open_viewer};
use crate::ui::common::{
    ConfirmDialog, CrumbLink, DeleteNoteDialog, ErrorNotice, Icon, Loading, RegenerateLessonDialog,
    TopBar, ago, date, js, mark_quotes, quote_marks_json, render_math_in,
};
use crate::ui::outline::level_label;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum LessonView {
    Read,
    Notes,
    Video,
}

#[component]
pub fn LessonScreen(id: Id, view: LessonView) -> Element {
    let mut app = use_app();
    let nav = navigator();
    let mut menu_open = use_signal(|| false);
    let mut renaming = use_signal(|| false);
    let mut rename_value = use_signal(String::new);
    let mut confirm_delete = use_signal(|| false);
    let mut confirm_regenerate = use_signal(|| false);
    let mut busy = use_signal(|| false);
    let mut action_error = use_signal(|| None::<String>);
    let rename_busy = use_signal(|| false);
    let mut delete_error = use_signal(|| None::<String>);

    let lib = app.lib.read();
    let Some(lesson) = lib.lessons.get(&id).cloned() else {
        let loading = *app.lib_status.read() == LoadStatus::Loading;
        drop(lib);
        return rsx! {
            TopBar { crumbs: vec!["Lesson".to_string()] }
            div { class: "scroll",
                if loading {
                    Loading {}
                } else {
                    div { class: "empty",
                        h2 { "This lesson isn't here any more" }
                        p { "It may have been deleted, or its topic's owner changed who can read it." }
                        button { class: "btn btn-primary", onclick: move |_| { nav.replace(Route::Ask {}); }, "New chat" }
                    }
                }
            }
        };
    };
    let (mut crumbs, mut crumb_links): (Vec<String>, Vec<Option<CrumbLink>>) = lib
        .topic_path(&lesson.topic_id)
        .iter()
        .map(|t| (t.title.clone(), Some(CrumbLink::Topic(t.id.clone()))))
        .unzip();
    crumbs.push(lesson.title.clone());
    crumb_links.push(None);
    drop(lib);

    let go = {
        let id = id.clone();
        move |v: LessonView| {
            let id = id.clone();
            match v {
                LessonView::Read => nav.replace(Route::LessonRead { id }),
                LessonView::Notes => nav.replace(Route::LessonNotes { id }),
                LessonView::Video => nav.replace(Route::LessonVideo { id }),
            };
        }
    };
    let go_read = go.clone();
    let go_notes = go.clone();
    let go_video = go;

    let regenerate = {
        let id = id.clone();
        move || {
            let id = id.clone();
            async move {
                let Some(api) = app.api() else { return };
                if *busy.peek() {
                    return;
                }
                busy.set(true);
                action_error.set(None);
                match api.regenerate_lesson(&id).await {
                    Ok(lesson) => app.lib.write().apply(Event::LessonChanged { lesson }),
                    Err(e) => action_error.set(Some(app.failure(&e))),
                }
                busy.set(false);
            }
        }
    };
    let regenerate_menu = regenerate.clone();
    let regenerate_confirmed = regenerate.clone();
    // A written lesson is replaced by the new draft, so ask first.
    let lesson_written = lesson.status == LessonStatus::Ready;

    rsx! {
        TopBar { crumbs, links: crumb_links,
            div { class: "seg", role: "tablist", "aria-label": "Lesson view",
                button { role: "tab", "aria-selected": (view == LessonView::Read).to_string(), tabindex: if view == LessonView::Read { "0" } else { "-1" }, onclick: move |_| go_read(LessonView::Read), "Read" }
                button { role: "tab", "aria-selected": (view == LessonView::Notes).to_string(), tabindex: if view == LessonView::Notes { "0" } else { "-1" }, onclick: move |_| go_notes(LessonView::Notes), "Notes" }
                button { role: "tab", "aria-selected": (view == LessonView::Video).to_string(), tabindex: if view == LessonView::Video { "0" } else { "-1" }, onclick: move |_| go_video(LessonView::Video), "Video" }
            }
            div { style: "position: relative",
                button {
                    class: "icon-btn",
                    "aria-label": "More lesson actions",
                    "aria-haspopup": "menu",
                    "aria-expanded": menu_open.read().to_string(),
                    onclick: move |_| menu_open.toggle(),
                    Icon { name: "more" }
                }
                if *menu_open.read() {
                    div { class: "pop-scrim", onclick: move |_| menu_open.set(false) }
                    div {
                        class: "menu right",
                        role: "menu",
                        style: "top: 34px",
                        onkeydown: move |e: KeyboardEvent| if e.key() == Key::Escape { e.stop_propagation(); menu_open.set(false) },
                        button {
                            role: "menuitem",
                            onmounted: crate::ui::common::focus_on_mount,
                            onclick: {
                                let title = lesson.title.clone();
                                move |_| {
                                    menu_open.set(false);
                                    rename_value.set(title.clone());
                                    action_error.set(None);
                                    renaming.set(true);
                                }
                            },
                            Icon { name: "edit" }
                            "Rename"
                        }
                        button {
                            role: "menuitem",
                            disabled: *busy.read(),
                            onclick: move |_| {
                                menu_open.set(false);
                                let r = regenerate_menu.clone();
                                async move {
                                    if lesson_written {
                                        confirm_regenerate.set(true);
                                    } else {
                                        r().await
                                    }
                                }
                            },
                            Icon { name: "refresh" }
                            if lesson_written { "Regenerate…" } else { "Regenerate" }
                        }
                        div { class: "sep", role: "separator" }
                        button {
                            role: "menuitem",
                            class: "danger",
                            onclick: move |_| {
                                menu_open.set(false);
                                action_error.set(None);
                                delete_error.set(None);
                                confirm_delete.set(true);
                            },
                            Icon { name: "trash" }
                            "Delete…"
                        }
                    }
                }
            }
        }
        if *renaming.read() {
            div { class: "banner",
                input {
                    class: "input",
                    style: "max-width: 420px",
                    "aria-label": "Lesson title",
                    initial_value: "{rename_value}",
                    onmounted: move |e| async move { let _ = e.set_focus(true).await; },
                    oninput: move |e| rename_value.set(e.value()),
                    onkeydown: {
                        let id = id.clone();
                        move |e: KeyboardEvent| {
                            let id = id.clone();
                            async move {
                                if e.key() == Key::Escape {
                                    renaming.set(false);
                                } else if e.key() == Key::Enter {
                                    e.prevent_default();
                                    // Read in a statement of its own: a guard held across the
                                    // save would make the next keystroke's `set` panic.
                                    let title = rename_value.peek().clone();
                                    save_title(app, id, title, renaming, action_error, rename_busy).await;
                                }
                            }
                        }
                    },
                }
                button {
                    class: "btn btn-primary btn-sm",
                    disabled: *rename_busy.read(),
                    onclick: {
                        let id = id.clone();
                        move |_| {
                            let id = id.clone();
                            let title = rename_value.peek().clone();
                            async move { save_title(app, id, title, renaming, action_error, rename_busy).await }
                        }
                    },
                    if *rename_busy.read() {
                        span { class: "spinner" }
                        "Saving…"
                    } else {
                        "Save"
                    }
                }
                button { class: "btn btn-ghost btn-sm", onclick: move |_| renaming.set(false), "Cancel" }
            }
        }
        if let Some(err) = action_error.read().clone() {
            div { class: "banner error", role: "alert",
                Icon { name: "alert" }
                span { class: "grow", "{err}" }
                button { class: "icon-btn sm", "aria-label": "Dismiss", onclick: move |_| action_error.set(None), Icon { name: "x" } }
            }
        }
        div { class: "scroll", id: "lesson-scroll",
            match view {
                LessonView::Read => rsx! {
                    ReadView {
                        lesson_id: id.clone(),
                        busy: *busy.read(),
                        on_regenerate: move |_| {
                            let r = regenerate.clone();
                            spawn(async move { r().await });
                        },
                    }
                },
                LessonView::Notes => rsx! { LessonNotesView { lesson_id: id.clone() } },
                LessonView::Video => rsx! { VideoView { lesson_id: id.clone() } },
            }
        }
        if *confirm_delete.read() {
            ConfirmDialog {
                title: "Delete “{lesson.title}”?",
                message: "This deletes the lesson with its notes and video. This can't be undone.",
                confirm_label: "Delete lesson",
                busy: *busy.read(),
                on_cancel: move |_| confirm_delete.set(false),
                on_confirm: {
                    let id = id.clone();
                    move |_| {
                        let id = id.clone();
                        async move {
                            let Some(api) = app.api() else { return };
                            busy.set(true);
                            match api.delete_lesson(&id).await {
                                Ok(()) => {
                                    confirm_delete.set(false);
                                    app.lib.write().apply(Event::LessonDeleted { id });
                                    nav.replace(Route::Ask {});
                                }
                                // Shown in the dialog, which stays open to try again.
                                Err(e) => delete_error.set(Some(app.failure(&e))),
                            }
                            busy.set(false);
                        }
                    }
                },
                error: delete_error.read().clone(),
            }
        }
        if *confirm_regenerate.read() {
            RegenerateLessonDialog {
                lesson_title: lesson.title.clone(),
                busy: *busy.read(),
                error: action_error.read().clone(),
                on_cancel: move |_| confirm_regenerate.set(false),
                on_confirm: move |_| {
                    let r = regenerate_confirmed.clone();
                    async move {
                        r().await;
                        if action_error.peek().is_none() {
                            confirm_regenerate.set(false);
                        }
                    }
                },
            }
        }
    }
}

async fn save_title(
    app: crate::state::AppCtx,
    id: Id,
    title: String,
    mut renaming: Signal<bool>,
    mut error: Signal<Option<String>>,
    mut busy: Signal<bool>,
) {
    let title = title.trim().to_string();
    if title.is_empty() {
        error.set(Some("A lesson needs a title.".into()));
        return;
    }
    let Some(api) = app.api() else { return };
    if *busy.peek() {
        return; // Enter pressed again while saving
    }
    let patch = LessonPatch {
        title: Some(title),
        ..Default::default()
    };
    busy.set(true);
    let result = api.patch_lesson(&id, &patch).await;
    busy.set(false);
    match result {
        Ok(lesson) => {
            let mut lib = app.lib;
            lib.write().apply(Event::LessonChanged { lesson });
            renaming.set(false);
            error.set(None);
        }
        Err(e) => error.set(Some(app.failure(&e))),
    }
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug, Clone, PartialEq)]
struct Selection {
    text: String,
    top: f64,
}

#[component]
fn ReadView(lesson_id: Id, busy: bool, on_regenerate: EventHandler<()>) -> Element {
    let app = use_app();
    let nav = navigator();
    let mut selection = use_signal(|| None::<Selection>);
    let mut new_quote = use_signal(|| None::<(String, f64)>);

    // Rendered body; recomputed only when this lesson's body changes.
    let body_id = lesson_id.clone();
    let body = use_memo(move || {
        app.lib
            .read()
            .lessons
            .get(&body_id)
            .map(|l| l.body_md.clone())
            .unwrap_or_default()
    });
    let cites_id = lesson_id.clone();
    let cites = use_memo(move || {
        app.lib
            .read()
            .lessons
            .get(&cites_id)
            .map(|l| citations::ordered(&l.citations))
            .unwrap_or_default()
    });
    let html = use_memo(move || markdown::render_lesson(&body.read(), &cites.read()));
    let plain = use_memo(move || markdown::plain_text(&body.read()));

    // Put the rendered body into the page. Only changed blocks are replaced (and typeset
    // first), so streaming text doesn't flicker or re-typeset finished formulas.
    use_effect(move || {
        let html = html.read().clone();
        let html = serde_json::to_string(&html).unwrap_or_default();
        let _ = js(&format!("MarginApp.setHtml('lesson-body', {html});"));
    });

    // Listen for text selections inside the lesson body.
    use_future(move || async move {
        let mut eval = js(
            "MarginApp.watchSelection('lesson-doc', function (s) { dioxus.send(s); });\nawait new Promise(function () {});",
        );
        while let Ok(sel) = eval.recv::<Selection>().await {
            if sel.text.is_empty() {
                selection.set(None);
            } else {
                selection.set(Some(sel));
            }
        }
    });
    // Citation markers are in the HTML the body is set to, so their clicks come from the
    // page; each opens the source viewer.
    let marker_lesson = lesson_id.clone();
    use_future(move || {
        let lesson_id = marker_lesson.clone();
        async move {
            let mut eval = js(
                "MarginApp.watchCitations('lesson-body', function (n) { dioxus.send(n); });\nawait new Promise(function () {});",
            );
            while let Ok(n) = eval.recv::<u32>().await {
                open_viewer(app, &lesson_id, n);
            }
        }
    });
    use_drop(move || {
        let _ = js("MarginApp.stopSelection(); MarginApp.stopCitations();");
        let mut viewer = app.source_viewer;
        viewer.set(None);
    });
    // Mark each margin note's passage in the text; a tap on one shows its note.
    let marks_id = lesson_id.clone();
    let marks = use_memo(move || quote_marks_json(app.lib.read().notes_for_lesson(&marks_id)));
    use_effect(move || mark_quotes("lesson-body", &marks.read()));

    let lib = app.lib.read();
    let Some(lesson) = lib.lessons.get(&lesson_id).cloned() else {
        return rsx! {};
    };
    let siblings = lib.lessons_in(&lesson.topic_id);
    let index = siblings.iter().position(|l| l.id == lesson.id).unwrap_or(0);
    let total = siblings.len();
    drop(siblings);
    let next = lib
        .next_lesson(&lesson.id)
        .map(|l| (l.id.clone(), l.title.clone()));
    let video = lib.video_for_lesson(&lesson.id).cloned();
    let notes: Vec<Note> = lib
        .notes_for_lesson(&lesson.id)
        .into_iter()
        .cloned()
        .collect();
    drop(lib);

    let plain_text = plain.read().clone();
    let margin_notes: Vec<Note> = markdown::margin_order(
        &plain_text,
        notes.iter().map(|n| (n.quote.as_deref(), n.clone())),
    );

    let status = lesson.status.clone();
    let sel = selection.read().clone();
    let quote_open = new_quote.read().clone();
    let topic_id = lesson.topic_id.clone();
    let lid = lesson.id.clone();
    // The margin column only takes space once the lesson has notes or one is being
    // written; until then the lesson is centered and a selection gets a small button.
    let show_margins = !notes.is_empty() || quote_open.is_some();
    let viewing = app
        .source_viewer
        .read()
        .as_ref()
        .filter(|v| v.lesson_id == lid)
        .map(|v| v.n);
    let pill = if show_margins { None } else { sel.clone() };

    rsx! {
        div { class: if show_margins { "margin-layout" } else { "margin-layout bare" },
            article { id: "lesson-doc", class: "doc",
                div { class: "meta",
                    span { class: "cat", "Lesson {index + 1} of {total}" }
                    span { "{lesson.minutes} min read" }
                    span { {level_label(lesson.level)} }
                    span { "Updated {date(&lesson.updated_at)}" }
                    if let Some(w) = lesson.written_with.as_ref().filter(|_| lesson.status == LessonStatus::Ready) {
                        span { class: "written-with", title: "{written_with_details(w)}",
                            {written_with_line(w)}
                        }
                    }
                }
                FollowsLine { topic_id: lesson.topic_id.clone(), narrowed: lesson.audience_narrowed, kind: "lesson" }
                h1 { "{lesson.title}" }
                if !lesson.summary.is_empty() {
                    p { class: "lede", "{lesson.summary}" }
                }
                if let Some(v) = video.as_ref().filter(|v| v.status == VideoStatus::Ready) {
                    VideoTeaser { video: v.clone(), title: lesson.title.clone() }
                }
                match status.clone() {
                    LessonStatus::Queued => rsx! {
                        div { class: "lesson-status", role: "status",
                            span { class: "dot run" }
                            "Waiting to be written…"
                        }
                    },
                    LessonStatus::Failed { error } => rsx! {
                        div { class: "notice error", role: "alert", style: "margin: 16px 0",
                            Icon { name: "alert" }
                            div { class: "body",
                                span { "This lesson couldn't be written. {error}" }
                                div { class: "row-actions",
                                    button {
                                        class: "btn btn-primary btn-sm",
                                        disabled: busy,
                                        onclick: move |_| on_regenerate.call(()),
                                        if busy { span { class: "spinner" } }
                                        "Regenerate"
                                    }
                                }
                            }
                        }
                    },
                    _ => rsx! {},
                }
                div { id: "lesson-body", class: "md" }
                if status == LessonStatus::Generating {
                    div { class: "generating", role: "status", span { class: "dot run" } "Writing…" }
                }
                CitationList { lesson_id: lid.clone(), citations: cites.read().clone() }
                if let Some((next_id, next_title)) = next {
                    div { class: "next",
                        div { small { "Next lesson" } "{next_title}" }
                        button {
                            class: "btn btn-secondary btn-sm",
                            onclick: move |_| { nav.push(Route::LessonRead { id: next_id.clone() }); },
                            "Continue"
                        }
                    }
                }
            }
            if let Some(s) = pill {
                button {
                    class: "note-pill",
                    style: "top: {s.top}px",
                    onclick: move |_| new_quote.set(Some((s.text.clone(), s.top))),
                    Icon { name: "plus" }
                    "Note"
                }
            }
            if show_margins {
            aside { class: "margins", "aria-label": "Margin notes",
                if let Some((quote, top)) = quote_open {
                    div { class: "add-note", style: "top: {top}px",
                        NewNoteEditor {
                            topic_id: topic_id.clone(),
                            lesson_id: Some(lid.clone()),
                            quote: Some(quote.clone()),
                            on_done: move |_| {
                                new_quote.set(None);
                                selection.set(None);
                                let _ = js("MarginApp.clearSelection();");
                            },
                        }
                    }
                } else if let Some(s) = sel {
                    div { class: "add-note", style: "top: {s.top}px",
                        div { class: "quote", "“{markdown::truncate(&s.text, 140)}”" }
                        div { class: "row-actions",
                            button {
                                class: "btn btn-primary btn-sm",
                                onclick: move |_| new_quote.set(Some((s.text.clone(), s.top))),
                                Icon { name: "plus" }
                                "Add note"
                            }
                        }
                    }
                }
                for note in margin_notes {
                    NoteCard { key: "{note.id}", note: note.clone(), compact: true }
                }
            }
            }
        }
        if let Some(n) = viewing {
            SourceViewerDialog { lesson_id: lid.clone(), n }
        }
    }
}

/// The lesson's video on the Read view: it plays right here, above the text.
#[component]
fn VideoTeaser(video: Video, title: String) -> Element {
    let duration = video.duration_seconds.map(fmt_duration).unwrap_or_default();
    rsx! {
        figure { class: "video-inline",
            VideoPlayer { video: video.clone() }
            figcaption { class: "vs", b { "Video" } "  {title}" if !duration.is_empty() { " · {duration}" } }
        }
    }
}

fn fmt_duration(secs: f32) -> String {
    let s = secs.round().max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

// ---------------------------------------------------------------------------
// Notes (shared by the margin and the lesson's Notes view)
// ---------------------------------------------------------------------------

/// A note shown as a margin note (`compact`) or a card, editable and deletable in place.
#[component]
pub fn NoteCard(note: Note, compact: bool) -> Element {
    let app = use_app();
    let mut editing = use_signal(|| false);
    let mut body = use_signal(String::new);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let mut confirm = use_signal(|| false);

    let card_id = format!("note-{}", note.id);
    let html = markdown::render(&note.body_md);
    let selector = format!("#{card_id}");
    use_effect(use_reactive!(|html| {
        let _ = html;
        render_math_in(&selector);
    }));

    let save = {
        let note = note.clone();
        move || {
            let note = note.clone();
            async move {
                let Some(api) = app.api() else { return };
                if *busy.peek() {
                    return;
                }
                let text = body.peek().clone();
                busy.set(true);
                error.set(None);
                let title = if note.title.trim().is_empty()
                    || note.title == markdown::derive_title(&note.body_md, note.quote.as_deref())
                {
                    Some(markdown::derive_title(&text, note.quote.as_deref()))
                } else {
                    None
                };
                let patch = NotePatch {
                    title,
                    body_md: Some(text),
                };
                match api.patch_note(&note.id, &patch).await {
                    Ok(n) => {
                        let mut lib = app.lib;
                        lib.write().apply(Event::NoteChanged { note: n });
                        editing.set(false);
                    }
                    Err(e) => error.set(Some(app.failure(&e))),
                }
                busy.set(false);
            }
        }
    };
    let save_key = save.clone();

    let class = match (compact, *editing.read()) {
        (true, true) => "mnote mine editing",
        (true, false) => "mnote mine",
        (false, _) => "note-card",
    };

    rsx! {
        div { class, id: "{card_id}",
            if compact {
                small { "Your note · {ago(&note.updated_at)}" }
            }
            if let Some(q) = &note.quote {
                div { class: "quote", "“{q}”" }
            }
            if !compact {
                h3 { "{note.title}" }
            }
            if *editing.read() {
                textarea {
                    class: "textarea",
                    "aria-label": "Note",
                    initial_value: "{body}",
                    onmounted: move |e| async move { let _ = e.set_focus(true).await; },
                    oninput: move |e| body.set(e.value()),
                    onkeydown: move |e: KeyboardEvent| {
                        let save = save_key.clone();
                        async move {
                            if e.key() == Key::Escape {
                                editing.set(false);
                            } else if e.key() == Key::Enter && (e.modifiers().meta() || e.modifiers().ctrl()) {
                                e.prevent_default();
                                save().await;
                            }
                        }
                    },
                }
                if let Some(err) = error.read().clone() {
                    span { class: "save-state error", "{err}" }
                }
                div { class: "row-actions",
                    button {
                        class: "btn btn-primary btn-sm",
                        disabled: *busy.read(),
                        onclick: move |_| { let s = save.clone(); async move { s().await } },
                        if *busy.read() {
                            span { class: "spinner" }
                            "Saving…"
                        } else {
                            "Save"
                        }
                    }
                    button { class: "btn btn-ghost btn-sm", onclick: move |_| editing.set(false), "Cancel" }
                }
            } else {
                div { class: "md", dangerous_inner_html: "{html}" }
                if let Some(err) = error.read().clone() {
                    span { class: "save-state error", "{err}" }
                }
                div { class: "tools", style: if compact { "" } else { "opacity: 1" },
                    button {
                        class: "icon-btn sm",
                        "aria-label": "Edit note",
                        title: "Edit note",
                        onclick: {
                            let text = note.body_md.clone();
                            move |_| {
                                body.set(text.clone());
                                error.set(None);
                                editing.set(true);
                            }
                        },
                        Icon { name: "edit" }
                    }
                    button {
                        class: "icon-btn sm",
                        "aria-label": "Delete note",
                        title: "Delete note",
                        onclick: move |_| confirm.set(true),
                        Icon { name: "trash" }
                    }
                    if !compact {
                        span { class: "faint", style: "font-size: 12.5px; margin-left: 6px; align-self: center", "Edited {ago(&note.updated_at)}" }
                    }
                }
            }
        }
        if *confirm.read() {
            DeleteNoteDialog {
                note_title: note.title.clone(),
                busy: *busy.read(),
                on_cancel: move |_| confirm.set(false),
                on_confirm: {
                    let id = note.id.clone();
                    move |_| {
                        let id = id.clone();
                        async move {
                            let Some(api) = app.api() else { return };
                            busy.set(true);
                            match api.delete_note(&id).await {
                                Ok(()) => {
                                    let mut lib = app.lib;
                                    lib.write().apply(Event::NoteDeleted { id });
                                }
                                Err(e) => error.set(Some(app.failure(&e))),
                            }
                            busy.set(false);
                            confirm.set(false);
                        }
                    }
                },
            }
        }
    }
}

/// Inline editor for a new note; with a quote it is a margin note.
#[component]
pub fn NewNoteEditor(
    topic_id: Id,
    lesson_id: Option<Id>,
    quote: Option<String>,
    on_done: EventHandler<()>,
) -> Element {
    let app = use_app();
    let mut body = use_signal(String::new);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);

    let save = {
        let quote = quote.clone();
        move || {
            let topic_id = topic_id.clone();
            let lesson_id = lesson_id.clone();
            let quote = quote.clone();
            async move {
                let text = body.peek().trim().to_string();
                if text.is_empty() && quote.is_none() {
                    error.set(Some("Write something first.".into()));
                    return;
                }
                let Some(api) = app.api() else { return };
                if *busy.peek() {
                    return; // ⌘Enter again while saving
                }
                busy.set(true);
                error.set(None);
                let new = NewNote {
                    title: markdown::derive_title(&text, quote.as_deref()),
                    topic_id,
                    lesson_id,
                    body_md: text,
                    quote,
                };
                match api.create_note(&new).await {
                    Ok(note) => {
                        let mut lib = app.lib;
                        lib.write().apply(Event::NoteChanged { note });
                        on_done.call(());
                    }
                    Err(e) => error.set(Some(app.failure(&e))),
                }
                busy.set(false);
            }
        }
    };
    let save_key = save.clone();

    rsx! {
        if let Some(q) = &quote {
            div { class: "quote", "“{markdown::truncate(q, 200)}”" }
        }
        textarea {
            class: "textarea",
            "aria-label": "New note",
            placeholder: if quote.is_some() { "Your note on this passage…" } else { "Write a note… (Markdown)" },
            initial_value: "{body}",
            onmounted: move |e| async move { let _ = e.set_focus(true).await; },
            oninput: move |e| body.set(e.value()),
            onkeydown: move |e: KeyboardEvent| {
                let save = save_key.clone();
                async move {
                    if e.key() == Key::Escape {
                        on_done.call(());
                    } else if e.key() == Key::Enter && (e.modifiers().meta() || e.modifiers().ctrl()) {
                        e.prevent_default();
                        save().await;
                    }
                }
            },
        }
        if let Some(err) = error.read().clone() {
            span { class: "save-state error", role: "alert", "{err}" }
        }
        div { class: "row-actions",
            button {
                class: "btn btn-primary btn-sm",
                disabled: *busy.read(),
                onclick: move |_| { let s = save.clone(); async move { s().await } },
                if *busy.read() { span { class: "spinner" } "Saving…" } else { "Save note" }
            }
            button { class: "btn btn-ghost btn-sm", onclick: move |_| on_done.call(()), "Cancel" }
        }
    }
}

#[component]
fn LessonNotesView(lesson_id: Id) -> Element {
    let app = use_app();
    let mut adding = use_signal(|| false);
    let lib = app.lib.read();
    let Some(lesson) = lib.lessons.get(&lesson_id).cloned() else {
        return rsx! {};
    };
    let plain = markdown::plain_text(&lesson.body_md);
    let notes: Vec<Note> = markdown::margin_order(
        &plain,
        lib.notes_for_lesson(&lesson_id)
            .into_iter()
            .map(|n| (n.quote.as_deref(), n.clone())),
    );
    drop(lib);

    rsx! {
        div { class: "note-cards",
            div { class: "view-head",
                h1 { "Notes on this lesson" }
                button {
                    class: "btn btn-secondary btn-sm",
                    disabled: *adding.read(),
                    onclick: move |_| adding.set(true),
                    Icon { name: "plus" }
                    "New note"
                }
            }
            if *adding.read() {
                div { class: "note-card",
                    NewNoteEditor {
                        topic_id: lesson.topic_id.clone(),
                        lesson_id: Some(lesson.id.clone()),
                        quote: None,
                        on_done: move |_| adding.set(false),
                    }
                }
            }
            if notes.is_empty() && !*adding.read() {
                div { class: "empty",
                    h2 { "No notes yet" }
                    p { "Select text in the Read view to add a margin note, or write one here." }
                }
            }
            for note in notes {
                NoteCard { key: "{note.id}", note: note.clone(), compact: false }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Video
// ---------------------------------------------------------------------------

#[component]
fn VideoView(lesson_id: Id) -> Element {
    let mut app = use_app();
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<ApiFailure>);
    let mut confirm_rerender = use_signal(|| false);

    // Keep the signed media URLs fresh: refetch the video when they are near expiry.
    let refresh_lesson = lesson_id.clone();
    use_future(move || {
        let lesson_id = refresh_lesson.clone();
        async move {
            loop {
                let expiring = app
                    .lib
                    .peek()
                    .video_for_lesson(&lesson_id)
                    .and_then(|v| v.media.as_ref().map(|m| (v.id.clone(), m.expires_at)));
                if let Some((video_id, expires_at)) = expiring
                    && expires_at - Utc::now() < chrono::Duration::minutes(10)
                    && let Some(api) = app.api()
                {
                    match api.video(&video_id).await {
                        Ok(video) => app.lib.write().apply(Event::VideoChanged { video }),
                        Err(e) => {
                            app.failure(&e);
                        }
                    }
                }
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        }
    });

    let lib = app.lib.read();
    let Some(lesson) = lib.lessons.get(&lesson_id).cloned() else {
        return rsx! {};
    };
    let video = lib.video_for_lesson(&lesson_id).cloned();
    drop(lib);

    let make = {
        let lesson_id = lesson_id.clone();
        move || {
            let lesson_id = lesson_id.clone();
            async move {
                let Some(api) = app.api() else { return };
                if *busy.peek() {
                    return;
                }
                busy.set(true);
                error.set(None);
                match api.make_video(&lesson_id).await {
                    Ok(video) => {
                        app.lib.write().apply(Event::VideoChanged { video });
                        confirm_rerender.set(false);
                    }
                    Err(e) => {
                        app.failure(&e);
                        error.set(Some(e));
                    }
                }
                busy.set(false);
            }
        }
    };
    let make_confirmed = make.clone();
    // A finished video is replaced by the new render, so ask first.
    let video_ready = matches!(video.as_ref().map(|v| &v.status), Some(VideoStatus::Ready));

    let lesson_ready = lesson.status == LessonStatus::Ready;
    let make_icon: &'static str = if video.is_some() { "refresh" } else { "video" };
    let (status_dot, status_text) = match video.as_ref().map(|v| &v.status) {
        None => ("dot", "No video yet".to_string()),
        Some(VideoStatus::Queued) => ("dot run", "Queued".to_string()),
        Some(VideoStatus::Scripting) => ("dot run", "Writing the scene".to_string()),
        Some(VideoStatus::Voicing { done, total }) => {
            ("dot run", format!("Voicing {done} of {total}"))
        }
        Some(VideoStatus::Rendering { progress }) => (
            "dot run",
            match progress {
                Some(p) => format!("Rendering {:.0}%", p * 100.0),
                None => "Rendering".to_string(),
            },
        ),
        Some(VideoStatus::Ready) => ("dot ok", "Ready".to_string()),
        Some(VideoStatus::Failed { .. }) => ("dot bad", "Failed".to_string()),
    };
    let in_progress = matches!(
        video.as_ref().map(|v| &v.status),
        Some(
            VideoStatus::Queued
                | VideoStatus::Scripting
                | VideoStatus::Voicing { .. }
                | VideoStatus::Rendering { .. }
        )
    );

    rsx! {
        div { class: "studio",
            div { class: "view-head studio-head",
                h1 { "{lesson.title}" }
                span { class: "status", span { class: "{status_dot}" } "{status_text}" }
                // Live: the stage only, so a changing percentage isn't read out each time.
                span { class: "sr-only", role: "status", {video_stage(video.as_ref().map(|v| &v.status))} }
                if !in_progress {
                    button {
                        class: "btn btn-primary btn-sm",
                        disabled: *busy.read() || !lesson_ready,
                        "aria-describedby": (!lesson_ready).then_some("make-video-why"),
                        onclick: move |_| {
                            if video_ready {
                                confirm_rerender.set(true);
                            } else {
                                spawn(make());
                            }
                        },
                        if *busy.read() { span { class: "spinner" } }
                        Icon { name: make_icon }
                        if video.is_some() { "Re-render" } else { "Make video" }
                    }
                    if !lesson_ready {
                        p { id: "make-video-why", class: "studio-why", "The lesson must finish writing first." }
                    }
                }
            }
            if let Some(err) = error.read().clone() {
                ErrorNotice { message: err.message.clone(), settings_pane: err.settings_pane() }
            }
            match video {
                None => rsx! {
                    div { class: "empty",
                        h2 { "No video for this lesson" }
                        p {
                            if lesson_ready {
                                "Make video writes a ManimGX scene from this lesson, voices the narration with PocketTTS, and renders it on the service."
                            } else {
                                "You can make a video once the lesson has finished writing."
                            }
                        }
                    }
                },
                Some(v) => rsx! { VideoDetails { video: v } },
            }
        }
        if *confirm_rerender.read() {
            ConfirmDialog {
                title: "Re-render the video?",
                message: "A new scene is written and rendered from the lesson, and it replaces the current video. This can't be undone.",
                confirm_label: "Re-render",
                busy: *busy.read(),
                error: error.read().as_ref().map(|e| e.message.clone()),
                on_cancel: move |_| confirm_rerender.set(false),
                on_confirm: move |_| {
                    let make = make_confirmed.clone();
                    async move {
                        make().await;
                    }
                },
            }
        }
    }
}

/// The lesson's video, playing in place with native controls and captions. Used inline
/// on the Read view and on the Video view.
#[component]
fn VideoPlayer(video: Video) -> Element {
    let app = use_app();
    let captions_url = video.media.as_ref().and_then(|m| m.captions_url.clone());
    // <track> needs WebVTT; the service writes SRT. Fetch, convert, and inline it.
    let captions = use_resource(use_reactive!(|captions_url| async move {
        let url = captions_url?;
        let api = app.api()?;
        let srt = api.fetch_text(&url).await.ok()?;
        Some(markdown::parse_srt(&srt))
    }));
    let cues = captions.read().clone().flatten();
    // The webview won't load a <track> from an inline URL, so the cues are added through
    // the video element's text-track API once both the player and the captions exist.
    let player_id = format!("player-{}", video.id);
    let ready = matches!(video.status, VideoStatus::Ready) && video.media.is_some();
    let has_poster = video.media.as_ref().is_some_and(|m| m.poster_url.is_some());
    let label = app
        .lib
        .read()
        .lessons
        .get(&video.lesson_id)
        .map(|l| format!("Video: {}", l.title))
        .unwrap_or_else(|| "Lesson video".to_string());
    use_effect(use_reactive!(|cues, player_id, ready| {
        let Some(cues) = cues.filter(|c| !c.is_empty() && ready) else {
            return;
        };
        let Ok(json) = serde_json::to_string(&cues) else {
            return;
        };
        document::eval(&format!(
            r#"(function() {{
                const v = document.getElementById({id});
                if (!v) return;
                for (const t of v.textTracks) {{ if (t.label === "Captions") t.mode = "disabled"; }}
                const track = v.addTextTrack("captions", "Captions", "en");
                for (const c of {json}) track.addCue(new VTTCue(c.start, c.end, c.text));
                // With a real poster, captions behave normally. Over the seeked-frame
                // thumbnail (no poster) they stay hidden until the video is first played.
                const thumb = v.dataset.poster !== "true" && v.dataset.thumb !== "played";
                track.mode = thumb ? "hidden" : "showing";
            }})()"#,
            id = serde_json::to_string(&player_id).unwrap_or_default(),
        ));
    }));

    let Some(media) = video.media.as_ref().filter(|_| ready) else {
        return rsx! {};
    };
    // Applies the remembered volume and mute, and keeps them in sync across players,
    // lessons and restarts. Without a poster (videos rendered before posters existed),
    // ManimGX videos open on a black frame, so the player seeks a few seconds in to show
    // a frame as the thumbnail; the first Play starts from the beginning.
    let prepare_js = format!(
        "MarginApp.preparePlayer({id}, {has_poster});",
        id = serde_json::to_string(&player_id).unwrap_or_default(),
    );
    rsx! {
        video {
            key: "{media.mp4_url}",
            id: "{player_id}",
            class: "player",
            "aria-label": "{label}",
            controls: true,
            // iPhone plays in place, not fullscreen.
            "playsinline": "true",
            "webkit-playsinline": "true",
            preload: "metadata",
            poster: media.poster_url.clone(),
            "data-poster": has_poster.to_string(),
            src: "{media.mp4_url}",
            onmounted: move |_| {
                let _ = js(&prepare_js);
            },
            "Your system can't play this video."
        }
    }
}

#[component]
fn VideoDetails(video: Video) -> Element {
    let pipeline = pipeline_steps(&video.status);

    rsx! {
        ol { class: "pipeline", "aria-label": "Video pipeline",
            for (label, state) in pipeline {
                li {
                    key: "{label}",
                    class: "{state}",
                    "aria-current": (state == "now").then_some("step"),
                    "{label}"
                    span { class: "sr-only", {step_state_text(state)} }
                }
            }
        }
        if let VideoStatus::Rendering { progress: Some(p) } = video.status {
            div { class: "bar", role: "progressbar", "aria-label": "Rendering video",
                "aria-valuenow": "{(p * 100.0).round()}", "aria-valuemin": "0", "aria-valuemax": "100",
                "aria-valuetext": "{(p * 100.0).round()}% rendered",
                i { style: "width: {(p * 100.0).clamp(0.0, 100.0)}%" }
            }
        }
        if let VideoStatus::Failed { error } = &video.status {
            div { class: "notice error", role: "alert",
                Icon { name: "alert" }
                div { class: "body", "The video failed. {error}" }
            }
        }
        VideoPlayer { video: video.clone() }
        if !video.log.is_empty() {
            details { class: "collapse",
                summary { Icon { name: "chev", class: "chev" } "Render log" }
                pre { class: "log", "{video.log}" }
            }
        }
    }
}

/// What a pipeline step's state means, for screen readers (the chips show it by color).
fn step_state_text(state: &str) -> &'static str {
    match state {
        "done" => ", done",
        "now" => ", in progress",
        "fail" => "",
        _ => ", not started",
    }
}

/// The video's stage in words, without counts or percentages, for the studio's live
/// region. Finishing and failing are announced app-wide (the shell), so they're blank.
fn video_stage(status: Option<&VideoStatus>) -> &'static str {
    match status {
        Some(VideoStatus::Queued) => "Video queued",
        Some(VideoStatus::Scripting) => "Writing the scene",
        Some(VideoStatus::Voicing { .. }) => "Voicing the narration",
        Some(VideoStatus::Rendering { .. }) => "Rendering the video",
        None | Some(VideoStatus::Ready) | Some(VideoStatus::Failed { .. }) => "",
    }
}

/// Labels and CSS states for the pipeline chips.
fn pipeline_steps(status: &VideoStatus) -> Vec<(String, &'static str)> {
    let rank = match status {
        VideoStatus::Queued => 0,
        VideoStatus::Scripting => 1,
        VideoStatus::Voicing { .. } => 2,
        VideoStatus::Rendering { .. } => 3,
        VideoStatus::Ready => 4,
        VideoStatus::Failed { .. } => 5,
    };
    let voice = match status {
        VideoStatus::Voicing { done, total } => format!("Voice · {done}/{total}"),
        _ => "Voice".to_string(),
    };
    let render = match status {
        VideoStatus::Rendering { progress: Some(p) } => format!("Render · {:.0}%", p * 100.0),
        _ => "Render".to_string(),
    };
    if rank == 5 {
        return vec![("Failed".to_string(), "fail")];
    }
    let state = |step: i32| -> &'static str {
        if rank > step {
            "done"
        } else if rank == step {
            "now"
        } else {
            ""
        }
    };
    vec![
        ("Queued".to_string(), state(0)),
        ("Script".to_string(), state(1)),
        (voice, state(2)),
        (render, state(3)),
        (
            "Ready".to_string(),
            if rank == 4 { "done" } else { state(4) },
        ),
    ]
}

/// The lesson's details line for what wrote it (R2): "Written with lesson-v2 on
/// claude-sonnet-5-5, 14 Oct 09:12", in local time.
pub fn written_with_line(w: &WrittenWith) -> String {
    format!(
        "Written with {} on {}, {}",
        w.prompt_version,
        w.model,
        w.at.with_timezone(&Local).format("%-d %b %H:%M")
    )
}

/// What hovering the line shows: the pipeline's fingerprint, the provider and the time.
pub fn written_with_details(w: &WrittenWith) -> String {
    format!(
        "Pipeline {} · {} · {}",
        w.pipeline_hash,
        w.provider.label(),
        w.at.with_timezone(&Local).format("%-d %b %Y, %H:%M")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::LibraryState;
    use crate::ui::test_render::render_route;
    use margin_shared::Provider;

    #[test]
    fn the_details_line_says_what_wrote_the_lesson() {
        let lib = crate::fake::sample_library();
        let l1 = lib.lessons.iter().find(|l| l.id == "l1").unwrap().clone();
        let w = l1.written_with.clone().unwrap();
        let line = written_with_line(&w);
        assert!(
            line.starts_with("Written with lesson-v2 on claude-sonnet-5-5, "),
            "{line}"
        );
        let details = written_with_details(&w);
        assert!(details.starts_with(&format!("Pipeline {} · Claude · ", w.pipeline_hash)));
        assert!(details.contains(&w.at.with_timezone(&Local).format("%Y").to_string()));

        let html = render_route("/lesson/l1", LibraryState::from_library(lib.clone()));
        assert!(html.contains(&line), "{html}");
        assert!(html.contains(&format!(r#"title="{details}""#)), "{html}");

        // Another provider and model, as the service sets them.
        let mut lib2 = lib.clone();
        let l2 = lib2.lessons.iter_mut().find(|l| l.id == "l2").unwrap();
        l2.written_with = Some(crate::fake::sample_written_with(
            Provider::OpenAi,
            "gpt-6-astra",
            l2.created_at,
        ));
        let html = render_route("/lesson/l2", LibraryState::from_library(lib2));
        assert!(
            html.contains("Written with lesson-v2 on gpt-6-astra, "),
            "{html}"
        );
        assert!(html.contains(" · OpenAI · "), "{html}");

        // Nothing for a lesson written before R2, or one not written yet.
        let mut lib3 = lib;
        for l in &mut lib3.lessons {
            if l.id == "l2" {
                l.written_with = None;
            }
            if l.id == "l1" {
                l.status = LessonStatus::Queued;
            }
        }
        let lib3 = LibraryState::from_library(lib3);
        let html = render_route("/lesson/l2", lib3.clone());
        assert!(html.contains("Updated "), "{html}");
        assert!(!html.contains("Written with"), "{html}");
        assert!(!render_route("/lesson/l1", lib3).contains("Written with"));
    }
}
