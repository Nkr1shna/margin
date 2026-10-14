//! Ask: the composer (prompt, lesson count, level, videos, quiz, subject, sources), the
//! drafted outline with inline editing, and "Create lessons".

use std::time::Duration;

use dioxus::prelude::*;
use margin_shared::{
    AcceptProposal, AskRequest, Id, Level, Outline, OutlineLesson, Proposal, ProposalStatus,
};

use crate::api::ApiFailure;
use crate::routes::Route;
use crate::sources::UploadState;
use crate::state::use_app;
use crate::ui::common::{ErrorNotice, Icon, TopBar, focus_first, js};
use crate::ui::settings::SettingsPane;
use crate::ui::sources::{AddSourcesMenu, AskAttachments, EmbeddingNotice, refresh_embedding};

/// The composer's text field, focused by Cmd+N (`ui::shell`).
pub const PROMPT_ID: &str = "ask-prompt";

/// Prompts to start from while the library is empty; a click puts one in the composer.
const EXAMPLE_PROMPTS: [&str; 3] = [
    "How do Fourier series approximate a signal?",
    "Linear algebra for machine learning",
    "How does TCP avoid congestion?",
];

pub fn level_label(level: Level) -> &'static str {
    match level {
        Level::Intro => "Intro level",
        Level::Intermediate => "Intermediate",
        Level::Advanced => "Advanced",
    }
}

fn level_value(level: Level) -> &'static str {
    match level {
        Level::Intro => "intro",
        Level::Intermediate => "intermediate",
        Level::Advanced => "advanced",
    }
}

fn parse_level(value: &str) -> Level {
    match value {
        "intermediate" => Level::Intermediate,
        "advanced" => Level::Advanced,
        _ => Level::Intro,
    }
}

