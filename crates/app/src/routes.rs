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
        #[route("/note/:id")]
        NoteOpen { id: String },
        #[route("/:..segments")]
        NotFound { segments: Vec<String> },
}

/// The document title for a screen (what assistive technology calls the web area).
pub fn page_title(route: &Route, lib: &crate::store::LibraryState) -> String {
    let lesson = |id: &str| lib.lessons.get(id).map(|l| l.title.clone());
    let screen = match route {
        Route::Ask {} => Some("Ask".to_string()),
        Route::LessonRead { id } => lesson(id),
        Route::LessonNotes { id } => lesson(id).map(|t| format!("{t}: Notes")),
        Route::LessonVideo { id } => lesson(id).map(|t| format!("{t}: Video")),
        Route::AllNotes {} => Some("All notes".to_string()),
        Route::TopicNotes { topic_id } => lib
            .topics
            .get(topic_id)
            .map(|t| format!("Notes in {}", t.title)),
        Route::NoteOpen { id } => lib.notes.get(id).map(|n| n.title.clone()),
        Route::NotFound { .. } => Some("Nothing here".to_string()),
    };
    match screen {
        Some(s) if !s.trim().is_empty() => format!("{s} — Margin"),
        _ => "Margin".to_string(),
    }
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
    use super::*;
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
        let title = |r: Route| page_title(&r, &lib);
        assert_eq!(title(Route::Ask {}), "Ask — Margin");
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
    }
}
