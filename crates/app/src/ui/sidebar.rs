//! Sidebar: workspace header, Ask, Search, All notes, the topic tree, and the footer.

use dioxus::prelude::*;
use margin_shared::{Id, NewTopic, TopicPatch};

use crate::config::Appearance;
use crate::routes::Route;
use crate::state::{LiveStatus, LoadStatus, use_app};
use crate::store::VideoDot;
use crate::ui::common::{ConfirmDialog, Icon};

/// Which inline topic editor is open in the tree.
#[derive(Clone, PartialEq, Debug)]
enum TreeEdit {
    None,
    /// New root topic.
    AddRoot,
    /// New subtopic under this topic.
    AddChild(Id),
    Rename(Id),
}

#[derive(Clone, Copy)]
struct TreeCtx {
    edit: Signal<TreeEdit>,
    menu: Signal<Option<Id>>,
    delete: Signal<Option<Id>>,
    error: Signal<Option<String>>,
}

#[component]
pub fn Sidebar() -> Element {
    let mut app = use_app();
    let nav = navigator();
    let route = use_route::<Route>();
    let tree = use_context_provider(|| TreeCtx {
        edit: Signal::new(TreeEdit::None),
        menu: Signal::new(None),
        delete: Signal::new(None),
        error: Signal::new(None),
    });
    let mut edit = tree.edit;
    let mut error = tree.error;

    let lib = app.lib.read();
    let roots: Vec<Id> = lib
        .child_topics(None)
        .iter()
        .map(|t| t.id.clone())
        .collect();
    let note_count = lib.notes.len();
    let loaded = *app.lib_status.read() == LoadStatus::Loaded;
    let live = app.live.read().clone();
    let login = app
        .session
        .read()
        .as_ref()
        .and_then(|s| s.me.as_ref().map(|m| m.github_login.clone()));
    let appearance = app.config.read().appearance;
    let drawer_open = *app.drawer_open.read();
    drop(lib);

    let (live_class, live_label) = match live {
        LiveStatus::Live => ("dot ok", "Live"),
        LiveStatus::Connecting => ("dot run", "Connecting"),
        LiveStatus::Reconnecting { .. } => ("dot", "Offline"),
    };
    let initial = login
        .as_deref()
        .and_then(|l| l.chars().next())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "·".into());
    let theme_icon = match appearance {
        Appearance::System => "half",
        Appearance::Light => "sun",
        Appearance::Dark => "moon",
    };

    let route_now = route.clone();
    let deleting = tree.delete.read().clone();
    let deleting_title = deleting
        .as_ref()
        .and_then(|id| app.lib.read().topics.get(id).map(|t| t.title.clone()));
    let mut delete_busy = use_signal(|| false);
    let mut delete_error = use_signal(|| None::<String>);

    rsx! {
        aside {
            class: if drawer_open { "sb open" } else { "sb" },
            "aria-label": "Sidebar",
            div { class: "sb-top",
                div { class: "ws",
                    span { class: "mark", "aria-hidden": "true" }
                    span { class: "grow", "Margin" }
                    span { class: "live", title: "{live_label}",
                        span { class: "{live_class}" }
                        span { class: "sr-only", "{live_label}" }
                    }
                }
                button {
                    class: "row",
                    "aria-current": matches!(route, Route::Ask {}).to_string(),
                    onclick: move |_| {
                        nav.push(Route::Ask {});
                    },
                    Icon { name: "spark" }
                    span { class: "grow", "Ask" }
                    span { class: "k", "⌘N" }
                }
                button {
                    class: "row",
                    onclick: move |_| app.search_open.set(true),
                    Icon { name: "search" }
                    span { class: "grow", "Search" }
                    span { class: "k", "⌘K" }
                }
                button {
                    class: "row",
                    "aria-current": matches!(route, Route::AllNotes {}).to_string(),
                    onclick: move |_| {
                        nav.push(Route::AllNotes {});
                    },
                    Icon { name: "note" }
                    span { class: "grow", "All notes" }
                    span { class: "count", "{note_count}" }
                }
            }
            div { class: "sb-label",
                span { "Topics" }
                button {
                    class: "icon-btn sm",
                    "aria-label": "Add topic",
                    title: "Add topic",
                    onclick: move |_| {
                        error.set(None);
                        edit.set(TreeEdit::AddRoot);
                    },
                    Icon { name: "plus" }
                }
            }
            div { class: "sb-scroll",
                if let Some(err) = error.read().clone() {
                    div { class: "notice error", role: "alert", style: "margin: 4px 0 8px; font-size: 13px;",
                        span { class: "grow", style: "white-space: normal", "{err}" }
                        button {
                            class: "icon-btn sm",
                            "aria-label": "Dismiss",
                            onclick: move |_| error.set(None),
                            Icon { name: "x" }
                        }
                    }
                }
                if *edit.read() == TreeEdit::AddRoot {
                    TopicInput { parent: None, initial: String::new(), placeholder: "New subject" }
                }
                if !loaded && roots.is_empty() {
                    div { class: "skel", style: "margin: 10px 8px; width: 70%" }
                    div { class: "skel", style: "margin: 10px 8px; width: 55%" }
                } else if roots.is_empty() && *edit.read() != TreeEdit::AddRoot {
                    p { class: "faint", style: "padding: 6px 8px; margin: 0; font-size: 13px;",
                        "No topics yet. Ask for something to learn, or add a topic with +."
                    }
                }
                ul { class: "tree", role: "tree",
                    for id in roots {
                        TopicNode { key: "{id}", id: id.clone() }
                    }
                }
            }
            div { class: "sb-foot",
                span { class: "avatar", "aria-hidden": "true", "{initial}" }
                span { class: "grow", style: "font-size: 13px; color: var(--fg-2)",
                    {login.clone().unwrap_or_else(|| "Signed in".into())}
                }
                button {
                    class: "icon-btn",
                    "aria-label": "Appearance: {appearance.label()}. Switch appearance",
                    title: "Appearance: {appearance.label()}",
                    onclick: move |_| {
                        let _ = app.set_config(|c| c.appearance = c.appearance.next());
                    },
                    Icon { name: theme_icon }
                }
                button {
                    class: "icon-btn",
                    "aria-label": "Settings",
                    title: "Settings",
                    "aria-current": matches!(route, Route::SettingsPage {}).to_string(),
                    onclick: move |_| {
                        nav.push(Route::SettingsPage {});
                    },
                    Icon { name: "gear" }
                }
            }
        }
        if let (Some(id), Some(title)) = (deleting.clone(), deleting_title) {
            ConfirmDialog {
                title: "Delete “{title}”?",
                message: "This deletes “{title}” with all of its subtopics, lessons, notes and videos. This can't be undone.",
                confirm_label: "Delete topic",
                busy: *delete_busy.read(),
                error: delete_error.read().clone(),
                on_cancel: move |_| {
                    let mut d = tree.delete;
                    d.set(None);
                    delete_error.set(None);
                },
                on_confirm: move |_| {
                    let id = id.clone();
                    let route = route_now.clone();
                    async move {
                        let Some(api) = app.api() else { return };
                        delete_busy.set(true);
                        delete_error.set(None);
                        match api.delete_topic(&id).await {
                            Ok(()) => {
                                // Leave screens that belonged to the deleted topic.
                                let gone = route_in_topic(&app, &route, &id);
                                app.lib.write().apply(margin_shared::Event::TopicDeleted { id });
                                let mut d = tree.delete;
                                d.set(None);
                                if gone {
                                    nav.replace(Route::Ask {});
                                }
                            }
                            Err(e) => delete_error.set(Some(app.failure(&e))),
                        }
                        delete_busy.set(false);
                    }
                },
            }
        }
    }
}