#[component]
pub fn AskScreen() -> Element {
    let mut app = use_app();
    let nav = navigator();
    let mut prompt = app.ask_prompt;
    let mut lesson_count = use_signal(|| 3u32);
    let mut level = use_signal(|| Level::Intro);
    let mut with_videos = use_signal(|| false);
    let mut with_quiz = use_signal(|| true);
    let mut subject = use_signal(|| None::<Id>);
    let mut submitting = use_signal(|| false);
    let mut error = use_signal(|| None::<ApiFailure>);
    let mut proposal_id = app.ask_proposal;
    let mut draft = use_signal(|| None::<Outline>);
    // Bumped when code replaces the composer text, to remount the (uncontrolled) textarea.
    let mut prompt_rev = use_signal(|| 0u64);
    let mut accepting = use_signal(|| false);
    let mut accept_error = use_signal(|| None::<ApiFailure>);
    // What the last add of sources left out.
    let mut sources_note = use_signal(|| None::<String>);

    // Whether the service can read sources yet (events keep it current).
    use_future(move || async move {
        let _ = refresh_embedding(app).await;
    });
    // Whether an API key is set, to say so before a prompt is sent rather than after
    // (SettingsChanged events keep it current).
    use_future(move || async move {
        if app.lib.peek().settings.is_some() {
            return;
        }
        let Some(api) = app.api() else { return };
        if let Ok(settings) = api.settings().await {
            app.lib
                .write()
                .apply(margin_shared::Event::SettingsChanged { settings });
        }
    });

    // When the shown proposal is (re)loaded, restore its request into the composer.
    let proposal: Option<Proposal> = proposal_id
        .read()
        .as_ref()
        .and_then(|id| app.lib.read().proposals.get(id).cloned());

    // Seed the editable outline once the draft is ready.
    use_effect(move || {
        let id = proposal_id.read().clone();
        let p = id.and_then(|id| app.lib.read().proposals.get(&id).cloned());
        if let Some(p) = p
            && p.status == ProposalStatus::Ready
            && draft.peek().is_none()
            && let Some(outline) = p.outline
        {
            draft.set(Some(outline));
        }
    });

    // Backup for missed events: refetch a drafting proposal every 3 s.
    use_future(move || async move {
        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let id = proposal_id.peek().clone();
            let Some(id) = id else { continue };
            let drafting = app
                .lib
                .peek()
                .proposals
                .get(&id)
                .is_some_and(|p| p.status == ProposalStatus::Drafting);
            if !drafting {
                continue;
            }
            let Some(api) = app.api() else { return };
            if let Ok(p) = api.proposal(&id).await {
                app.lib
                    .write()
                    .apply(margin_shared::Event::ProposalChanged { proposal: p });
            }
        }
    });

    let request = move || AskRequest {
        prompt: prompt.peek().trim().to_string(),
        lesson_count: *lesson_count.peek(),
        level: *level.peek(),
        with_videos: *with_videos.peek(),
        with_quiz: *with_quiz.peek(),
        parent_topic_id: subject.peek().clone(),
        source_ids: app
            .uploads
            .peek()
            .iter()
            .filter(|u| u.topic_id.is_none())
            .filter_map(|u| match &u.state {
                UploadState::Done(source) => Some(source.id.clone()),
                _ => None,
            })
            .collect(),
    };

    let submit = move |req: AskRequest| {
        let uploading = app
            .uploads
            .peek()
            .iter()
            .any(|u| u.topic_id.is_none() && u.pending());
        let no_key = app
            .lib
            .peek()
            .settings
            .as_ref()
            .is_some_and(|s| !s.has_api_key);
        if req.prompt.is_empty() || *submitting.peek() || uploading || no_key {
            return;
        }
        spawn(async move {
            let Some(api) = app.api() else { return };
            submitting.set(true);
            error.set(None);
            accept_error.set(None);
            match api.ask(&req).await {
                Ok(p) => {
                    draft.set(None);
                    proposal_id.set(Some(p.id.clone()));
                    app.lib
                        .write()
                        .apply(margin_shared::Event::ProposalChanged { proposal: p });
                }
                Err(e) => {
                    app.failure(&e);
                    error.set(Some(e));
                }
            }
            submitting.set(false);
        });
    };

    let lib = app.lib.read();
    let subjects: Vec<(Id, String)> = lib
        .child_topics(None)
        .iter()
        .map(|t| (t.id.clone(), t.title.clone()))
        .collect();
    let subject_name = subject
        .read()
        .as_ref()
        .and_then(|id| lib.topics.get(id).map(|t| t.title.clone()));
    let no_key = lib.settings.as_ref().is_some_and(|s| !s.has_api_key);
    // A first run: offer a few prompts to start from.
    let empty_library =
        lib.topics.is_empty() && *app.lib_status.read() == crate::state::LoadStatus::Loaded;
    drop(lib);

    let (has_sources, uploading) = {
        let uploads = app.uploads.read();
        let mut ask = uploads.iter().filter(|u| u.topic_id.is_none());
        let has = ask.clone().next().is_some();
        (has, ask.any(|u| u.pending()))
    };
    let can_send = !prompt.read().trim().is_empty() && !*submitting.read() && !uploading && !no_key;
    let send_title = if no_key {
        "Add an API key in Settings to draft outlines"
    } else if uploading {
        "Waiting for the sources to upload"
    } else {
        "Draft an outline (Enter)"
    };
    let show_examples = empty_library && prompt.read().trim().is_empty() && proposal.is_none();
    let count = *lesson_count.read();

    rsx! {
        TopBar { crumbs: vec!["Ask".to_string()] }
        div { class: "scroll",
            div { class: "gen",
                h1 { "What do you want to learn?" }
                div { class: "composer",
                    for rev in std::iter::once(prompt_rev()) {
                    textarea {
                        key: "{rev}",
                        id: PROMPT_ID,
                        rows: 2,
                        // Ready to type on arrival, where there's a mouse or trackpad (on a
                        // touch screen it would pop up the keyboard every visit).
                        onmounted: move |_| {
                            let _ = js(&format!("MarginApp.focus('#{PROMPT_ID}', true);"));
                        },
                        "aria-label": "What do you want to learn?",
                        placeholder: "A topic, a question, or what you already know…",
                        initial_value: "{prompt}",
                        oninput: move |e| prompt.set(e.value()),
                        onkeydown: move |e: KeyboardEvent| {
                            if e.key() == Key::Enter && !e.modifiers().shift() && !e.is_composing() {
                                e.prevent_default();
                                submit(request());
                            }
                        },
                    }
                    }
                    div { class: "composer-row",
                        select {
                            class: "chip",
                            "aria-label": "Number of lessons",
                            value: "{count}",
                            onchange: move |e| {
                                if let Ok(n) = e.value().parse::<u32>() {
                                    lesson_count.set(n.clamp(1, 12));
                                }
                            },
                            for n in 1..=12u32 {
                                option { key: "{n}", value: "{n}", selected: n == count,
                                    if n == 1 { "Single lesson" } else { "Course · {n} lessons" }
                                }
                            }
                        }
                        select {
                            class: "chip",
                            "aria-label": "Level",
                            value: level_value(*level.read()),
                            onchange: move |e| level.set(parse_level(&e.value())),
                            for l in [Level::Intro, Level::Intermediate, Level::Advanced] {
                                option { key: "{level_value(l)}", value: level_value(l), selected: *level.read() == l, {level_label(l)} }
                            }
                        }
                        button {
                            class: "chip",
                            "aria-pressed": with_videos.read().to_string(),
                            onclick: move |_| with_videos.toggle(),
                            Icon { name: "video" }
                            "With videos"
                        }
                        button {
                            class: "chip",
                            "aria-pressed": with_quiz.read().to_string(),
                            onclick: move |_| with_quiz.toggle(),
                            // The check marks the state, so it shows only when on.
                            if *with_quiz.read() {
                                Icon { name: "check" }
                            }
                            "Quiz at the end"
                        }
                        select {
                            class: "chip",
                            "aria-label": "Subject",
                            onchange: move |e| {
                                let v = e.value();
                                subject.set(if v.is_empty() { None } else { Some(v) });
                            },
                            option { value: "", selected: subject.read().is_none(), "Any subject" }
                            for (id, title) in subjects {
                                option { key: "{id}", value: "{id}", selected: subject.read().as_deref() == Some(id.as_str()), "{title}" }
                            }
                        }
                        AddSourcesMenu { on_note: move |note| sources_note.set(note) }
                        button {
                            class: "send",
                            "aria-label": "Draft an outline",
                            title: send_title,
                            "aria-describedby": no_key.then_some("ask-no-key"),
                            disabled: !can_send,
                            onclick: move |_| submit(request()),
                            if *submitting.read() {
                                span { class: "spinner" }
                            } else {
                                Icon { name: "up" }
                            }
                        }
                    }
                }
                if no_key {
                    div { id: "ask-no-key", class: "notice", role: "status",
                        Icon { name: "alert" }
                        div { class: "body",
                            span { "Add an API key to draft outlines. Margin needs one for your AI provider before it can write lessons." }
                            div { class: "row-actions",
                                button {
                                    class: "btn btn-secondary btn-sm",
                                    onclick: move |_| app.settings_open.set(Some(SettingsPane::Ai)),
                                    Icon { name: "gear" }
                                    "Open Settings → AI"
                                }
                            }
                        }
                    }
                }
                if show_examples {
                    div { class: "examples", role: "group", "aria-label": "Example prompts",
                        for example in EXAMPLE_PROMPTS {
                            button {
                                key: "{example}",
                                class: "chip",
                                onclick: move |_| {
                                    prompt.set(example.to_string());
                                    prompt_rev += 1;
                                    focus_first(".composer textarea");
                                },
                                "{example}"
                            }
                        }
                    }
                }
                AskAttachments {}
                if let Some(note) = sources_note.read().clone() {
                    div { class: "notice", role: "status",
                        Icon { name: "alert" }
                        div { class: "body", "{note}" }
                        button { class: "icon-btn sm", "aria-label": "Dismiss", onclick: move |_| sources_note.set(None), Icon { name: "x" } }
                    }
                }
                if has_sources {
                    EmbeddingNotice {}
                }
                if let Some(err) = error.read().clone() {
                    ErrorNotice { message: err.message.clone(), settings_pane: err.settings_pane() }
                }
                if let Some(p) = proposal {
                    ProposalView {
                        proposal: p.clone(),
                        draft,
                        subject_name: subject_name.clone(),
                        accepting: *accepting.read(),
                        accept_error: accept_error.read().clone(),
                        on_retry: move |_| {
                            let mut req = p.request.clone();
                            req.prompt = req.prompt.trim().to_string();
                            submit(req)
                        },
                        on_discard: move |_| {
                            proposal_id.set(None);
                            draft.set(None);
                            accept_error.set(None);
                        },
                        on_accept: move |outline: Outline| {
                            let Some(id) = proposal_id.peek().clone() else { return };
                            spawn(async move {
                                let Some(api) = app.api() else { return };
                                accepting.set(true);
                                accept_error.set(None);
                                match api.accept_proposal(&id, &AcceptProposal { outline }).await {
                                    Ok(topic) => {
                                        // Make sure the new lessons are here before opening the first one.
                                        if let Ok(library) = api.library().await {
                                            app.lib.write().replace_library(library);
                                        }
                                        app.lib.write().apply(margin_shared::Event::TopicChanged { topic: topic.clone() });
                                        let first = app.lib.peek().first_lesson_in(&topic.id).map(|l| l.id.clone());
                                        proposal_id.set(None);
                                        draft.set(None);
                                        prompt.set(String::new());
                                        prompt_rev += 1;
                                        // The sources now belong to the new topic.
                                        app.uploads.write().retain(|u| u.topic_id.is_some());
                                        sources_note.set(None);
                                        app.collapsed.write().remove(&topic.id);
                                        match first {
                                            Some(lesson) => nav.push(Route::LessonRead { id: lesson }),
                                            None => nav.push(Route::TopicNotes { topic_id: topic.id }),
                                        };
                                    }
                                    Err(e) => {
                                        app.failure(&e);
                                        accept_error.set(Some(e));
                                    }
                                }
                                accepting.set(false);
                            });
                        },
                    }
                }
            }
        }
    }
}

