//! The ⌘K palette: debounced GET /api/search, arrow keys to move, Enter to open,
//! Escape to close.

use std::time::Duration;

use dioxus::prelude::*;
use margin_shared::{SearchHit, SearchKind};

use crate::routes::Route;
use crate::state::{AppCtx, use_app};
use crate::ui::common::{Icon, js};

const DEBOUNCE: Duration = Duration::from_millis(200);

#[derive(Clone, PartialEq, Debug)]
enum Results {
    Idle,
    Searching,
    Hits(Vec<SearchHit>),
    Failed(String),
}

/// Route that opens a hit: lessons in Read, notes in the editor, topics at their first
/// lesson (or their notes when they have no lessons).
fn route_for(app: &AppCtx, hit: &SearchHit) -> Route {
    match hit.kind {
        SearchKind::Lesson => Route::LessonRead { id: hit.id.clone() },
        SearchKind::Note => Route::NoteOpen { id: hit.id.clone() },
        SearchKind::Topic => match app.lib.peek().first_lesson_in(&hit.id) {
            Some(lesson) => Route::LessonRead {
                id: lesson.id.clone(),
            },
            None => Route::TopicNotes {
                topic_id: hit.id.clone(),
            },
        },
    }
}

/// What the palette's live region says for a search state.
fn search_status(results: &Results) -> String {
    match results {
        Results::Idle => String::new(),
        Results::Searching => "Searching…".into(),
        Results::Failed(message) => format!("Search failed. {message}"),
        Results::Hits(h) if h.is_empty() => "No matches.".into(),
        Results::Hits(h) if h.len() == 1 => "1 result.".into(),
        Results::Hits(h) => format!("{} results.", h.len()),
    }
}

#[component]
pub fn SearchPalette() -> Element {
    let mut app = use_app();
    let nav = navigator();
    let mut query = use_signal(String::new);
    let mut results = use_signal(|| Results::Idle);
    let mut selected = use_signal(|| 0usize);
    let mut generation = use_signal(|| 0u64);

    let mut close = move || app.search_open.set(false);

    let mut open_hit = move |hit: SearchHit| {
        let route = route_for(&app, &hit);
        app.search_open.set(false);
        nav.push(route);
    };

    let mut run = move |q: String| {
        let next = *generation.peek() + 1;
        generation.set(next);
        selected.set(0);
        if q.trim().is_empty() {
            results.set(Results::Idle);
            return;
        }
        results.set(Results::Searching);
        spawn(async move {
            tokio::time::sleep(DEBOUNCE).await;
            if *generation.peek() != next {
                return;
            }
            let Some(api) = app.api() else { return };
            let outcome = api.search(q.trim()).await;
            if *generation.peek() != next {
                return;
            }
            match outcome {
                Ok(hits) => results.set(Results::Hits(hits)),
                Err(e) => results.set(Results::Failed(app.failure(&e))),
            }
        });
    };

    // Keep the highlighted hit visible after the DOM updates.
    use_effect(move || {
        let _ = selected.read();
        let _ = js("MarginApp.scrollIntoView('.hit[aria-selected=\"true\"]');");
    });

    let current = results.read().clone();
    let hits: Vec<SearchHit> = match &current {
        Results::Hits(h) => h.clone(),
        _ => Vec::new(),
    };
    let count = hits.len();
    let sel = (*selected.read()).min(count.saturating_sub(1));
    let keyboard_hits = hits.clone();

    rsx! {
        div {
            class: "modal-scrim",
            onclick: move |_| close(),
            div {
                class: "palette",
                role: "dialog",
                "aria-modal": "true",
                "aria-label": "Search",
                onclick: move |e| e.stop_propagation(),
                // Escape from anywhere in the palette (the field handles its own).
                onkeydown: move |e: KeyboardEvent| {
                    if e.key() == Key::Escape {
                        close();
                    }
                },
                div { class: "palette-input",
                    Icon { name: "search" }
                    input {
                        r#type: "text",
                        placeholder: "Search lessons, notes and topics",
                        "aria-label": "Search",
                        role: "combobox",
                        "aria-expanded": (count > 0).to_string(),
                        "aria-controls": "search-results",
                        "aria-activedescendant": if count > 0 { format!("hit-{sel}") } else { String::new() },
                        initial_value: "{query}",
                        onmounted: move |e| async move { let _ = e.set_focus(true).await; },
                        oninput: move |e| {
                            let v = e.value();
                            query.set(v.clone());
                            run(v);
                        },
                        onkeydown: move |e: KeyboardEvent| {
                            match e.key() {
                                Key::Escape => {
                                    e.prevent_default();
                                    close();
                                }
                                Key::ArrowDown => {
                                    e.prevent_default();
                                    if count > 0 {
                                        selected.set((sel + 1) % count);
                                    }
                                }
                                Key::ArrowUp => {
                                    e.prevent_default();
                                    if count > 0 {
                                        selected.set((sel + count - 1) % count);
                                    }
                                }
                                Key::Enter => {
                                    e.prevent_default();
                                    if let Some(hit) = keyboard_hits.get(sel).cloned() {
                                        open_hit(hit);
                                    }
                                }
                                _ => {}
                            }
                        },
                    }
                    if current == Results::Searching {
                        span { class: "spinner", "aria-hidden": "true" }
                    }
                }
                // Always in the page, so screen readers hear each change of the search state.
                div { class: "sr-only", role: "status", {search_status(&current)} }
                div { class: "palette-list", id: "search-results", role: "listbox",
                    match current {
                        Results::Idle => rsx! {
                            p { class: "faint", style: "padding: 14px 10px; margin: 0", "Type to search everything in your library." }
                        },
                        Results::Searching if count == 0 => rsx! {},
                        Results::Failed(message) => rsx! {
                            div { class: "notice error", style: "margin: 6px", Icon { name: "alert" } div { class: "body", "{message}" } }
                        },
                        Results::Hits(h) if h.is_empty() => rsx! {
                            p { class: "faint", style: "padding: 14px 10px; margin: 0", "No matches for “{query}”." }
                        },
                        _ => rsx! {
                            for (i, hit) in hits.into_iter().enumerate() {
                                button {
                                    key: "{hit.id}",
                                    id: "hit-{i}",
                                    class: "hit",
                                    role: "option",
                                    "aria-selected": (i == sel).to_string(),
                                    onmouseenter: move |_| selected.set(i),
                                    onclick: {
                                        let hit = hit.clone();
                                        move |_| open_hit(hit.clone())
                                    },
                                    Icon { name: match hit.kind { SearchKind::Lesson => "book", SearchKind::Note => "note", SearchKind::Topic => "topic" } }
                                    b { "{hit.title}" }
                                    small {
                                        match hit.kind { SearchKind::Lesson => "Lesson", SearchKind::Note => "Note", SearchKind::Topic => "Topic" }
                                        if !hit.context.is_empty() { " · {hit.context}" }
                                    }
                                    if !hit.snippet.is_empty() {
                                        span { class: "snip", "{hit.snippet}" }
                                    }
                                }
                            }
                        },
                    }
                }
                div { class: "palette-foot",
                    span { "↑↓ to move" }
                    span { "↵ to open" }
                    span { "esc to close" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_text_follows_the_search() {
        assert_eq!(search_status(&Results::Idle), "");
        assert_eq!(search_status(&Results::Searching), "Searching…");
        assert_eq!(search_status(&Results::Hits(Vec::new())), "No matches.");
        assert_eq!(
            search_status(&Results::Failed("Offline.".into())),
            "Search failed. Offline."
        );
    }
}