/// Whether the current route shows something inside topic `topic_id` (or below it).
fn route_in_topic(app: &crate::state::AppCtx, route: &Route, topic_id: &str) -> bool {
    let lib = app.lib.peek();
    let topic_of = |t: &str| lib.topic_path(t).iter().any(|p| p.id == topic_id);
    match route {
        Route::LessonRead { id } | Route::LessonNotes { id } | Route::LessonVideo { id } => {
            lib.lessons.get(id).is_some_and(|l| topic_of(&l.topic_id))
        }
        Route::TopicNotes { topic_id: t } => topic_of(t),
        Route::NoteOpen { id } => lib.notes.get(id).is_some_and(|n| topic_of(&n.topic_id)),
        _ => false,
    }
}

#[component]
fn TopicNode(id: Id) -> Element {
    let mut app = use_app();
    let nav = navigator();
    let route = use_route::<Route>();
    let tree = use_context::<TreeCtx>();
    let mut edit = tree.edit;
    let mut menu = tree.menu;
    let mut delete = tree.delete;
    let mut error = tree.error;

    let lib = app.lib.read();
    let Some(topic) = lib.topics.get(&id).cloned() else {
        return rsx! {};
    };
    let children: Vec<Id> = lib
        .child_topics(Some(&id))
        .iter()
        .map(|t| t.id.clone())
        .collect();
    let lessons: Vec<(Id, String, VideoDot, bool)> = lib
        .lessons_in(&id)
        .iter()
        .map(|l| {
            let generating = matches!(
                l.status,
                margin_shared::LessonStatus::Generating | margin_shared::LessonStatus::Queued
            );
            (
                l.id.clone(),
                l.title.clone(),
                lib.video_dot(&l.id),
                generating,
            )
        })
        .collect();
    let note_count = lib.notes_in_topic(&id).len();
    let count = lib.lesson_count_deep(&id);
    drop(lib);

    let collapsed = app.collapsed.read().contains(&id);
    let menu_open = menu.read().as_deref() == Some(id.as_str());
    let renaming = *edit.read() == TreeEdit::Rename(id.clone());
    let adding_child = *edit.read() == TreeEdit::AddChild(id.clone());
    let has_children =
        !children.is_empty() || !lessons.is_empty() || note_count > 0 || adding_child;

    let toggle = {
        let id = id.clone();
        move |_| {
            let mut set = app.collapsed.write();
            if !set.remove(&id) {
                set.insert(id.clone());
            }
        }
    };

    rsx! {
        li {
            class: if collapsed { "collapsed" } else { "" },
            role: "treeitem",
            "aria-expanded": (!collapsed).to_string(),
            if renaming {
                TopicInput { parent: topic.parent_id.clone(), initial: topic.title.clone(), placeholder: "Topic name", rename: Some(id.clone()) }
            } else {
                div { class: "tree-row",
                    button {
                        class: "row",
                        "aria-label": if collapsed { "Expand {topic.title}" } else { "Collapse {topic.title}" },
                        onclick: toggle,
                        Icon { name: "chev", class: "chev" }
                        span { class: "grow", "{topic.title}" }
                        if count > 0 {
                            span { class: "count", "{count}" }
                        }
                    }
                    button {
                        class: "icon-btn sm more",
                        "aria-label": "Actions for {topic.title}",
                        "aria-haspopup": "menu",
                        "aria-expanded": menu_open.to_string(),
                        onclick: {
                            let id = id.clone();
                            move |_| {
                                if menu_open {
                                    menu.set(None);
                                } else {
                                    menu.set(Some(id.clone()));
                                }
                            }
                        },
                        Icon { name: "more" }
                    }
                }
            }
            if menu_open {
                div { class: "pop-scrim", onclick: move |_| menu.set(None) }
                div {
                    class: "menu right",
                    role: "menu",
                    onkeydown: move |e: KeyboardEvent| {
                        if e.key() == Key::Escape {
                            menu.set(None);
                        }
                    },
                    button {
                        role: "menuitem",
                        autofocus: true,
                        onclick: {
                            let id = id.clone();
                            move |_| {
                                menu.set(None);
                                error.set(None);
                                edit.set(TreeEdit::Rename(id.clone()));
                            }
                        },
                        Icon { name: "edit" }
                        "Rename"
                    }
                    button {
                        role: "menuitem",
                        onclick: {
                            let id = id.clone();
                            move |_| {
                                menu.set(None);
                                error.set(None);
                                app.collapsed.write().remove(&id);
                                edit.set(TreeEdit::AddChild(id.clone()));
                            }
                        },
                        Icon { name: "plus" }
                        "Add subtopic"
                    }
                    button {
                        role: "menuitem",
                        onclick: {
                            let id = id.clone();
                            move |_| {
                                menu.set(None);
                                nav.push(Route::TopicNotes { topic_id: id.clone() });
                            }
                        },
                        Icon { name: "note" }
                        "Notes in this topic"
                    }
                    button {
                        role: "menuitem",
                        class: "danger",
                        onclick: {
                            let id = id.clone();
                            move |_| {
                                menu.set(None);
                                delete.set(Some(id.clone()));
                            }
                        },
                        Icon { name: "trash" }
                        "Delete…"
                    }
                }
            }
            if has_children && !collapsed {
                ul { class: "tree", role: "group",
                    for child in children {
                        TopicNode { key: "{child}", id: child.clone() }
                    }
                    for (lid, title, dot, generating) in lessons {
                        li { key: "{lid}", role: "treeitem",
                            button {
                                class: "row",
                                "aria-current": route_is_lesson(&route, &lid).to_string(),
                                onclick: {
                                    let lid = lid.clone();
                                    move |_| {
                                        nav.push(Route::LessonRead { id: lid.clone() });
                                    }
                                },
                                Icon { name: "book", class: "leaf-ic" }
                                span { class: "grow", "{title}" }
                                if generating {
                                    span { class: "spinner", style: "width: 10px; height: 10px; border-width: 1.5px", title: "Writing" }
                                }
                                match dot {
                                    VideoDot::Ready => rsx! { span { class: "dot ok", title: "Video ready" } },
                                    VideoDot::InProgress => rsx! { span { class: "dot run", title: "Video in progress" } },
                                    VideoDot::None => rsx! { span { class: "dot", title: "No video" } },
                                }
                            }
                        }
                    }
                    if note_count > 0 {
                        li { key: "notes-{id}", role: "treeitem",
                            button {
                                class: "row",
                                "aria-current": matches!(&route, Route::TopicNotes { topic_id } if *topic_id == id).to_string(),
                                onclick: {
                                    let id = id.clone();
                                    move |_| {
                                        nav.push(Route::TopicNotes { topic_id: id.clone() });
                                    }
                                },
                                Icon { name: "note", class: "leaf-ic" }
                                span { class: "grow", "Notes" }
                                span { class: "count", "{note_count}" }
                            }
                        }
                    }
                    if adding_child {
                        li { key: "add-{id}",
                            TopicInput { parent: Some(id.clone()), initial: String::new(), placeholder: "New subtopic" }
                        }
                    }
                }
            }
        }
    }
}