#[component]
fn ProposalView(
    proposal: Proposal,
    draft: Signal<Option<Outline>>,
    subject_name: Option<String>,
    accepting: bool,
    accept_error: Option<ApiFailure>,
    on_retry: EventHandler<()>,
    on_discard: EventHandler<()>,
    on_accept: EventHandler<Outline>,
) -> Element {
    let nav = navigator();
    match proposal.status.clone() {
        ProposalStatus::Drafting => rsx! {
            div { class: "outline", role: "status",
                div { class: "outline-h",
                    b { "Drafting an outline…" }
                    span { class: "spinner" }
                }
                for i in 0..proposal.request.lesson_count.min(6) {
                    // Placeholder rows: nothing for a screen reader to read.
                    div { key: "{i}", class: "ol-item", "aria-hidden": "true",
                        span { class: "n", "{i + 1}" }
                        div { class: "ol-main",
                            div { class: "skel", style: "width: {55 + (i * 7) % 30}%" }
                            div { class: "skel", style: "width: 40%; height: 10px" }
                        }
                        span {}
                    }
                }
            }
        },
        ProposalStatus::Failed { error } => rsx! {
            ErrorNotice {
                message: format!("Couldn't draft an outline. {error}"),
                on_retry: move |_| on_retry.call(()),
            }
        },
        ProposalStatus::Accepted { topic_id } => rsx! {
            div { class: "notice",
                Icon { name: "check" }
                div { class: "body",
                    span { "These lessons were created." }
                    div { class: "row-actions",
                        button {
                            class: "btn btn-secondary btn-sm",
                            onclick: move |_| {
                                nav.push(Route::TopicNotes { topic_id: topic_id.clone() });
                            },
                            "Open topic notes"
                        }
                        button { class: "btn btn-ghost btn-sm", onclick: move |_| on_discard.call(()), "Start over" }
                    }
                }
            }
        },
        ProposalStatus::Ready => {
            let current = draft.read().clone();
            match current {
                None => rsx! {},
                Some(outline) => rsx! {
                    OutlineEditor {
                        outline,
                        draft,
                        fixed_subject: subject_name.clone().or_else(|| proposal.request.parent_topic_id.as_ref().map(|_| "the chosen subject".to_string())),
                        accepting,
                        accept_error,
                        on_discard,
                        on_accept,
                    }
                },
            }
        }
    }
}

