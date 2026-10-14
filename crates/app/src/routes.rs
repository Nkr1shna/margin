//! Screens reachable inside the signed-in app. Every route renders inside the Option D
//! shell (sidebar + main column).

use dioxus::prelude::*;

use crate::ui::ask::AskScreen;
use crate::ui::lesson::{LessonScreen, LessonView};
use crate::ui::notes::NotesScreen;
use crate::ui::shell::Shell;

#[derive(Routable, Clone, PartialEq, Debug)]
#[rustfmt::skip]
pub enum Route {
    #[layout(Shell)]
        #[route("/")]
        Ask {},
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

#[component]
fn Ask() -> Element {
    rsx! { AskScreen {} }
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
}