fn route_is_lesson(route: &Route, lesson_id: &str) -> bool {
    match route {
        Route::LessonRead { id } | Route::LessonNotes { id } | Route::LessonVideo { id } => {
            id == lesson_id
        }
        _ => false,
    }
}

/// Inline text field to name a new topic or rename one. Enter saves, Escape cancels,
/// leaving the field saves a non-empty change.
#[component]
fn TopicInput(
    parent: Option<Id>,
    initial: String,
    placeholder: &'static str,
    #[props(default)] rename: Option<Id>,
) -> Element {
    let app = use_app();
    let tree = use_context::<TreeCtx>();
    let mut value = use_signal(|| initial.clone());
    let mut busy = use_signal(|| false);

    let commit = {
        let parent = parent.clone();
        let rename = rename.clone();
        let initial = initial.clone();
        move || {
            let parent = parent.clone();
            let rename = rename.clone();
            let initial = initial.clone();
            async move {
                if *busy.peek() {
                    return;
                }
                let mut edit = tree.edit;
                let mut error = tree.error;
                let title = value.peek().trim().to_string();
                if title.is_empty() || title == initial {
                    edit.set(TreeEdit::None);
                    return;
                }
                let Some(api) = app.api() else { return };
                busy.set(true);
                let result = match &rename {
                    Some(id) => {
                        api.patch_topic(
                            id,
                            &TopicPatch {
                                title: Some(title),
                                ..Default::default()
                            },
                        )
                        .await
                    }
                    None => {
                        api.create_topic(&NewTopic {
                            parent_id: parent,
                            title,
                        })
                        .await
                    }
                };
                busy.set(false);
                match result {
                    Ok(topic) => {
                        let mut lib = app.lib;
                        lib.write()
                            .apply(margin_shared::Event::TopicChanged { topic });
                        edit.set(TreeEdit::None);
                    }
                    Err(e) => error.set(Some(app.failure(&e))),
                }
            }
        }
    };
    let commit_blur = commit.clone();

    rsx! {
        input {
            class: "input tree-input",
            r#type: "text",
            placeholder,
            "aria-label": placeholder,
            initial_value: "{value}",
            disabled: *busy.read(),
            onmounted: move |e| async move {
                let _ = e.set_focus(true).await;
            },
            oninput: move |e| value.set(e.value()),
            onkeydown: move |e: KeyboardEvent| {
                let commit = commit.clone();
                async move {
                    match e.key() {
                        Key::Enter => {
                            e.prevent_default();
                            commit().await;
                        }
                        Key::Escape => {
                            let mut edit = tree.edit;
                            edit.set(TreeEdit::None);
                        }
                        _ => {}
                    }
                }
            },
            onblur: move |_| {
                let commit = commit_blur.clone();
                async move { commit().await }
            },
        }
    }
}