#[component]
fn OutlineEditor(
    outline: Outline,
    draft: Signal<Option<Outline>>,
    fixed_subject: Option<String>,
    accepting: bool,
    accept_error: Option<ApiFailure>,
    on_discard: EventHandler<()>,
    on_accept: EventHandler<Outline>,
) -> Element {
    let mut editing = use_signal(|| false);
    // A stable key per lesson row, moved and removed with it, so a row's (uncontrolled)
    // fields and the focus stay with the lesson.
    let mut keys = use_hook(|| CopyValue::new(RowKeys::default()));
    // The control to focus once the rows have re-rendered after a move, remove or add.
    let mut focus_next = use_signal(|| None::<String>);
    use_effect(move || {
        if focus_next.read().is_none() {
            return;
        }
        if let Some(id) = focus_next.write().take() {
            let id = serde_json::to_string(&format!("#{id}")).unwrap_or_default();
            let _ = js(&format!("MarginApp.focus({id});"));
        }
    });
    let mut edit = move |f: &dyn Fn(&mut Outline)| {
        let mut d = draft.write();
        if let Some(o) = d.as_mut() {
            f(o);
        }
    };
    let total: u32 = outline.lessons.iter().map(|l| l.minutes).sum();
    let n = outline.lessons.len();
    if keys.peek().len() != n {
        keys.write().fit(n);
    }
    let row_keys = keys.peek().keys();
    let issue = first_issue(&outline);
    let topic_missing = outline.topic_title.trim().is_empty();
    let subject_label = fixed_subject
        .clone()
        .unwrap_or_else(|| outline.subject_title.clone());

    rsx! {
        div { class: "outline",
            div { class: "outline-h",
                b { "Proposed outline" }
                small { "{subject_label} · {n} lessons · ~{total} min" }
            }
            if *editing.read() {
                div { class: "outline-fields",
                    div { class: "field",
                        label { r#for: "ol-topic", "Topic" }
                        input {
                            id: "ol-topic",
                            class: "input",
                            placeholder: "Topic title",
                            "aria-invalid": topic_missing.to_string(),
                            "aria-describedby": topic_missing.then_some("ol-topic-err"),
                            initial_value: "{outline.topic_title}",
                            oninput: move |e| { let v = e.value(); edit(&move |o: &mut Outline| o.topic_title = v.clone()) },
                        }
                        if topic_missing {
                            span { id: "ol-topic-err", class: "field-error", "Needs a title" }
                        }
                    }
                    if fixed_subject.is_none() {
                        div { class: "field",
                            label { r#for: "ol-subject", "Subject" }
                            input {
                                id: "ol-subject",
                                class: "input",
                                initial_value: "{outline.subject_title}",
                                oninput: move |e| { let v = e.value(); edit(&move |o: &mut Outline| o.subject_title = v.clone()) },
                            }
                        }
                    }
                }
            } else {
                p { class: "muted", style: "margin: 0 0 12px", "New topic: ", b { style: "font-weight: 500; color: var(--fg)", "{outline.topic_title}" } }
            }
            for (i, (lesson, key)) in outline.lessons.iter().cloned().zip(row_keys).enumerate() {
                div { key: "{key}", class: if *editing.read() { "ol-item editing" } else { "ol-item" },
                    span { class: "n", "{i + 1}" }
                    if *editing.read() {
                        div { class: "ol-main",
                            input {
                                id: row_control(key, "title"),
                                class: "input ol-title",
                                "aria-label": "Lesson {i + 1} title",
                                placeholder: "Lesson title",
                                "aria-invalid": lesson.title.trim().is_empty().to_string(),
                                "aria-describedby": lesson.title.trim().is_empty().then(|| format!("ol-title-{i}-err")),
                                initial_value: "{lesson.title}",
                                oninput: move |e| { let v = e.value(); edit(&move |o: &mut Outline| o.lessons[i].title = v.clone()) },
                            }
                            if lesson.title.trim().is_empty() {
                                span { id: "ol-title-{i}-err", class: "field-error", "Needs a title" }
                            }
                            input {
                                class: "input",
                                "aria-label": "Lesson {i + 1} summary",
                                placeholder: "One-line summary",
                                initial_value: "{lesson.summary}",
                                oninput: move |e| { let v = e.value(); edit(&move |o: &mut Outline| o.lessons[i].summary = v.clone()) },
                            }
                        }
                        div { class: "ol-tools",
                            input {
                                class: "input mins",
                                r#type: "number",
                                min: "1",
                                max: "180",
                                "aria-label": "Lesson {i + 1} minutes",
                                initial_value: "{lesson.minutes}",
                                oninput: move |e| {
                                    if let Ok(m) = e.value().parse::<u32>() {
                                        edit(&move |o: &mut Outline| o.lessons[i].minutes = m.clamp(1, 180));
                                    }
                                },
                            }
                            button {
                                id: row_control(key, "up"),
                                class: "icon-btn",
                                "aria-label": "Move lesson {i + 1} up",
                                disabled: i == 0,
                                onclick: move |_| {
                                    edit(&move |o: &mut Outline| o.lessons.swap(i, i - 1));
                                    keys.write().swap(i, i - 1);
                                    focus_next.set(Some(row_control(key, after_move(i - 1, n, true))));
                                },
                                Icon { name: "up" }
                            }
                            button {
                                id: row_control(key, "down"),
                                class: "icon-btn",
                                "aria-label": "Move lesson {i + 1} down",
                                disabled: i + 1 == n,
                                onclick: move |_| {
                                    edit(&move |o: &mut Outline| o.lessons.swap(i, i + 1));
                                    keys.write().swap(i, i + 1);
                                    focus_next.set(Some(row_control(key, after_move(i + 1, n, false))));
                                },
                                Icon { name: "down" }
                            }
                            button {
                                class: "icon-btn ol-del",
                                "aria-label": "Remove lesson {i + 1}",
                                disabled: n == 1,
                                onclick: move |_| {
                                    edit(&move |o: &mut Outline| { o.lessons.remove(i); });
                                    let next = {
                                        let mut k = keys.write();
                                        k.remove(i);
                                        k.at(i.min(n - 2))
                                    };
                                    focus_next.set(next.map(|k| row_control(k, "title")));
                                },
                                Icon { name: "trash" }
                            }
                        }
                    } else {
                        div { class: "ol-view",
                            b { "{lesson.title}" }
                            p { "{lesson.summary}" }
                        }
                        span { class: "n", style: "padding-top: 2px", "~{lesson.minutes} min" }
                    }
                }
            }
            if *editing.read() && n < 12 {
                div { style: "padding-top: 10px",
                    button {
                        id: "ol-add",
                        class: "btn btn-ghost btn-sm",
                        onclick: move |_| {
                            edit(&|o: &mut Outline| o.lessons.push(OutlineLesson { title: String::new(), summary: String::new(), minutes: 10 }));
                            let key = keys.write().push();
                            focus_next.set(Some(row_control(key, "title")));
                        },
                        Icon { name: "plus" }
                        "Add lesson"
                    }
                }
            }
            if let Some(err) = accept_error {
                div { style: "padding-top: 12px",
                    ErrorNotice { message: err.message.clone(), settings_pane: err.settings_pane() }
                }
            }
            div { class: "outline-f",
                // Why "Create lessons" won't go yet (it also jumps to the field).
                if let Some(issue) = &issue {
                    span { class: "field-error", role: "status", style: "margin-right: auto; align-self: center", {issue.message()} }
                }
                button { class: "btn btn-ghost", disabled: accepting, onclick: move |_| on_discard.call(()), "Discard" }
                button {
                    class: "btn btn-secondary",
                    disabled: accepting,
                    "aria-pressed": editing.read().to_string(),
                    onclick: move |_| editing.toggle(),
                    if *editing.read() { "Done editing" } else { "Edit outline" }
                }
                button {
                    class: "btn btn-primary",
                    disabled: accepting,
                    "aria-busy": accepting.to_string(),
                    onclick: move |_| {
                        if accepting {
                            return;
                        }
                        if let Some(issue) = first_issue(&outline) {
                            editing.set(true);
                            focus_first(&issue.selector(&keys.peek()));
                            return;
                        }
                        let o = outline.clone();
                        let cleaned = Outline {
                            topic_title: o.topic_title.trim().to_string(),
                            subject_title: o.subject_title.trim().to_string(),
                            lessons: o.lessons.into_iter().map(|l| OutlineLesson { title: l.title.trim().to_string(), summary: l.summary.trim().to_string(), minutes: l.minutes }).collect(),
                        };
                        on_accept.call(cleaned)
                    },
                    if accepting {
                        span { class: "spinner" }
                        "Creating lessons…"
                    } else {
                        "Create lessons"
                    }
                }
            }
        }
    }
}

/// Stable keys for the outline editor's lesson rows, kept in the rows' order.
#[derive(Clone, Debug, Default, PartialEq)]
struct RowKeys {
    keys: Vec<u64>,
    next: u64,
}

impl RowKeys {
    fn len(&self) -> usize {
        self.keys.len()
    }

    fn keys(&self) -> Vec<u64> {
        self.keys.clone()
    }

    fn at(&self, i: usize) -> Option<u64> {
        self.keys.get(i).copied()
    }

    /// A new row at the end; returns its key.
    fn push(&mut self) -> u64 {
        let key = self.next;
        self.next += 1;
        self.keys.push(key);
        key
    }

    fn swap(&mut self, a: usize, b: usize) {
        if a < self.keys.len() && b < self.keys.len() {
            self.keys.swap(a, b);
        }
    }

    fn remove(&mut self, i: usize) {
        if i < self.keys.len() {
            self.keys.remove(i);
        }
    }

    /// Match `n` rows (a new draft): keep the keys there are, add or drop at the end.
    fn fit(&mut self, n: usize) {
        self.keys.truncate(n);
        while self.keys.len() < n {
            self.push();
        }
    }
}

/// What keeps an outline from being created, in the order the fields appear.
#[derive(Debug, Clone, PartialEq)]
enum OutlineIssue {
    TopicTitle,
    NoLessons,
    LessonTitle(usize),
}

impl OutlineIssue {
    fn message(&self) -> String {
        match self {
            OutlineIssue::TopicTitle => "The topic needs a title.".into(),
            OutlineIssue::NoLessons => "Add at least one lesson.".into(),
            OutlineIssue::LessonTitle(i) => format!("Lesson {} needs a title.", i + 1),
        }
    }

    /// The field to fix (shown in edit mode); `keys` are the rows' keys, in order.
    fn selector(&self, keys: &RowKeys) -> String {
        match self {
            OutlineIssue::TopicTitle => "#ol-topic".into(),
            OutlineIssue::NoLessons => "#ol-add".into(),
            OutlineIssue::LessonTitle(i) => match keys.at(*i) {
                Some(key) => format!("#{}", row_control(key, "title")),
                None => "#ol-add".into(),
            },
        }
    }
}

/// The id of a row's control: "title", "up" or "down".
fn row_control(key: u64, part: &str) -> String {
    format!("ol-{key}-{part}")
}

/// After a lesson moves (`up` or down) to row `to` of `n`, which of its move buttons
/// takes the focus: the one just used while it can go on, else the other (the first
/// row can't go up, the last can't go down).
fn after_move(to: usize, n: usize, up: bool) -> &'static str {
    match (up, to == 0, to + 1 == n) {
        (true, false, _) => "up",
        (true, true, _) => "down",
        (false, _, false) => "down",
        (false, _, true) => "up",
    }
}

fn first_issue(outline: &Outline) -> Option<OutlineIssue> {
    if outline.topic_title.trim().is_empty() {
        return Some(OutlineIssue::TopicTitle);
    }
    if outline.lessons.is_empty() {
        return Some(OutlineIssue::NoLessons);
    }
    outline
        .lessons
        .iter()
        .position(|l| l.title.trim().is_empty())
        .map(OutlineIssue::LessonTitle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_keys_follow_their_lessons() {
        let mut keys = RowKeys::default();
        keys.fit(3);
        assert_eq!(keys.keys(), vec![0, 1, 2]);
        keys.swap(0, 1);
        assert_eq!(keys.keys(), vec![1, 0, 2]);
        keys.remove(1);
        assert_eq!(keys.keys(), vec![1, 2]);
        // New rows never reuse a key, so a fresh field never inherits an old one.
        assert_eq!(keys.push(), 3);
        assert_eq!(keys.keys(), vec![1, 2, 3]);
        keys.fit(2);
        assert_eq!(keys.keys(), vec![1, 2]);
        assert_eq!(keys.at(1), Some(2));
        assert_eq!(keys.at(5), None);
        // Out of range is ignored.
        keys.swap(0, 9);
        keys.remove(9);
        assert_eq!(keys.len(), 2);
        assert_eq!(row_control(3, "title"), "ol-3-title");
    }

    #[test]
    fn focus_stays_on_a_usable_move_button() {
        // Up, still room to go up: keep "up"; reached the top: "down".
        assert_eq!(after_move(1, 4, true), "up");
        assert_eq!(after_move(0, 4, true), "down");
        // Down, still room: "down"; reached the bottom: "up".
        assert_eq!(after_move(2, 4, false), "down");
        assert_eq!(after_move(3, 4, false), "up");
        // Two rows: every move ends at an edge.
        assert_eq!(after_move(0, 2, true), "down");
        assert_eq!(after_move(1, 2, false), "up");
    }

    fn lesson(title: &str) -> OutlineLesson {
        OutlineLesson {
            title: title.into(),
            summary: String::new(),
            minutes: 10,
        }
    }

    #[test]
    fn first_issue_points_at_the_first_empty_field() {
        let mut o = Outline {
            topic_title: "Fourier".into(),
            subject_title: "Maths".into(),
            lessons: vec![lesson("Series"), lesson("  "), lesson("")],
        };
        assert_eq!(first_issue(&o), Some(OutlineIssue::LessonTitle(1)));
        let mut keys = RowKeys::default();
        keys.fit(3);
        keys.swap(0, 1);
        assert_eq!(OutlineIssue::LessonTitle(1).selector(&keys), "#ol-0-title");
        assert_eq!(OutlineIssue::LessonTitle(7).selector(&keys), "#ol-add");
        assert_eq!(
            OutlineIssue::LessonTitle(1).message(),
            "Lesson 2 needs a title."
        );
        o.lessons.truncate(1);
        assert_eq!(first_issue(&o), None);
        o.topic_title = " ".into();
        assert_eq!(first_issue(&o), Some(OutlineIssue::TopicTitle));
        o.topic_title = "Fourier".into();
        o.lessons.clear();
        assert_eq!(first_issue(&o), Some(OutlineIssue::NoLessons));
    }
}
