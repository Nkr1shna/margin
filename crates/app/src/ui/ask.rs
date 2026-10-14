//! Ask: the composer (prompt, lesson count, level, videos, quiz, subject, sources), the
//! drafted outline with inline editing, and "Create lessons".

use std::time::Duration;

use dioxus::prelude::*;
use margin_shared::{AcceptProposal, AskRequest, Id, Level, Outline, Proposal, ProposalStatus};

use crate::api::ApiFailure;
use crate::routes::Route;
use crate::sources::UploadState;
use crate::state::use_app;
use crate::ui::common::{ErrorNotice, Icon, TopBar, focus_first, js};
use crate::ui::outline::ProposalView;
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

/// The composer that drafted outlines before Ask became a chat. Not routed: the chat's
/// outline card takes its place (`propose_course`), and it goes with `Api::ask`.
#[allow(dead_code)]
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
                        on_retry: EventHandler::new(move |_| {
                            let mut req = p.request.clone();
                            req.prompt = req.prompt.trim().to_string();
                            submit(req)
                        }),
                        on_discard: EventHandler::new(move |_| {
                            proposal_id.set(None);
                            draft.set(None);
                            accept_error.set(None);
                        }),
                        on_accept: move |outline: Outline| {
                            let Some(id) = proposal_id.peek().clone() else { return };
                            spawn(async move {
                                let Some(api) = app.api() else { return };
                                accepting.set(true);
                                accept_error.set(None);
                                match api.accept_proposal(&id, &AcceptProposal {
                outline,
                chips: Default::default(),
            }).await {
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
