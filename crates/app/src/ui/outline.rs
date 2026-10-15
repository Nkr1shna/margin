//! A course outline the agent proposed (`propose_course`), as the chat's outline card:
//! the editor (topic, lessons with titles, summaries and minutes, reorder, add, remove),
//! the chips for the course's options (level, videos, quiz), "Create lessons", and once
//! created, the course itself with links to its lessons.

use dioxus::prelude::*;
use margin_shared::{
    AskRequest, LessonStatus, Level, Outline, OutlineChips, OutlineLesson, Proposal,
    ProposalStatus, VideoStatus,
};

use crate::api::ApiFailure;
use crate::routes::Route;
use crate::state::{LoadStatus, use_app};
use crate::ui::audience::ProposalAudience;
use crate::ui::common::{ErrorNotice, Icon, Loading, focus_first, js, reveal_topic};

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

/// What the outline card's chips say the course is made with. Starts as the proposal
/// asked (what the owner said, or what the agent inferred).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CourseOptions {
    pub level: Level,
    pub with_videos: bool,
    pub with_quiz: bool,
}

impl CourseOptions {
    pub fn of(request: &AskRequest) -> Self {
        Self {
            level: request.level,
            with_videos: request.with_videos,
            with_quiz: request.with_quiz,
        }
    }

    /// The chips that differ from what `request` asked for (`None` keeps the proposal's).
    pub fn chips(&self, request: &AskRequest) -> OutlineChips {
        fn changed<T: PartialEq>(now: T, then: T) -> Option<T> {
            (now != then).then_some(now)
        }
        OutlineChips {
            level: changed(self.level, request.level),
            with_videos: changed(self.with_videos, request.with_videos),
            with_quiz: changed(self.with_quiz, request.with_quiz),
        }
    }
}

/// An outline card's edits before Create lessons. Kept in the app
/// (`AppCtx::outline_drafts`), so they survive a look elsewhere.
#[derive(Debug, Clone, PartialEq)]
pub struct OutlineDraft {
    pub outline: Outline,
    pub options: CourseOptions,
}

impl OutlineDraft {
    pub fn of(proposal: &Proposal) -> Option<Self> {
        Some(Self {
            outline: proposal.outline.clone()?,
            options: CourseOptions::of(&proposal.request),
        })
    }

    /// The outline as it is sent: every text trimmed.
    pub fn cleaned(&self) -> Outline {
        let o = &self.outline;
        Outline {
            topic_title: o.topic_title.trim().to_string(),
            subject_title: o.subject_title.trim().to_string(),
            lessons: o
                .lessons
                .iter()
                .map(|l| OutlineLesson {
                    title: l.title.trim().to_string(),
                    summary: l.summary.trim().to_string(),
                    minutes: l.minutes,
                })
                .collect(),
        }
    }
}

/// A proposal in whichever state it is: being drafted, failed, ready to edit and create,
/// created, or replaced by a newer outline in the chat.
#[component]
pub fn ProposalView(
    proposal: Proposal,
    draft: Signal<Option<OutlineDraft>>,
    /// The subject the proposal is filed under, when it names one (`parent_topic_id`)
    /// that is still in the library.
    subject_name: Option<String>,
    /// The proposal named a subject (`parent_topic_id`) that has since been deleted: the
    /// course goes under the outline's subject title instead.
    subject_gone: bool,
    accepting: bool,
    accept_error: Option<ApiFailure>,
    on_accept: EventHandler<OutlineDraft>,
) -> Element {
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
            ErrorNotice { message: format!("Couldn't draft an outline. {error}") }
        },
        // Only the chat's newest outline can create a course: this one stays as it was
        // proposed, inactive.
        ProposalStatus::Superseded { .. } => rsx! {
            div { class: "outline",
                div { class: "outline-h",
                    b { "Earlier outline" }
                    if let Some(o) = &proposal.outline {
                        small { "{o.lessons.len()} lessons · ~{o.lessons.iter().map(|l| l.minutes).sum::<u32>()} min" }
                    }
                }
                p { class: "muted", role: "status", style: "margin: 0 0 12px", "A newer outline replaced this one." }
                if let Some(o) = proposal.outline.clone() {
                    p { class: "muted", style: "margin: 0 0 12px", "Topic: ", b { style: "font-weight: 500; color: var(--fg)", "{o.topic_title}" } }
                    for (i, lesson) in o.lessons.iter().enumerate() {
                        div { key: "{i}", class: "ol-item",
                            span { class: "n", "{i + 1}" }
                            div { class: "ol-view",
                                b { "{lesson.title}" }
                                p { "{lesson.summary}" }
                            }
                            span { class: "n", style: "padding-top: 2px", "~{lesson.minutes} min" }
                        }
                    }
                }
            }
        },
        ProposalStatus::Accepted { topic_id } => rsx! {
            CourseView {
                topic_id,
                fallback_title: proposal.outline.as_ref().map(|o| o.topic_title.clone()).unwrap_or_default(),
            }
        },
        ProposalStatus::Ready => {
            let current = draft.read().clone();
            match current {
                None => rsx! {},
                Some(d) => rsx! {
                    ProposalAudience { proposal: proposal.clone() }
                    OutlineEditor {
                        scope: proposal.id.clone(),
                        outline: d.outline,
                        options: d.options,
                        draft,
                        fixed_subject: subject_name.clone(),
                        subject_gone,
                        accepting,
                        accept_error,
                        on_accept,
                    }
                },
            }
        }
    }
}

