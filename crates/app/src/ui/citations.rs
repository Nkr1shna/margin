//! A lesson's sources: the "Sources" list after the body and the source viewer that a
//! citation marker or a list entry opens, at the cited page, time, image or passage.

use dioxus::prelude::*;
use margin_shared::{Citation, Id, SourceKind};

use crate::citations::{
    self, Segment, Viewer, kind_label, locator, neighbors, open_label, open_url, position,
};
use crate::state::{AppCtx, SourceViewer, use_app};
use crate::ui::common::{Icon, js, open_external};
use crate::ui::sources::kind_icon;

const DIALOG_ID: &str = "source-viewer";
const MEDIA_ID: &str = "viewer-media";
const TRANSCRIPT_ID: &str = "viewer-transcript";

/// Open the source viewer on `n`.
pub fn open_viewer(app: AppCtx, lesson_id: &Id, n: u32) {
    let mut viewer = app.source_viewer;
    viewer.set(Some(SourceViewer {
        lesson_id: lesson_id.clone(),
        n,
    }));
}

/// Close the source viewer and give focus back to what opened it.
fn close_viewer(app: AppCtx, n: u32) {
    let mut viewer = app.source_viewer;
    viewer.set(None);
    let id = serde_json::to_string(DIALOG_ID).unwrap_or_default();
    let fallback = serde_json::to_string(&format!("#lesson-body button.cite[data-cite=\"{n}\"]"))
        .unwrap_or_default();
    let _ = js(&format!("MarginApp.dialogClosed({id}, {fallback});"));
}

/// Signed media URLs expire; when one stops loading, reload the library (once per
/// viewer) for fresh ones.
fn refresh_media(app: AppCtx, mut retried: Signal<bool>) {
    if *retried.peek() {
        return;
    }
    retried.set(true);
    let mut reload = app.reload;
    *reload.write() += 1;
}

