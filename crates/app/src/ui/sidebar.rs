//! Sidebar: workspace header, Ask, Search, All notes, the topic tree, and the footer.

use dioxus::prelude::*;
use margin_shared::{Event, Id, LessonPatch, NewTopic, TopicPatch};

use crate::commands::Command;
use crate::config::Appearance;
use crate::routes::Route;
use crate::state::{LoadStatus, use_app};
use crate::store::{LibraryState, VideoDot};
use crate::ui::common::{
    BrandMark, ConfirmDialog, DragRegion, HistoryButtons, Icon, UNIFIED_TITLE_BAR,
};
use crate::ui::settings::SettingsPane;
use crate::ui::shell::close_drawer;
use crate::ui::sources::TopicSourcesDialog;

/// Which inline topic editor is open in the tree.
#[derive(Clone, PartialEq, Debug)]
enum TreeEdit {
    None,
    /// New root topic.
    AddRoot,
    /// New subtopic under this topic.
    AddChild(Id),
    Rename(Id),
    RenameLesson(Id),
}

#[derive(Clone, Copy)]
struct TreeCtx {
    edit: Signal<TreeEdit>,
    menu: Signal<Option<Id>>,
    delete: Signal<Option<Id>>,
    delete_lesson: Signal<Option<Id>>,
    /// The topic whose Sources dialog is open.
    sources: Signal<Option<Id>>,
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
        delete_lesson: Signal::new(None),
        sources: Signal::new(None),
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
    let loaded = *app.lib_status.read() == LoadStatus::Loaded;
    let login = app
        .session
        .read()
        .as_ref()
        .and_then(|s| s.me.as_ref().map(|m| m.github_login.clone()));
    let appearance = app.config.read().appearance;
    let drawer_open = *app.drawer_open.read();
    drop(lib);

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
    let deleting_lesson = tree.delete_lesson.read().clone();
    let deleting_lesson_title = deleting_lesson
        .as_ref()
        .and_then(|id| app.lib.read().lessons.get(id).map(|l| l.title.clone()));
    let route_now_lesson = route.clone();
    let mut sources_topic = tree.sources;
    // Open only while the topic exists (it may be deleted elsewhere meanwhile).
    let sources_open = sources_topic
        .read()
        .clone()
        .filter(|id| app.lib.read().topics.contains_key(id));
    let mut delete_busy = use_signal(|| false);
    let mut delete_error = use_signal(|| None::<String>);