/// What a created course's lesson is doing, for its row on the card.
pub fn lesson_state(status: &LessonStatus, video: Option<&VideoStatus>) -> &'static str {
    match (status, video) {
        (LessonStatus::Queued, _) => "Waiting to be written",
        (LessonStatus::Generating, _) => "Writing…",
        (LessonStatus::Failed { .. }, _) => "Couldn't be written",
        (LessonStatus::Ready, Some(VideoStatus::Ready) | None) => "Ready",
        (LessonStatus::Ready, Some(VideoStatus::Failed { .. })) => "Ready · the video failed",
        (LessonStatus::Ready, Some(_)) => "Ready · making the video…",
    }
}

/// The course an outline became, as it is now in the library: its lessons as they're
/// written, each opening its lesson, and the topic to open or show.
#[component]
fn CourseView(topic_id: String, fallback_title: String) -> Element {
    let app = use_app();
    let nav = navigator();
    let lib = app.lib.read();
    let Some(topic) = lib.topics.get(&topic_id).cloned() else {
        let loaded = *app.lib_status.read() == LoadStatus::Loaded;
        drop(lib);
        return if loaded {
            rsx! {
                div { class: "notice", role: "status",
                    Icon { name: "check" }
                    div { class: "body", "The course “{fallback_title}” was created, and has since been deleted." }
                }
            }
        } else {
            rsx! { Loading { label: "Loading the course…" } }
        };
    };
    let subject = lib
        .topic_path(&topic_id)
        .iter()
        .rev()
        .nth(1)
        .map(|t| t.title.clone());
    // (id, title, minutes, what it's doing, written)
    let lessons: Vec<(String, String, u32, &'static str, bool)> = lib
        .lessons_in(&topic_id)
        .iter()
        .map(|l| {
            let video = lib.video_for_lesson(&l.id).map(|v| &v.status);
            (
                l.id.clone(),
                l.title.clone(),
                l.minutes,
                lesson_state(&l.status, video),
                l.status == LessonStatus::Ready,
            )
        })
        .collect();
    drop(lib);
    let n = lessons.len();
    let written = lessons.iter().filter(|l| l.4).count();
    let first = lessons.first().map(|l| l.0.clone());
    let count = match n {
        1 => "1 lesson".to_string(),
        n => format!("{n} lessons"),
    };
    let progress = if written == n {
        "all written".to_string()
    } else {
        format!("{written} written")
    };
    let notes_topic = topic_id.clone();
    rsx! {
        div { class: "outline course",
            div { class: "outline-h",
                b { "Course created" }
                small {
                    if let Some(s) = &subject { "{s} · " }
                    "{count} · {progress}"
                }
            }
            p { class: "muted", style: "margin: 0 0 12px", "New topic: ", b { style: "font-weight: 500; color: var(--fg)", "{topic.title}" } }
            ol { class: "course-lessons", "aria-label": "Lessons in {topic.title}",
                for (i, (id, title, minutes, state, _)) in lessons.into_iter().enumerate() {
                    li { key: "{id}",
                        button {
                            class: "ol-item ol-link",
                            onclick: move |_| {
                                nav.push(Route::LessonRead { id: id.clone() });
                            },
                            span { class: "n", "{i + 1}" }
                            span { class: "ol-view",
                                b { "{title}" }
                                span { class: "ol-state", "{state}" }
                            }
                            span { class: "n", "~{minutes} min" }
                        }
                    }
                }
            }
            div { class: "outline-f",
                button {
                    class: "btn btn-ghost",
                    onclick: move |_| reveal_topic(app, &topic_id),
                    "Show in sidebar"
                }
                button {
                    class: "btn btn-secondary",
                    onclick: move |_| {
                        nav.push(Route::TopicNotes { topic_id: notes_topic.clone() });
                    },
                    "Topic notes"
                }
                if let Some(first) = first {
                    button {
                        id: course_open_id(&topic_id),
                        class: "btn btn-primary",
                        onclick: move |_| {
                            nav.push(Route::LessonRead { id: first.clone() });
                        },
                        "Open the first lesson"
                    }
                }
            }
        }
    }
}

/// The id of a created course's "Open the first lesson" button, which takes the focus
/// once Create lessons has made the course.
pub fn course_open_id(topic_id: &str) -> String {
    let id: String = topic_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("course-{id}-open")
}

/// The editable outline with the course's chips and Create lessons. `scope` (the
/// proposal's id) keeps its controls' ids apart from another card's in the same chat.
#[component]
pub fn OutlineEditor(
    scope: String,
    outline: Outline,
    options: CourseOptions,
    draft: Signal<Option<OutlineDraft>>,
    fixed_subject: Option<String>,
    /// The subject the proposal named was deleted (see `ProposalView`).
    subject_gone: bool,
    accepting: bool,
    accept_error: Option<ApiFailure>,
    on_accept: EventHandler<OutlineDraft>,
) -> Element {
    let ids = Ids::new(&scope);
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
        if let Some(d) = d.as_mut() {
            f(&mut d.outline);
        }
    };
    let mut set_options = move |f: &dyn Fn(&mut CourseOptions)| {
        let mut d = draft.write();
        if let Some(d) = d.as_mut() {
            f(&mut d.options);
        }
    };
    let total: u32 = outline.lessons.iter().map(|l| l.minutes).sum();
    let n = outline.lessons.len();
    if keys.peek().len() != n {
        keys.write().fit(n);
    }
    let row_keys = keys.peek().keys();
    let needs_subject = fixed_subject.is_none();
    let issue = first_issue(&outline, needs_subject);
    let topic_missing = outline.topic_title.trim().is_empty();
    let subject_missing = needs_subject && outline.subject_title.trim().is_empty();
    let subject_label = fixed_subject
        .clone()
        .unwrap_or_else(|| outline.subject_title.trim().to_string());
    let (topic_id, topic_err, subject_id, subject_err, add_id) = (
        ids.topic(),
        format!("{}-err", ids.topic()),
        ids.subject(),
        format!("{}-err", ids.subject()),
        ids.add(),
    );
    let gone_notice = match outline.subject_title.trim() {
        "" => "The subject this course was going into has been deleted. Give it a subject under Edit outline.".to_string(),
        s => format!("The subject this course was going into has been deleted. It will go under “{s}” instead; you can change that under Edit outline."),
    };

    rsx! {
        div { class: "outline",
            div { class: "outline-h",
                b { "Proposed outline" }
                small {
                    if !subject_label.is_empty() { "{subject_label} · " }
                    "{n} lessons · ~{total} min"
                }
            }
            if subject_gone && needs_subject {
                div { class: "notice", role: "status", style: "margin-bottom: 12px",
                    Icon { name: "alert" }
                    div { class: "body", "{gone_notice}" }
                }
            }
            if *editing.read() {
                div { class: "outline-fields",
                    div { class: "field",
                        label { r#for: "{topic_id}", "Topic" }
                        input {
                            id: "{topic_id}",
                            class: "input",
                            placeholder: "Topic title",
                            disabled: accepting,
                            "aria-invalid": topic_missing.to_string(),
                            "aria-describedby": topic_missing.then(|| topic_err.clone()),
                            initial_value: "{outline.topic_title}",
                            oninput: move |e| { let v = e.value(); edit(&move |o: &mut Outline| o.topic_title = v.clone()) },
                        }
                        if topic_missing {
                            span { id: "{topic_err}", class: "field-error", "Needs a title" }
                        }
                    }
                    if needs_subject {
                        div { class: "field",
                            label { r#for: "{subject_id}", "Subject" }
                            input {
                                id: "{subject_id}",
                                class: "input",
                                placeholder: "Subject title",
                                disabled: accepting,
                                "aria-invalid": subject_missing.to_string(),
                                "aria-describedby": subject_missing.then(|| subject_err.clone()),
                                initial_value: "{outline.subject_title}",
                                oninput: move |e| { let v = e.value(); edit(&move |o: &mut Outline| o.subject_title = v.clone()) },
                            }
                            if subject_missing {
                                span { id: "{subject_err}", class: "field-error", "Needs a title" }
                            }
                        }
                    }
                }
            } else {
                p { class: "muted", style: "margin: 0 0 12px", "New topic: ", b { style: "font-weight: 500; color: var(--fg)", "{outline.topic_title}" } }
            }
            // The handlers below use the row index from this render; the draft may have
            // changed since (two quick clicks), so they check it is still in range.
            for (i, (lesson, key)) in outline.lessons.iter().cloned().zip(row_keys).enumerate() {
                div { key: "{key}", class: if *editing.read() { "ol-item editing" } else { "ol-item" },
                    span { class: "n", "{i + 1}" }
                    if *editing.read() {
                        div { class: "ol-main",
                            input {
                                id: ids.row(key, "title"),
                                class: "input ol-title",
                                "aria-label": "Lesson {i + 1} title",
                                placeholder: "Lesson title",
                                disabled: accepting,
                                "aria-invalid": lesson.title.trim().is_empty().to_string(),
                                "aria-describedby": lesson.title.trim().is_empty().then(|| ids.row(key, "err")),
                                initial_value: "{lesson.title}",
                                oninput: move |e| { let v = e.value(); edit(&move |o: &mut Outline| if let Some(l) = o.lessons.get_mut(i) { l.title = v.clone() }) },
                            }
                            if lesson.title.trim().is_empty() {
                                span { id: ids.row(key, "err"), class: "field-error", "Needs a title" }
                            }
                            input {
                                class: "input",
                                "aria-label": "Lesson {i + 1} summary",
                                placeholder: "One-line summary",
                                disabled: accepting,
                                initial_value: "{lesson.summary}",
                                oninput: move |e| { let v = e.value(); edit(&move |o: &mut Outline| if let Some(l) = o.lessons.get_mut(i) { l.summary = v.clone() }) },
                            }
                        }
                        div { class: "ol-tools",
                            input {
                                class: "input mins",
                                r#type: "number",
                                min: "1",
                                max: "180",
                                "aria-label": "Lesson {i + 1} minutes",
                                disabled: accepting,
                                initial_value: "{lesson.minutes}",
                                oninput: move |e| {
                                    if let Ok(m) = e.value().parse::<u32>() {
                                        edit(&move |o: &mut Outline| if let Some(l) = o.lessons.get_mut(i) { l.minutes = m.clamp(1, 180) });
                                    }
                                },
                            }
                            button {
                                id: ids.row(key, "up"),
                                class: "icon-btn",
                                "aria-label": "Move lesson {i + 1} up",
                                disabled: accepting || i == 0,
                                onclick: {
                                    let ids = ids.clone();
                                    move |_| {
                                        edit(&move |o: &mut Outline| if i > 0 && i < o.lessons.len() { o.lessons.swap(i, i - 1) });
                                        keys.write().swap(i, i.saturating_sub(1));
                                        focus_next.set(Some(ids.row(key, after_move(i.saturating_sub(1), n, true))));
                                    }
                                },
                                Icon { name: "up" }
                            }
                            button {
                                id: ids.row(key, "down"),
                                class: "icon-btn",
                                "aria-label": "Move lesson {i + 1} down",
                                disabled: accepting || i + 1 == n,
                                onclick: {
                                    let ids = ids.clone();
                                    move |_| {
                                        edit(&move |o: &mut Outline| if i + 1 < o.lessons.len() { o.lessons.swap(i, i + 1) });
                                        keys.write().swap(i, i + 1);
                                        focus_next.set(Some(ids.row(key, after_move(i + 1, n, false))));
                                    }
                                },
                                Icon { name: "down" }
                            }
                            button {
                                class: "icon-btn ol-del",
                                "aria-label": "Remove lesson {i + 1}",
                                disabled: accepting || n == 1,
                                onclick: {
                                    let ids = ids.clone();
                                    move |_| {
                                        edit(&move |o: &mut Outline| if i < o.lessons.len() && o.lessons.len() > 1 { o.lessons.remove(i); });
                                        let next = {
                                            let mut k = keys.write();
                                            k.remove(i);
                                            k.at(i.min(n.saturating_sub(2)))
                                        };
                                        focus_next.set(next.map(|k| ids.row(k, "title")));
                                    }
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
                        id: "{add_id}",
                        class: "btn btn-ghost btn-sm",
                        disabled: accepting,
                        onclick: {
                            let ids = ids.clone();
                            move |_| {
                                edit(&|o: &mut Outline| o.lessons.push(OutlineLesson { title: String::new(), summary: String::new(), minutes: 10 }));
                                let key = keys.write().push();
                                focus_next.set(Some(ids.row(key, "title")));
                            }
                        },
                        Icon { name: "plus" }
                        "Add lesson"
                    }
                }
            }
            // The course's options: what the agent understood, changed here before the
            // lessons are created.
            div { class: "composer-row outline-opts", role: "group", "aria-label": "Course options",
                select {
                    class: "chip",
                    "aria-label": "Level",
                    disabled: accepting,
                    value: level_value(options.level),
                    onchange: move |e| {
                        let level = parse_level(&e.value());
                        set_options(&move |o: &mut CourseOptions| o.level = level)
                    },
                    for l in [Level::Intro, Level::Intermediate, Level::Advanced] {
                        option { key: "{level_value(l)}", value: level_value(l), selected: options.level == l, {level_label(l)} }
                    }
                }
                button {
                    class: "chip",
                    "aria-pressed": options.with_videos.to_string(),
                    disabled: accepting,
                    onclick: move |_| set_options(&|o: &mut CourseOptions| o.with_videos = !o.with_videos),
                    Icon { name: "video" }
                    "With videos"
                }
                button {
                    class: "chip",
                    "aria-pressed": options.with_quiz.to_string(),
                    disabled: accepting,
                    onclick: move |_| set_options(&|o: &mut CourseOptions| o.with_quiz = !o.with_quiz),
                    // The check marks the state, so it shows only when on.
                    if options.with_quiz {
                        Icon { name: "check" }
                    }
                    "Quiz at the end"
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
                        let Some(d) = draft.peek().clone() else { return };
                        if let Some(issue) = first_issue(&d.outline, needs_subject) {
                            editing.set(true);
                            focus_first(&issue.selector(&ids, &keys.peek()));
                            return;
                        }
                        on_accept.call(d)
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

/// The ids of one outline editor's controls, kept apart from another card's.
#[derive(Clone, Debug, PartialEq)]
struct Ids {
    prefix: String,
}

impl Ids {
    fn new(scope: &str) -> Self {
        // Only letters, digits and dashes, so each id is also a plain CSS selector.
        let scope: String = scope
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        Self {
            prefix: format!("ol-{scope}"),
        }
    }

    fn topic(&self) -> String {
        format!("{}-topic", self.prefix)
    }

    fn subject(&self) -> String {
        format!("{}-subject", self.prefix)
    }

    fn add(&self) -> String {
        format!("{}-add", self.prefix)
    }

    /// The id of a row's control: "title", "err", "up" or "down".
    fn row(&self, key: u64, part: &str) -> String {
        format!("{}-{key}-{part}", self.prefix)
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
    SubjectTitle,
    NoLessons,
    LessonTitle(usize),
}

impl OutlineIssue {
    fn message(&self) -> String {
        match self {
            OutlineIssue::TopicTitle => "The topic needs a title.".into(),
            OutlineIssue::SubjectTitle => "The subject needs a title.".into(),
            OutlineIssue::NoLessons => "Add at least one lesson.".into(),
            OutlineIssue::LessonTitle(i) => format!("Lesson {} needs a title.", i + 1),
        }
    }

    /// The field to fix (shown in edit mode); `keys` are the rows' keys, in order.
    fn selector(&self, ids: &Ids, keys: &RowKeys) -> String {
        match self {
            OutlineIssue::TopicTitle => format!("#{}", ids.topic()),
            OutlineIssue::SubjectTitle => format!("#{}", ids.subject()),
            OutlineIssue::NoLessons => format!("#{}", ids.add()),
            OutlineIssue::LessonTitle(i) => match keys.at(*i) {
                Some(key) => format!("#{}", ids.row(key, "title")),
                None => format!("#{}", ids.add()),
            },
        }
    }
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

/// `needs_subject`: the course isn't filed under an existing topic, so the outline's
/// subject title names (or makes) its subject.
fn first_issue(outline: &Outline, needs_subject: bool) -> Option<OutlineIssue> {
    if outline.topic_title.trim().is_empty() {
        return Some(OutlineIssue::TopicTitle);
    }
    if needs_subject && outline.subject_title.trim().is_empty() {
        return Some(OutlineIssue::SubjectTitle);
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
    }

    #[test]
    fn ids_are_kept_apart_per_card_and_usable_as_selectors() {
        let ids = Ids::new("3f2a-9c:x");
        assert_eq!(ids.row(3, "title"), "ol-3f2a-9c-x-3-title");
        assert_eq!(ids.topic(), "ol-3f2a-9c-x-topic");
        assert_eq!(ids.add(), "ol-3f2a-9c-x-add");
        assert_ne!(Ids::new("p1").topic(), Ids::new("p2").topic());
    }

    fn request(level: Level, with_videos: bool, with_quiz: bool) -> AskRequest {
        AskRequest {
            prompt: "Fourier".into(),
            lesson_count: 3,
            level,
            with_videos,
            with_quiz,
            parent_topic_id: None,
            source_ids: vec![],
        }
    }

    #[test]
    fn chips_send_only_what_changed() {
        let req = request(Level::Intro, false, true);
        let mut options = CourseOptions::of(&req);
        assert_eq!(options.chips(&req), OutlineChips::default());
        options.level = Level::Advanced;
        options.with_videos = true;
        assert_eq!(
            options.chips(&req),
            OutlineChips {
                level: Some(Level::Advanced),
                with_videos: Some(true),
                with_quiz: None,
            }
        );
        // Changed and changed back: nothing to override.
        options.with_quiz = false;
        options.with_quiz = true;
        assert_eq!(options.chips(&req).with_quiz, None);
    }

    #[test]
    fn drafts_send_trimmed_text() {
        let draft = OutlineDraft {
            outline: Outline {
                topic_title: " Fourier ".into(),
                subject_title: "Maths ".into(),
                lessons: vec![OutlineLesson {
                    title: " Series".into(),
                    summary: "Why ".into(),
                    minutes: 9,
                }],
            },
            options: CourseOptions::of(&request(Level::Intro, false, false)),
        };
        let o = draft.cleaned();
        assert_eq!(o.topic_title, "Fourier");
        assert_eq!(o.subject_title, "Maths");
        assert_eq!(o.lessons[0].title, "Series");
        assert_eq!(o.lessons[0].summary, "Why");
        assert_eq!(o.lessons[0].minutes, 9);
    }

    #[test]
    fn created_lessons_say_what_they_are_doing() {
        assert_eq!(
            lesson_state(&LessonStatus::Queued, None),
            "Waiting to be written"
        );
        assert_eq!(lesson_state(&LessonStatus::Generating, None), "Writing…");
        assert_eq!(lesson_state(&LessonStatus::Ready, None), "Ready");
        assert_eq!(
            lesson_state(&LessonStatus::Ready, Some(&VideoStatus::Scripting)),
            "Ready · making the video…"
        );
        assert_eq!(
            lesson_state(
                &LessonStatus::Ready,
                Some(&VideoStatus::Failed { error: "x".into() })
            ),
            "Ready · the video failed"
        );
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
        assert_eq!(first_issue(&o, true), Some(OutlineIssue::LessonTitle(1)));
        let mut keys = RowKeys::default();
        keys.fit(3);
        keys.swap(0, 1);
        let ids = Ids::new("p1");
        assert_eq!(
            OutlineIssue::LessonTitle(1).selector(&ids, &keys),
            "#ol-p1-0-title"
        );
        assert_eq!(
            OutlineIssue::LessonTitle(7).selector(&ids, &keys),
            "#ol-p1-add"
        );
        assert_eq!(
            OutlineIssue::TopicTitle.selector(&ids, &keys),
            "#ol-p1-topic"
        );
        assert_eq!(
            OutlineIssue::LessonTitle(1).message(),
            "Lesson 2 needs a title."
        );
        o.lessons.truncate(1);
        assert_eq!(first_issue(&o, true), None);
        o.topic_title = " ".into();
        assert_eq!(first_issue(&o, true), Some(OutlineIssue::TopicTitle));
        o.topic_title = "Fourier".into();
        // The subject counts only when the course isn't filed under a topic.
        o.subject_title = "  ".into();
        assert_eq!(first_issue(&o, true), Some(OutlineIssue::SubjectTitle));
        assert_eq!(first_issue(&o, false), None);
        assert_eq!(
            OutlineIssue::SubjectTitle.selector(&ids, &keys),
            "#ol-p1-subject"
        );
        o.lessons.clear();
        assert_eq!(first_issue(&o, false), Some(OutlineIssue::NoLessons));
    }
}