/// The lesson's citations in order, after the body: number, kind, source, where in it,
/// a short excerpt and a thumbnail. Each opens the source viewer.
#[component]
pub fn CitationList(lesson_id: Id, citations: Vec<Citation>) -> Element {
    let app = use_app();
    if citations.is_empty() {
        return rsx! {};
    }
    rsx! {
        section { class: "cites", "aria-labelledby": "cites-title",
            h2 { id: "cites-title", "Sources" }
            ol { class: "cite-list",
                for c in citations {
                    li { key: "{c.n}",
                        button {
                            class: "cite-item",
                            "aria-label": "{citations::marker_label(c.n, &c.source_name)}",
                            onclick: {
                                let lesson_id = lesson_id.clone();
                                move |_| open_viewer(app, &lesson_id, c.n)
                            },
                            span { class: "cite-n", "{c.n}" }
                            Icon { name: kind_icon(c.kind), class: "cite-ic" }
                            span { class: "cite-main",
                                span { class: "cite-name", "{c.source_name}" }
                                span { class: "cite-meta",
                                    "{kind_label(c.kind)}"
                                    if let Some(at) = locator(&c) { " · {at}" }
                                }
                                if !c.excerpt.trim().is_empty() {
                                    span { class: "cite-excerpt", title: "{c.excerpt.trim()}", "{c.excerpt.trim()}" }
                                }
                            }
                            if let Some(url) = c.image_url.clone() {
                                img { class: "cite-thumb", src: "{url}", alt: "", loading: "lazy" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The source viewer: a dialog on one citation of the lesson, with Previous and Next
/// through the others. Esc, the close button or a click outside closes it.
#[component]
pub fn SourceViewerDialog(lesson_id: Id, n: u32) -> Element {
    let app = use_app();
    let mut open_error = use_signal(|| None::<String>);
    let retried = use_signal(|| false);

    // Focus moves into the dialog when it opens (the webview ignores autofocus on
    // inserted elements) and stays there as Previous and Next change what it shows.
    use_effect(use_reactive!(|n| {
        let _ = n;
        let id = serde_json::to_string(DIALOG_ID).unwrap_or_default();
        let _ = js(&format!("MarginApp.dialogOpened({id});"));
    }));

    let lib = app.lib.read();
    let all = lib
        .lessons
        .get(&lesson_id)
        .map(|l| citations::ordered(&l.citations))
        .unwrap_or_default();
    let found = all.iter().find(|c| c.n == n).cloned();
    // An audio or video source's whole transcript: what was fetched last shows at once,
    // and is fetched again each time the viewer turns to the source (a source read
    // again has a new one).
    let recording = found
        .as_ref()
        .filter(|c| matches!(c.kind, SourceKind::Audio | SourceKind::Video))
        .filter(|c| lib.sources.contains_key(&c.source_id))
        .map(|c| c.source_id.clone());
    use_effect(use_reactive!(|recording| {
        if let Some(source_id) = recording {
            spawn(fetch_transcript(app, source_id));
        }
    }));
    let Some(c) = found else {
        drop(lib);
        // The lesson changed under the viewer (regenerated or deleted).
        return rsx! {};
    };
    let media_url = lib
        .sources
        .get(&c.source_id)
        .and_then(|s| s.media_url.clone());
    let source_gone = !lib.sources.contains_key(&c.source_id);
    drop(lib);

    let lines = app
        .transcripts
        .read()
        .get(&c.source_id)
        .cloned()
        .unwrap_or_default();
    let view = citations::viewer(&c, &all, media_url.as_deref(), &lines);
    let open = open_url(&c, media_url.as_deref());
    let (prev, next) = neighbors(&all, n);
    let (index, total) = position(&all, n).unwrap_or((1, all.len()));
    let go = {
        let lesson_id = lesson_id.clone();
        move |to: u32| {
            open_error.set(None);
            open_viewer(app, &lesson_id, to);
        }
    };
    let (mut go_prev, mut go_next) = (go.clone(), go);
    let meta = match locator(&c) {
        Some(at) => format!("{} · {at}", kind_label(c.kind)),
        None => kind_label(c.kind).to_string(),
    };

    rsx! {
        div {
            class: "modal-scrim center",
            onclick: move |_| close_viewer(app, n),
            div {
                id: "{DIALOG_ID}",
                class: "modal viewer-modal",
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": "viewer-title",
                tabindex: "-1",
                onclick: move |e| e.stop_propagation(),
                onkeydown: move |e: KeyboardEvent| {
                    if e.key() == Key::Escape {
                        e.stop_propagation();
                        close_viewer(app, n);
                    }
                },
                div { class: "viewer-head",
                    span { class: "cite-n", "aria-hidden": "true", "{n}" }
                    Icon { name: kind_icon(c.kind), class: "cite-ic" }
                    div { class: "grow",
                        h2 { id: "viewer-title", title: "{c.source_name}", "{c.source_name}" }
                        small { "{meta}" }
                    }
                    if let Some(url) = open {
                        button {
                            class: "btn btn-secondary btn-sm",
                            title: "Open the whole file in your browser",
                            onclick: move |_| open_error.set(open_external(&url).err()),
                            "{open_label(c.kind)}"
                        }
                    }
                    button {
                        class: "icon-btn",
                        "aria-label": "Close",
                        title: "Close (Esc)",
                        onclick: move |_| close_viewer(app, n),
                        Icon { name: "x" }
                    }
                }
                if let Some(err) = open_error.read().clone() {
                    div { class: "notice error", role: "alert",
                        Icon { name: "alert" }
                        div { class: "body", "{err}" }
                    }
                }
                div { class: "viewer-body",
                    ViewerContent {
                        key: "{n}",
                        view,
                        name: c.source_name.clone(),
                        source_gone,
                        on_media_error: move |_| refresh_media(app, retried),
                    }
                }
                div { class: "viewer-foot",
                    button {
                        class: "btn btn-ghost btn-sm",
                        disabled: prev.is_none(),
                        "aria-label": "Previous source",
                        onclick: move |_| if let Some(p) = prev { go_prev(p) },
                        Icon { name: "back" }
                        "Previous"
                    }
                    span { class: "faint", "{index} of {total}" }
                    button {
                        class: "btn btn-ghost btn-sm",
                        disabled: next.is_none(),
                        "aria-label": "Next source",
                        onclick: move |_| if let Some(x) = next { go_next(x) },
                        "Next"
                        Icon { name: "forward" }
                    }
                }
            }
        }
    }
}

#[component]
fn ViewerContent(
    view: Viewer,
    name: String,
    source_gone: bool,
    on_media_error: EventHandler<()>,
) -> Element {
    let missing_media = if source_gone {
        "This source was removed from the topic, so only the cited passage is left."
    } else {
        "The service has no copy of this file to play."
    };
    match view {
        Viewer::Image { url } => rsx! {
            figure { class: "viewer-figure",
                img { src: "{url}", alt: "{name}", onerror: move |_| on_media_error.call(()) }
            }
        },
        Viewer::PdfPage {
            page,
            image,
            excerpt,
        } => {
            let page_label = page.map(|p| format!("Page {p}"));
            rsx! {
                if let Some(url) = image {
                    figure { class: "viewer-figure page",
                        img {
                            src: "{url}",
                            alt: page_label.clone().map(|p| format!("{p} of {name}")).unwrap_or_else(|| name.clone()),
                            onerror: move |_| on_media_error.call(()),
                        }
                        if let Some(label) = page_label.clone() {
                            figcaption { "{label}" }
                        }
                    }
                } else if let Some(label) = page_label {
                    p { class: "viewer-label", "{label}" }
                }
                if !excerpt.is_empty() {
                    Passage { excerpt, code: false }
                }
            }
        }
        Viewer::Audio {
            src,
            start_ms,
            transcript,
        } => rsx! {
            if let Some(src) = src {
                audio {
                    id: "{MEDIA_ID}",
                    class: "viewer-audio",
                    controls: true,
                    preload: "metadata",
                    src: citations::media_at(&src, Some(start_ms)),
                    onmounted: move |_| seek(start_ms, false),
                    onerror: move |_| on_media_error.call(()),
                    "Your system can't play this audio."
                }
            } else {
                p { class: "faint", "{missing_media}" }
            }
            Transcript { segments: transcript }
        },
        Viewer::Video {
            src,
            start_ms,
            poster,
            transcript,
        } => rsx! {
            if let Some(src) = src {
                video {
                    id: "{MEDIA_ID}",
                    class: "player viewer-video",
                    controls: true,
                    playsinline: true,
                    preload: "metadata",
                    poster,
                    src: citations::media_at(&src, Some(start_ms)),
                    onmounted: move |_| seek(start_ms, false),
                    onerror: move |_| on_media_error.call(()),
                    "Your system can't play this video."
                }
            } else {
                if let Some(url) = poster {
                    figure { class: "viewer-figure",
                        img { src: "{url}", alt: "A frame of {name}", onerror: move |_| on_media_error.call(()) }
                    }
                }
                p { class: "faint", "{missing_media}" }
            }
            Transcript { segments: transcript }
        },
        Viewer::Passage { excerpt, code } => rsx! {
            Passage { excerpt, code }
        },
        Viewer::Missing => rsx! {
            p { class: "faint", "There's no passage or picture to show for this citation." }
        },
    }
}

/// Fetch a source's whole transcript into `AppCtx::transcripts`. A failure leaves the
/// viewer on what it has (the cited passages, at worst).
async fn fetch_transcript(app: AppCtx, source_id: Id) {
    let Some(api) = app.api() else { return };
    match api.source_transcript(&source_id).await {
        Ok(lines) => {
            let mut transcripts = app.transcripts;
            if transcripts.peek().get(&source_id) != Some(&lines) {
                transcripts.write().insert(source_id, lines);
            }
        }
        Err(e) => eprintln!("couldn't fetch the transcript of {source_id}: {e}"),
    }
}

/// Move the viewer's player to `start_ms` (once it knows its duration), optionally
/// playing from there.
fn seek(start_ms: u64, play: bool) {
    let id = serde_json::to_string(MEDIA_ID).unwrap_or_default();
    let _ = js(&format!(
        "MarginApp.seekMedia({id}, {}, {play});",
        citations::seconds(start_ms)
    ));
}

/// The cited passage, highlighted.
#[component]
fn Passage(excerpt: String, code: bool) -> Element {
    rsx! {
        if code {
            // Marking each line of code reads as noise; the whole block is the passage.
            pre { class: "code viewer-code", "aria-label": "Cited passage", code { "{excerpt}" } }
        } else {
            blockquote { class: "viewer-excerpt", mark { "{excerpt}" } }
        }
    }
}

/// The source's whole transcript (or, without one, the passages the lesson cites), by
/// time: the open citation's span marked and scrolled into view, the lesson's other
/// citations of it tinted. Any line plays the recording from its start.
#[component]
fn Transcript(segments: Vec<Segment>) -> Element {
    // Lines arrive after the viewer opens; bring the cited ones into view once they're
    // there (and again if they change).
    let first = citations::first_current(&segments);
    let count = segments.len();
    use_effect(use_reactive!(|first, count| {
        let _ = count;
        if first.is_some() {
            let id = serde_json::to_string(TRANSCRIPT_ID).unwrap_or_default();
            let _ = js(&format!("MarginApp.revealIn({id}, '.seg-line.current');"));
        }
    }));
    if segments.is_empty() {
        return rsx! {};
    }
    // Lines of the whole transcript carry no citation number.
    let whole = segments.iter().all(|s| s.n == 0);
    rsx! {
        div { class: "transcript",
            h3 { if whole { "Transcript" } else { "Cited passages" } }
            ol { id: "{TRANSCRIPT_ID}", class: "seg-list",
                for s in segments {
                    li { key: "{s.n}-{s.start_ms}",
                        button {
                            class: match (s.current, s.cited) {
                                (true, _) => "seg-line current",
                                (false, true) => "seg-line cited",
                                _ => "seg-line",
                            },
                            "aria-current": s.current.to_string(),
                            title: "Play from {citations::clock(s.start_ms)}",
                            onclick: move |_| seek(s.start_ms, true),
                            span { class: "seg-time", "{citations::clock(s.start_ms)}" }
                            if s.current {
                                mark { "{s.text}" }
                            } else {
                                span { "{s.text}" }
                            }
                            if s.cited && !s.current {
                                span { class: "sr-only", " (cited elsewhere in this lesson)" }
                            }
                        }
                    }
                }
            }
        }
    }
}