    rsx! {
        aside {
            class: if drawer_open { "sb open" } else { "sb" },
            "aria-label": "Sidebar",
            // Narrow screens: Escape closes the drawer (menus and fields inside handle
            // their own Escape first).
            onkeydown: move |e: KeyboardEvent| {
                if drawer_open && e.key() == Key::Escape {
                    close_drawer(app);
                }
            },
            div {
                class: "sb-resize",
                role: "separator",
                "aria-orientation": "vertical",
                "aria-label": "Resize sidebar",
                title: "Drag to resize, double-click to reset",
            }
            div { class: "sb-top",
                div { class: "ws",
                    DragRegion {}
                    // On macOS the app's name is in the menu bar; this row holds the
                    // window controls instead.
                    if !UNIFIED_TITLE_BAR {
                        BrandMark {}
                        span { class: "grow", "Margin" }
                    }
                    button {
                        class: "icon-btn sb-toggle",
                        "aria-label": "Hide sidebar",
                        title: "Hide sidebar ({Command::ToggleSidebar.shortcut()})",
                        onclick: move |_| {
                            if drawer_open {
                                close_drawer(app);
                            } else {
                                let _ = app.set_config(|c| c.sidebar_hidden = true);
                            }
                        },
                        Icon { name: "panel" }
                    }
                    if UNIFIED_TITLE_BAR {
                        HistoryButtons {}
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
                    span { class: "k", {Command::Ask.shortcut()} }
                }
                button {
                    class: "row",
                    onclick: move |_| app.search_open.set(true),
                    Icon { name: "search" }
                    span { class: "grow", "Search" }
                    span { class: "k", {Command::Search.shortcut()} }
                }
                button {
                    class: "row",
                    // Also while a note opened from All notes is showing.
                    "aria-current": (route.notes_scope() == Some(None)).to_string(),
                    onclick: move |_| {
                        nav.push(Route::AllNotes {});
                    },
                    Icon { name: "note" }
                    span { class: "grow", "All notes" }
                    span { class: "k", {Command::AllNotes.shortcut()} }
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
                    "aria-label": "Appearance: {appearance.label()}. Switch to {appearance.next().label()}",
                    title: "Appearance: {appearance.label()} (click for {appearance.next().label()})",
                    onclick: move |_| {
                        let _ = app.set_config(|c| c.appearance = c.appearance.next());
                    },
                    Icon { name: theme_icon }
                }
                button {
                    class: "icon-btn",
                    "aria-label": "Settings",
                    title: "Settings ({Command::Settings.shortcut()})",
                    "aria-current": app.settings_open.read().is_some().to_string(),
                    onclick: move |_| app.settings_open.set(Some(SettingsPane::General)),
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
                                let gone = route_in_topic(&app.lib.peek(), &route, &id);
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
        if let Some(id) = sources_open {
            TopicSourcesDialog { key: "{id}", topic_id: id.clone(), on_close: move |_| sources_topic.set(None) }
        }
        if let (Some(id), Some(title)) = (deleting_lesson.clone(), deleting_lesson_title) {
            ConfirmDialog {
                title: "Delete “{title}”?",
                message: "This deletes the lesson with its notes and video. This can't be undone.",
                confirm_label: "Delete lesson",
                busy: *delete_busy.read(),
                error: delete_error.read().clone(),
                on_cancel: move |_| {
                    let mut d = tree.delete_lesson;
                    d.set(None);
                    delete_error.set(None);
                },
                on_confirm: move |_| {
                    let id = id.clone();
                    let route = route_now_lesson.clone();
                    async move {
                        let Some(api) = app.api() else { return };
                        delete_busy.set(true);
                        delete_error.set(None);
                        match api.delete_lesson(&id).await {
                            Ok(()) => {
                                let open = route_is_lesson(&route, &id);
                                app.lib.write().apply(Event::LessonDeleted { id });
                                let mut d = tree.delete_lesson;
                                d.set(None);
                                if open {
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

/// A lesson in the tree: opens on click; its ⋯ button or a right-click offers Rename,
/// Regenerate and Delete, like the lesson screen's menu.
#[component]
fn LessonNode(id: Id, title: String, dot: VideoDot, generating: bool) -> Element {
    let app = use_app();
    let nav = navigator();
    let route = use_route::<Route>();
    let tree = use_context::<TreeCtx>();
    let mut edit = tree.edit;
    let mut menu = tree.menu;
    let mut delete_lesson = tree.delete_lesson;
    let mut error = tree.error;
    let menu_open = menu.read().as_deref() == Some(id.as_str());
    let renaming = *edit.read() == TreeEdit::RenameLesson(id.clone());
    let current = route_is_lesson(&route, &id);
    let open_menu = {
        let id = id.clone();
        move || menu.set(Some(id.clone()))
    };

    rsx! {
        li { role: "treeitem",
            if renaming {
                LessonInput { id: id.clone(), initial: title.clone() }
            } else {
                div {
                    // The selection spans the whole row, ⋯ slot included, like Notes rows.
                    class: if current { "tree-row current" } else { "tree-row" },
                    "data-long-press": "true",
                    oncontextmenu: {
                        let mut open_menu = open_menu.clone();
                        move |e: MouseEvent| {
                            e.prevent_default();
                            open_menu();
                        }
                    },
                    button {
                        class: "row",
                        "aria-current": current.to_string(),
                        onclick: {
                            let id = id.clone();
                            move |_| {
                                nav.push(Route::LessonRead { id: id.clone() });
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
                    button {
                        class: "icon-btn sm more",
                        "aria-label": "Actions for {title}",
                        "aria-haspopup": "menu",
                        "aria-expanded": menu_open.to_string(),
                        onclick: {
                            let mut open_menu = open_menu.clone();
                            move |_| if menu_open { menu.set(None) } else { open_menu() }
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
                            e.stop_propagation();
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
                                edit.set(TreeEdit::RenameLesson(id.clone()));
                            }
                        },
                        Icon { name: "edit" }
                        "Rename"
                    }
                    button {
                        role: "menuitem",
                        disabled: generating,
                        onclick: {
                            let id = id.clone();
                            move |_| {
                                menu.set(None);
                                error.set(None);
                                let id = id.clone();
                                spawn(async move {
                                    let Some(api) = app.api() else { return };
                                    match api.regenerate_lesson(&id).await {
                                        Ok(lesson) => {
                                            let mut lib = app.lib;
                                            lib.write().apply(Event::LessonChanged { lesson });
                                        }
                                        Err(e) => error.set(Some(app.failure(&e))),
                                    }
                                });
                            }
                        },
                        Icon { name: "refresh" }
                        "Regenerate"
                    }
                    button {
                        role: "menuitem",
                        class: "danger",
                        onclick: {
                            let id = id.clone();
                            move |_| {
                                menu.set(None);
                                delete_lesson.set(Some(id.clone()));
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

/// Inline field renaming a lesson. Enter or leaving the field saves; Escape cancels.
#[component]
fn LessonInput(id: Id, initial: String) -> Element {
    let app = use_app();
    let tree = use_context::<TreeCtx>();
    let mut value = use_signal(|| initial.clone());
    let commit = {
        let id = id.clone();
        let initial = initial.clone();
        move || {
            let id = id.clone();
            let initial = initial.clone();
            async move {
                let mut edit = tree.edit;
                let mut error = tree.error;
                let title = value.peek().trim().to_string();
                edit.set(TreeEdit::None);
                if title.is_empty() || title == initial {
                    return;
                }
                let Some(api) = app.api() else { return };
                let patch = LessonPatch {
                    title: Some(title),
                    ..Default::default()
                };
                match api.patch_lesson(&id, &patch).await {
                    Ok(lesson) => {
                        let mut lib = app.lib;
                        lib.write().apply(Event::LessonChanged { lesson });
                    }
                    Err(e) => error.set(Some(app.failure(&e))),
                }
            }
        }
    };
    let on_blur = commit.clone();
    rsx! {
        input {
            class: "input tree-input",
            "aria-label": "Lesson title",
            autofocus: true,
            initial_value: "{initial}",
            oninput: move |e| value.set(e.value()),
            onkeydown: move |e: KeyboardEvent| {
                match e.key() {
                    Key::Enter => {
                        spawn(commit());
                    }
                    Key::Escape => {
                        e.stop_propagation();
                        let mut edit = tree.edit;
                        edit.set(TreeEdit::None);
                    }
                    _ => {}
                }
            },
            onblur: move |_| {
                if *tree.edit.peek() == TreeEdit::None {
                    return;
                }
                spawn(on_blur());
            },
        }
    }
}

/// Whether the current route shows something inside topic `topic_id` (or below it).
fn route_in_topic(lib: &LibraryState, route: &Route, topic_id: &str) -> bool {
    let topic_of = |t: &str| lib.topic_path(t).iter().any(|p| p.id == topic_id);
    let note_in = |id: &str| lib.notes.get(id).is_some_and(|n| topic_of(&n.topic_id));
    match route {
        Route::LessonRead { id } | Route::LessonNotes { id } | Route::LessonVideo { id } => {
            lib.lessons.get(id).is_some_and(|l| topic_of(&l.topic_id))
        }
        Route::TopicNotes { topic_id: t } => topic_of(t),
        Route::TopicNoteOpen { topic_id: t, id } => topic_of(t) || note_in(id),
        Route::NoteOpen { id } => note_in(id),
        _ => false,
    }
}

/// Whether the tree row of the current screen (a lesson, or a topic's Notes) is inside
/// topic `topic_id`, so a collapsed `topic_id` hides it.
fn current_row_in_topic(lib: &LibraryState, route: &Route, topic_id: &str) -> bool {
    let row_topic = match route {
        Route::LessonRead { id } | Route::LessonNotes { id } | Route::LessonVideo { id } => {
            lib.lessons.get(id).map(|l| l.topic_id.as_str())
        }
        Route::TopicNotes { topic_id } | Route::TopicNoteOpen { topic_id, .. } => {
            Some(topic_id.as_str())
        }
        _ => None,
    };
    row_topic.is_some_and(|t| lib.topic_path(t).iter().any(|p| p.id == topic_id))
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
    let mut sources = tree.sources;
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
    let collapsed = app.collapsed.read().contains(&id);
    // Collapsed over the open screen's row: the topic stands in for it.
    let holds_current = collapsed && current_row_in_topic(&lib, &route, &id);
    drop(lib);

    let menu_open = menu.read().as_deref() == Some(id.as_str());
    let renaming = *edit.read() == TreeEdit::Rename(id.clone());
    let adding_child = *edit.read() == TreeEdit::AddChild(id.clone());
    let has_children =
        !children.is_empty() || !lessons.is_empty() || note_count > 0 || adding_child;

    let chev_class: &'static str = if has_children {
        "chev"
    } else {
        "chev chev-off"
    };
    let toggle_label = if !has_children {
        topic.title.clone()
    } else if holds_current {
        format!("Expand {} (contains the open page)", topic.title)
    } else if collapsed {
        format!("Expand {}", topic.title)
    } else {
        format!("Collapse {}", topic.title)
    };
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
            "aria-expanded": has_children.then_some(if collapsed { "false" } else { "true" }),
            if renaming {
                TopicInput { parent: topic.parent_id.clone(), initial: topic.title.clone(), placeholder: "Topic name", rename: Some(id.clone()) }
            } else {
                div {
                    class: if holds_current { "tree-row holds-current" } else { "tree-row" },
                    "data-long-press": "true",
                    // Breadcrumbs find the topic here to show it (`reveal_topic`).
                    "data-topic": "{id}",
                    // Right-click opens the same menu as the ⋯ button.
                    oncontextmenu: {
                        let id = id.clone();
                        move |e: MouseEvent| {
                            e.prevent_default();
                            menu.set(Some(id.clone()));
                        }
                    },
                    button {
                        class: "row",
                        "aria-label": toggle_label,
                        onclick: toggle,
                        // An empty topic has nothing to expand: keep the chevron's space
                        // so titles line up, but don't show it.
                        Icon { name: "chev", class: chev_class }
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
                            e.stop_propagation();
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
                        onclick: {
                            let id = id.clone();
                            move |_| {
                                menu.set(None);
                                sources.set(Some(id.clone()));
                            }
                        },
                        Icon { name: "clip" }
                        "Sources…"
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
                        LessonNode { key: "{lid}", id: lid.clone(), title, dot, generating }
                    }
                    if note_count > 0 {
                        li { key: "notes-{id}", role: "treeitem",
                            button {
                                class: "row",
                                // Also while a note opened from this list is showing.
                                "aria-current": (route.notes_scope() == Some(Some(id.as_str()))).to_string(),
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
                if e.key() == Key::Escape {
                    // Cancels the edit only (an open drawer stays open).
                    e.stop_propagation();
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn lib() -> LibraryState {
        let mut lib = LibraryState::default();
        lib.replace_library(crate::fake::sample_library());
        lib
    }

    #[test]
    fn collapsed_topics_know_the_current_row() {
        let lib = lib();
        // signals > fourier > l1; signals > complex > l4.
        let lesson = Route::LessonRead { id: "l1".into() };
        assert!(current_row_in_topic(&lib, &lesson, "fourier"));
        assert!(current_row_in_topic(&lib, &lesson, "signals"));
        assert!(!current_row_in_topic(&lib, &lesson, "complex"));
        // A topic's notes (also with a note open) live in that topic's subtree.
        let notes = Route::TopicNoteOpen {
            topic_id: "fourier".into(),
            id: "n1".into(),
        };
        assert!(current_row_in_topic(&lib, &notes, "signals"));
        assert!(!current_row_in_topic(&lib, &notes, "linalg"));
        // All notes is outside the tree.
        let all = Route::NoteOpen { id: "n1".into() };
        assert!(!current_row_in_topic(&lib, &all, "fourier"));
    }

    #[test]
    fn deleting_a_topic_leaves_screens_inside_it() {
        let lib = lib();
        let scoped = Route::TopicNoteOpen {
            topic_id: "signals".into(),
            id: "n1".into(),
        };
        assert!(route_in_topic(&lib, &scoped, "signals"));
        // The open note is in fourier, though the list is its parent's.
        assert!(route_in_topic(&lib, &scoped, "fourier"));
        assert!(route_in_topic(
            &lib,
            &Route::NoteOpen { id: "n1".into() },
            "fourier"
        ));
        assert!(!route_in_topic(&lib, &scoped, "linalg"));
    }
}
