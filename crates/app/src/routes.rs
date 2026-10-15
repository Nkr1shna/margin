//! Screens reachable inside the signed-in app. Every route renders inside the Option D
//! shell (sidebar + main column).

use dioxus::prelude::*;

use crate::ui::chat::ChatScreen;
use crate::ui::common::TopBar;
use crate::ui::lesson::{LessonScreen, LessonView};
use crate::ui::notes::NotesScreen;
use crate::ui::shell::Shell;

#[derive(Routable, Clone, PartialEq, Debug)]
#[rustfmt::skip]
pub enum Route {
    #[layout(Shell)]
        /// Ask: a new chat. Its first message creates the chat and moves to it.
        #[route("/")]
        Ask {},
        #[route("/chat/:id")]
        Chat { id: String },
        #[route("/lesson/:id")]
        LessonRead { id: String },
        #[route("/lesson/:id/notes")]
        LessonNotes { id: String },
        #[route("/lesson/:id/video")]
        LessonVideo { id: String },
        #[route("/notes")]
        AllNotes {},
        #[route("/notes/topic/:topic_id")]
        TopicNotes { topic_id: String },
        /// A note opened from All notes (or from Search, a lesson, a link).
        #[route("/note/:id")]
        NoteOpen { id: String },
        /// A note opened from a topic's notes list: the list stays scoped to the topic.
        #[route("/notes/topic/:topic_id/:id")]
        TopicNoteOpen { topic_id: String, id: String },
        #[route("/:..segments")]
        NotFound { segments: Vec<String> },
}

/// The document title for a screen (what assistive technology calls the web area).
pub fn page_title(route: &Route, lib: &crate::store::LibraryState) -> String {
    let lesson = |id: &str| lib.lessons.get(id).map(|l| l.title.clone());
    let screen = match route {
        Route::Ask {} => Some("New chat".to_string()),
        Route::Chat { id } => lib.chats.get(id).map(|c| chat_title(&c.title)),
        Route::LessonRead { id } => lesson(id),
        Route::LessonNotes { id } => lesson(id).map(|t| format!("{t}: Notes")),
        Route::LessonVideo { id } => lesson(id).map(|t| format!("{t}: Video")),
        Route::AllNotes {} => Some("All notes".to_string()),
        Route::TopicNotes { topic_id } => lib
            .topics
            .get(topic_id)
            .map(|t| format!("Notes in {}", t.title)),
        Route::NoteOpen { id } | Route::TopicNoteOpen { id, .. } => {
            lib.notes.get(id).map(|n| n.title.clone())
        }
        Route::NotFound { .. } => Some("Nothing here".to_string()),
    };
    match screen {
        Some(s) if !s.trim().is_empty() => format!("{s} — Margin"),
        _ => "Margin".to_string(),
    }
}

/// The screen the agent's `navigate` asks for. `None` for Settings, which is a dialog
/// over the current screen rather than a route. A topic opens its notes list, the one
/// screen of a topic's own.
pub fn nav_route(to: &margin_shared::NavTarget) -> Option<Route> {
    use margin_shared::{LessonView, NavTarget};
    Some(match to {
        NavTarget::Lesson { lesson_id, view } => {
            let id = lesson_id.clone();
            match view {
                None | Some(LessonView::Read) => Route::LessonRead { id },
                Some(LessonView::Notes) => Route::LessonNotes { id },
                Some(LessonView::Video) => Route::LessonVideo { id },
            }
        }
        NavTarget::Topic { topic_id } => Route::TopicNotes {
            topic_id: topic_id.clone(),
        },
        NavTarget::Note { note_id } => Route::NoteOpen {
            id: note_id.clone(),
        },
        NavTarget::AllNotes => Route::AllNotes {},
        NavTarget::Chat { chat_id } => Route::Chat {
            id: chat_id.clone(),
        },
        NavTarget::Settings => return None,
    })
}

/// A chat's title as screens show it ("New chat" until its first message names it).
pub fn chat_title(title: &str) -> String {
    if title.trim().is_empty() {
        "New chat".to_string()
    } else {
        title.to_string()
    }
}

#[component]
fn Ask() -> Element {
    rsx! { ChatScreen { id: None } }
}

#[component]
fn Chat(id: String) -> Element {
    rsx! {
        for id in std::iter::once(id) {
            ChatScreen { key: "{id}", id: Some(id.clone()) }
        }
    }
}

// Keyed single-item lists remount the screen when the id changes, so per-screen state
// (drafts, selections, timers) never leaks from one lesson or note to the next.

#[component]
fn LessonRead(id: String) -> Element {
    rsx! {
        for id in std::iter::once(id) {
            LessonScreen { key: "{id}", id: id.clone(), view: LessonView::Read }
        }
    }
}

#[component]
fn LessonNotes(id: String) -> Element {
    rsx! {
        for id in std::iter::once(id) {
            LessonScreen { key: "{id}", id: id.clone(), view: LessonView::Notes }
        }
    }
}

