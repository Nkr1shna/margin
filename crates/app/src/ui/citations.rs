//! A lesson's sources: the "Sources" list after the body and the source viewer that a
//! citation marker or a list entry opens, at the cited page, time, image or passage.

use dioxus::prelude::*;
use margin_shared::{Citation, Id, SourceKind, WorkspaceFile};

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

/// Give focus back to what opened the viewer once it has closed, or else to the element
/// `fallback` selects.
fn viewer_closed(fallback: &str) {
    let id = serde_json::to_string(DIALOG_ID).unwrap_or_default();
    let fallback = serde_json::to_string(fallback).unwrap_or_default();
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

/// The source viewer on one citation of a lesson, with Previous and Next through the
/// others.
#[component]
pub fn SourceViewerDialog(lesson_id: Id, n: u32) -> Element {
    let app = use_app();
    let all = app
        .lib
        .read()
        .lessons
        .get(&lesson_id)
        .map(|l| citations::ordered(&l.citations))
        .unwrap_or_default();
    rsx! {
        CitationViewer {
            citations: all,
            n,
            fallback: format!("#lesson-body button.cite[data-cite=\"{n}\"]"),
            on_go: move |to| open_viewer(app, &lesson_id, to),
            on_close: move |_| {
                let mut viewer = app.source_viewer;
                viewer.set(None);
            },
        }
    }
}

/// The source viewer: a dialog on passage `n` of `citations` (in order), with Previous
/// and Next through the others. Esc, the close button or a click outside closes it, and
/// focus goes back to what opened it (else to `fallback`).
#[component]
pub fn CitationViewer(
    citations: Vec<Citation>,
    n: u32,
    fallback: String,
    on_go: EventHandler<u32>,
    on_close: EventHandler<()>,
) -> Element {
    let app = use_app();
    let close = use_callback(move |()| {
        on_close.call(());
        viewer_closed(&fallback);
    });
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
    let all = citations;
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
        // What it cites changed under the viewer (a lesson regenerated or deleted).
        return rsx! {};
    };
    // A folder's files aren't among the sources; the folder's list has them once opened.
    let source = lib.sources.get(&c.source_id).or_else(|| {
        lib.folder_files
            .values()
            .find_map(|files| files.get(&c.source_id))
    });
    let media_url = source.and_then(|s| s.media_url.clone());
    let source_gone = source.is_none() && c.file.is_none();
    let folder_attached = c
        .file
        .as_ref()
        .is_some_and(|f| lib.workspaces.contains_key(&f.workspace_id));
    let folder_line = citations::folder_line(
        &c,
        c.file
            .as_ref()
            .and_then(|f| lib.workspaces.get(&f.workspace_id)),
    );
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
    let go = move |to: u32| {
        open_error.set(None);
        on_go.call(to);
    };
    let (mut go_prev, mut go_next) = (go, go);
    let meta = match locator(&c) {
        Some(at) => format!("{} · {at}", kind_label(c.kind)),
        None => kind_label(c.kind).to_string(),
    };

    rsx! {
        div {
            class: "modal-scrim center",
            onclick: move |_| close.call(()),
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
                        close.call(());
                    }
                },
                div { class: "viewer-head",
                    span { class: "cite-n", "aria-hidden": "true", "{n}" }
                    Icon { name: kind_icon(c.kind), class: "cite-ic" }
                    div { class: "grow",
                        h2 { id: "viewer-title", title: "{c.source_name}", "{c.source_name}" }
                        small { "{meta}" }
                        if let Some(line) = folder_line {
                            small { class: "viewer-folder",
                                Icon { name: "folder" }
                                "{line}"
                            }
                        }
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
                        onclick: move |_| close.call(()),
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
                        source_id: c.source_id.clone(),
                        excerpt: c.excerpt.trim().to_string(),
                        file: c.file.clone(),
                        folder_attached,
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

/// Whether a folder file is still among its folder's files on the service (a 404 for
/// the folder means it was removed). Unsure (offline) counts as there.
async fn folder_file_present(app: AppCtx, file: &WorkspaceFile, source_id: &str) -> bool {
    let Some(api) = app.api() else { return true };
    match api.workspace_files(&file.workspace_id).await {
        Ok(files) => {
            let present = files.iter().any(|f| f.id == source_id);
            let mut lib = app.lib;
            lib.write().replace_folder_files(&file.workspace_id, files);
            present
        }
        Err(e) if e.status == Some(404) => false,
        Err(_) => true,
    }
}

#[component]
fn ViewerContent(
    view: Viewer,
    name: String,
    source_gone: bool,
    source_id: Id,
    excerpt: String,
    /// Set for a file of an attached folder.
    file: Option<WorkspaceFile>,
    /// Its folder is still attached (a removed folder's files are gone from the service).
    folder_attached: bool,
    on_media_error: EventHandler<()>,
) -> Element {
    let app = use_app();
    // A folder file's picture stopped loading because the file left the folder (or the
    // folder was removed): the passage as it was, and why.
    let mut gone = use_signal(|| file.is_some() && !folder_attached);
    let on_media_error = EventHandler::new(move |()| match file.clone() {
        None => on_media_error.call(()),
        Some(file) => {
            let source_id = source_id.clone();
            spawn(async move {
                if folder_file_present(app, &file, &source_id).await {
                    // Still there: the signed address expired.
                    on_media_error.call(());
                } else {
                    gone.set(true);
                }
            });
        }
    });
    if *gone.read() {
        return rsx! {
            div { class: "notice", role: "status",
                Icon { name: "alert" }
                div { class: "body", {citations::FOLDER_FILE_GONE} }
            }
            if excerpt.is_empty() {
                p { class: "faint", "There's no passage to show for this citation." }
            } else {
                Passage { excerpt, code: false }
            }
        };
    }
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

#[cfg(test)]
mod tests {
    use crate::fake;
    use crate::store::LibraryState;
    use crate::ui::test_render::{Extra, render_with};

    fn shown(n: usize, lib: LibraryState) -> String {
        let extra = Extra {
            shown_passage: Some(fake::sample_folder_citations()[n].clone()),
            ..Default::default()
        };
        render_with("/", lib, None, extra)
    }

    fn lib() -> LibraryState {
        let mut lib = LibraryState::from_library(fake::sample_library());
        lib.replace_workspaces(fake::sample_workspaces());
        lib
    }

    #[test]
    fn folder_citations_open_with_where_the_file_is() {
        // A text passage.
        let html = shown(0, lib());
        assert!(html.contains("notes/ch1/intro.md"), "{html}");
        assert!(html.contains("In “notes” on test-mac · ch1/intro.md"));
        assert!(html.contains("A Fourier series rebuilds a periodic signal"));
        // A PDF page: its picture and its text.
        let html = shown(1, lib());
        assert!(html.contains("In “notes” on test-mac · paper.pdf"));
        assert!(html.contains("Page 3"));
        assert!(html.contains("page-3.png"));
        assert!(html.contains("Each coefficient is an inner product"));
        // A picture.
        let html = shown(2, lib());
        assert!(html.contains("In “notes” on test-mac · figure.png"));
        assert!(html.contains(r#"class="viewer-figure""#));
        assert!(!html.contains(crate::citations::FOLDER_FILE_GONE));
    }

    #[test]
    fn a_file_of_a_removed_folder_shows_the_passage_as_it_was() {
        let html = shown(3, lib());
        assert!(
            html.contains("In a folder that is no longer attached · old/gibbs.pdf"),
            "{html}"
        );
        assert!(html.contains(crate::citations::FOLDER_FILE_GONE));
        assert!(html.contains("The overshoot near a jump stays at about 9%."));
        // Its picture isn't tried.
        assert!(!html.contains("page-1.png"));
    }
}
