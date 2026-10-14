//! Screens reachable inside the signed-in app. Every route renders inside the Option D
//! shell (sidebar + main column).

use dioxus::prelude::*;

use crate::ui::ask::AskScreen;
use crate::ui::lesson::{LessonScreen, LessonView};
use crate::ui::notes::NotesScreen;
use crate::ui::settings::SettingsScreen;
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
        #[route("/note/:id")]
        NoteOpen { id: String },
        #[route("/settings")]
        SettingsPage {},
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
fn SettingsPage() -> Element {
    rsx! { SettingsScreen {} }
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