#[component]
fn LessonVideo(id: String) -> Element {
    rsx! {
        for id in std::iter::once(id) {
            LessonScreen { key: "{id}", id: id.clone(), view: LessonView::Video }
        }
    }
}

#[component]
fn AllNotes() -> Element {
    rsx! { NotesScreen { topic: None, selected: None } }
}

#[component]
fn TopicNotes(topic_id: String) -> Element {
    rsx! { NotesScreen { topic: Some(topic_id), selected: None } }
}

#[component]
fn NoteOpen(id: String) -> Element {
    rsx! { NotesScreen { topic: None, selected: Some(id) } }
}

#[component]
fn TopicNoteOpen(topic_id: String, id: String) -> Element {
    rsx! { NotesScreen { topic: Some(topic_id), selected: Some(id) } }
}

impl Route {
    /// A notes list: a topic's (`Some`) or All notes.
    pub fn notes_list(topic: Option<String>) -> Route {
        match topic {
            Some(topic_id) => Route::TopicNotes { topic_id },
            None => Route::AllNotes {},
        }
    }

    /// A note, opened in the given list.
    pub fn note_in(topic: Option<String>, id: String) -> Route {
        match topic {
            Some(topic_id) => Route::TopicNoteOpen { topic_id, id },
            None => Route::NoteOpen { id },
        }
    }

    /// The notes list this route shows or belongs to (`Some(None)` is All notes).
    pub fn notes_scope(&self) -> Option<Option<&str>> {
        match self {
            Route::AllNotes {} | Route::NoteOpen { .. } => Some(None),
            Route::TopicNotes { topic_id } | Route::TopicNoteOpen { topic_id, .. } => {
                Some(Some(topic_id))
            }
            _ => None,
        }
    }
}

#[component]
fn NotFound(segments: Vec<String>) -> Element {
    let nav = navigator();
    rsx! {
        TopBar { crumbs: vec!["Not found".to_string()] }
        div { class: "scroll",
            div { class: "empty",
                h2 { "Nothing here" }
                p { "This page doesn't exist." }
                button {
                    class: "btn btn-primary",
                    onclick: move |_| {
                        nav.replace(Route::Ask {});
                    },
                    "Go to Ask"
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn notes_keep_their_list() {
        let topic = Route::note_in(Some("t1".into()), "n1".into());
        assert_eq!(topic.to_string(), "/notes/topic/t1/n1");
        assert_eq!(
            Route::from_str("/notes/topic/t1/n1").ok(),
            Some(topic.clone())
        );
        assert_eq!(topic.notes_scope(), Some(Some("t1")));
        assert_eq!(
            Route::from_str("/notes/topic/t1").ok(),
            Some(Route::notes_list(Some("t1".into())))
        );

        let all = Route::note_in(None, "n1".into());
        assert_eq!(all.to_string(), "/note/n1");
        assert_eq!(all.notes_scope(), Some(None));
        assert_eq!(Route::notes_list(None).notes_scope(), Some(None));
        assert_eq!(Route::Ask {}.notes_scope(), None);
    }

    use crate::store::LibraryState;
    use crate::store::tests::{lesson, topic};
    use margin_shared::LessonStatus;

    #[test]
    fn titles_name_the_screen() {
        let mut lib = LibraryState::default();
        lib.topics
            .insert("f".into(), topic("f", None, "Fourier", 0));
        lib.lessons
            .insert("l1".into(), lesson("l1", "f", 0, LessonStatus::Ready));
        let chat = |title: &str| margin_shared::Chat {
            id: "c1".into(),
            title: title.into(),
            title_by: Default::default(),
            state: margin_shared::ChatState::Idle,
            workspace_ids: vec![],
            source_ids: vec![],
            pending: None,
            usage: Default::default(),
            waiting: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            provider: Default::default(),
        };
        // A chat is named by its first message; until then it's a new chat.
        lib.chats.insert("c1".into(), chat(""));
        let in_chat = Route::Chat { id: "c1".into() };
        assert_eq!(page_title(&in_chat, &lib), "New chat — Margin");
        lib.chats.insert("c1".into(), chat("Fourier"));
        assert_eq!(page_title(&in_chat, &lib), "Fourier — Margin");
        assert_eq!(Route::from_str("/chat/c1").ok(), Some(in_chat));
        let title = |r: Route| page_title(&r, &lib);
        assert_eq!(title(Route::Ask {}), "New chat — Margin");
        assert_eq!(
            title(Route::LessonRead { id: "l1".into() }),
            "Lesson l1 — Margin"
        );
        assert_eq!(
            title(Route::LessonVideo { id: "l1".into() }),
            "Lesson l1: Video — Margin"
        );
        assert_eq!(
            title(Route::TopicNotes {
                topic_id: "f".into()
            }),
            "Notes in Fourier — Margin"
        );
        // Not loaded (yet): just the app's name.
        assert_eq!(title(Route::NoteOpen { id: "x".into() }), "Margin");
        assert_eq!(
            title(Route::TopicNoteOpen {
                topic_id: "f".into(),
                id: "x".into()
            }),
            "Margin"
        );
    }
}
