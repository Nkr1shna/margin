//! Images, PDF pages, audio and video sources end to end with fakes (see
//! `FakeSourceTools`): upload → Ready, retrieval hands pictures to the writer as image
//! inputs and recordings as their transcript, the lesson's `[^n]` markers are renumbered
//! into citations, and the source media URLs are served signed, with range requests.

mod common;

use common::*;
use margin_service::sources::{ChunkRow, MediaKind, MediaRow};
use margin_shared::{
    Citation, Event, Lesson, LessonStatus, Library, ProposalStatus, Source, SourceKind,
    SourceStatus, Topic,
};
use margin_shared::{TranscriptSegment, routes};
use serde_json::json;

async fn transcript(h: &Harness, token: &str, id: &str) -> Vec<TranscriptSegment> {
    let r = h
        .get(token, &routes::source_transcript(id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

async fn library(h: &Harness, token: &str) -> Library {
    h.get(token, "/api/library")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn upload_ok(h: &Harness, token: &str, name: &str, content: &str) -> Source {
    let r = h.upload(token, name, None, content.to_string()).await;
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

/// Waits until the source is no longer Queued or Processing.
async fn settled(h: &Harness, token: &str, id: &str) -> Source {
    eventually(|| async {
        library(h, token).await.sources.into_iter().find(|s| {
            s.id == id
                && !matches!(
                    s.status,
                    SourceStatus::Queued | SourceStatus::Processing { .. }
                )
        })
    })
    .await
}

/// A source's picture and sound passages, in order, in the shape the old `media` table
/// had.
async fn media_rows(h: &Harness, source_id: &str) -> Vec<MediaRow> {
    h.state
        .store
        .passages_of(source_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|p| p.modality != "text")
        .enumerate()
        .map(|(i, p)| MediaRow {
            kind: match (p.modality.as_str(), p.file.as_deref()) {
                ("sound", _) => MediaKind::Audio,
                ("frame", _) => MediaKind::Frame,
                (_, Some("original")) => MediaKind::Image,
                _ => MediaKind::Page,
            },
            source_id: p.document_id,
            topic_id: p.scope_ids.first().cloned(),
            ord: i as u32,
            file: p.file,
            page: p.page.map(|v| v as u32),
            start_ms: p.start_ms.map(|v| v as u64),
            end_ms: p.end_ms.map(|v| v as u64),
            text: p.text,
            vector: p.vector,
        })
        .collect()
}

/// A source's text passages, in order, in the shape the old `chunk` table had.
async fn chunk_rows(h: &Harness, source_id: &str) -> Vec<ChunkRow> {
    h.state
        .store
        .passages_of(source_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|p| p.modality == "text")
        .enumerate()
        .map(|(i, p)| ChunkRow {
            source_id: p.document_id,
            topic_id: p.scope_ids.first().cloned(),
            ord: i as u32,
            page: p.page.map(|v| v as u32),
            start_ms: p.start_ms.map(|v| v as u64),
            end_ms: p.end_ms.map(|v| v as u64),
            text: p.text,
            vector: p.vector,
        })
        .collect()
}

/// Has the agent propose a one-lesson course on the sources, accepts it and waits for
/// the lesson to settle.
async fn lesson_from(h: &Harness, token: &str, prompt: &str, sources: &[&Source]) -> Lesson {
    let mut req = ask_request(prompt, 1, false);
    req.source_ids = sources.iter().map(|s| s.id.clone()).collect();
    let p = h.propose(token, &req).await;
    assert_eq!(p.status, ProposalStatus::Ready);
    let r = h
        .post(token, &format!("/api/proposals/{}/accept", p.id))
        .json(&json!({"outline": p.outline.unwrap()}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let topic: Topic = r.json().await.unwrap();
    eventually(|| async {
        library(h, token).await.lessons.into_iter().find(|l| {
            l.topic_id == topic.id
                && matches!(l.status, LessonStatus::Ready | LessonStatus::Failed { .. })
        })
    })
    .await
}

/// Every retrieved hit is packed, whatever its score (`Budget.min_score_pct` 0), and at
/// most `images_max` pictures: the tests below check what is sent, not the cut-off.
async fn keep_every_hit(h: &Harness, token: &str, images_max: u32) {
    let r = h
        .patch(token, "/api/settings")
        .json(&json!({"pipeline": {"budget": {"min_score_pct": 0, "images_max": images_max}}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

/// Fetches a signed URL: status, content type, body.
async fn fetch(h: &Harness, url: &str) -> (u16, String, Vec<u8>) {
    let r = h.http.get(url).send().await.unwrap();
    let status = r.status().as_u16();
    let kind = r
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    (status, kind, r.bytes().await.unwrap().to_vec())
}

fn expect_signed(h: &Harness, url: &str, source_id: &str, file: &str) {
    let prefix = format!("{}/media/sources/{source_id}/{file}?exp=", h.url);
    assert!(url.starts_with(&prefix), "{url}");
    let exp: i64 = url[prefix.len()..]
        .split('&')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let hours = (exp - chrono::Utc::now().timestamp()) / 3600;
    assert!((5..=6).contains(&hours), "{hours}");
    assert!(url.ends_with(&h.state.source_media_signature(source_id, file, exp)));
}

const DIAGRAM: &str = "Fourier coefficient diagram: the projection of a square wave onto cosines";

#[tokio::test(flavor = "multi_thread")]
async fn images_are_embedded_shown_to_the_writer_and_cited() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    h.configure_ai(&t).await;
    let mut socket = h.socket(&t).await;

    let image = upload_ok(&h, &t, "figures/diagram.png", DIAGRAM).await;
    assert_eq!(image.kind, SourceKind::Image);
    expect_signed(
        &h,
        image.media_url.as_deref().unwrap(),
        &image.id,
        "original",
    );
    let image = settled(&h, &t, &image.id).await;
    assert_eq!(
        image.status,
        SourceStatus::Ready {
            chunks: 1,
            pages: None,
            warning: None,
        }
    );
    expect_signed(
        &h,
        image.media_url.as_deref().unwrap(),
        &image.id,
        "original",
    );
    // Events carry signed URLs too.
    let events = socket
        .until(|e| matches!(e, Event::SourceChanged { source } if source.status != SourceStatus::Queued && !matches!(source.status, SourceStatus::Processing { .. })))
        .await;
    assert!(events.iter().all(|e| match e {
        Event::SourceChanged { source } => source.media_url.is_some(),
        _ => true,
    }));
    let rows = media_rows(&h, &image.id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, MediaKind::Image);
    assert_eq!(rows[0].file.as_deref(), Some("original"));
    assert_eq!(rows[0].vector.len(), margin_embed::DIM);
    assert!(chunk_rows(&h, &image.id).await.is_empty());

    // The original is served signed, as an image, with range requests.
    let url = image.media_url.clone().unwrap();
    let (status, kind, body) = fetch(&h, &url).await;
    assert_eq!((status, kind.as_str()), (200, "image/png"));
    assert_eq!(body, DIAGRAM.as_bytes());
    let r = h
        .http
        .get(&url)
        .header("range", "bytes=0-6")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    assert_eq!(
        r.headers()["content-range"].to_str().unwrap(),
        format!("bytes 0-6/{}", DIAGRAM.len())
    );
    assert_eq!(r.headers()["x-content-type-options"], "nosniff");
    assert!(
        r.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .contains("diagram.png")
    );
    assert_eq!(r.bytes().await.unwrap().as_ref(), &DIAGRAM.as_bytes()[..7]);
    let (status, _, _) = fetch(&h, &url.replace("sig=", "sig=00")).await;
    assert_eq!(status, 403);
    let (status, _, _) = fetch(&h, &url.replace("/original?", "/page-1.png?")).await;
    assert_eq!(status, 403, "a signature is for one file");
    let (status, _, _) = fetch(&h, url.split('?').next().unwrap()).await;
    assert_eq!(status, 403);
    // A video's signature doesn't open a source file.
    let exp = chrono::Utc::now().timestamp() + 600;
    let video_sig = h.state.media_signature(&image.id, "original", exp);
    let (status, _, _) = fetch(
        &h,
        &format!(
            "{}/media/sources/{}/original?exp={exp}&sig={video_sig}",
            h.url, image.id
        ),
    )
    .await;
    assert_eq!(status, 403);

    // The writer is shown the picture and cites it; the markers are renumbered.
    *h.generator.lesson_body.lock().unwrap() =
        Some("The square wave projects onto cosines.[^1] Unknown[^7].\n".into());
    let lesson = lesson_from(&h, &t, "Fourier coefficient projection", &[&image]).await;
    assert_eq!(lesson.status, LessonStatus::Ready);
    {
        let lessons = h.generator.lesson_requests.lock().unwrap();
        let passage = &lessons[0].passages[0];
        assert_eq!(passage.source, "figures/diagram.png");
        let picture = passage.image.as_ref().unwrap();
        assert_eq!(picture.media_type, "image/jpeg");
        assert_eq!(picture.data, DIAGRAM.as_bytes());
    }
    assert_eq!(
        lesson.body_md,
        "The square wave projects onto cosines.[^1] Unknown.\n"
    );
    assert_eq!(lesson.citations.len(), 1);
    let c = &lesson.citations[0];
    assert_eq!(
        (c.n, c.source_id.as_str(), c.kind),
        (1, image.id.as_str(), SourceKind::Image)
    );
    assert_eq!(c.source_name, "figures/diagram.png");
    assert_eq!(c.excerpt, "");
    let image_url = c.image_url.clone().unwrap();
    expect_signed(&h, &image_url, &image.id, "original");
    assert_eq!(fetch(&h, &image_url).await.0, 200);

    // Deleting the source removes its media rows, and its links stop working.
    let r = h
        .delete(&t, &format!("/api/sources/{}", image.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    assert!(media_rows(&h, &image.id).await.is_empty());
    assert_eq!(fetch(&h, &image_url).await.0, 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn pdf_pages_are_pictures_and_citations_point_at_pages() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    h.configure_ai(&t).await;
    keep_every_hit(&h, &t, 8).await;
    let pdf = upload_ok(
        &h,
        &t,
        "dsp.pdf",
        "Cover page about cooking.\x0cThe Fourier coefficient formula for a periodic signal.",
    )
    .await;
    let pdf = settled(&h, &t, &pdf.id).await;
    assert_eq!(
        pdf.status,
        SourceStatus::Ready {
            chunks: 4,
            pages: Some(2),
            warning: None,
        }
    );
    let rows = media_rows(&h, &pdf.id).await;
    let files: Vec<_> = rows.iter().map(|r| r.file.clone().unwrap()).collect();
    assert_eq!(files, ["page-1.png", "page-2.png"]);
    assert!(rows.iter().all(|r| r.kind == MediaKind::Page));
    assert_eq!(rows[1].page, Some(2));

    // Page 2's text is cited; its picture comes along as the citation's thumbnail.
    *h.generator.lesson_body.lock().unwrap() = Some("Text.[^2] Picture.[^1]\n".into());
    let lesson = lesson_from(&h, &t, "Fourier coefficient formula", &[&pdf]).await;
    let passages = h.generator.lesson_requests.lock().unwrap()[0]
        .passages
        .clone();
    let first = &passages[0];
    let second = &passages[1];
    assert_eq!(lesson.body_md, "Text.[^1] Picture.[^2]\n");
    let by_n = |n: u32| lesson.citations.iter().find(|c| c.n == n).unwrap().clone();
    let cite_of = |p: &margin_ai::Passage| -> (Option<u32>, bool) { (p.page, p.image.is_some()) };
    // [^1] is now the second passage, [^2] the first.
    assert_eq!(
        (by_n(1).page, by_n(1).excerpt.as_str()),
        (second.page, second.text.trim())
    );
    assert_eq!(
        (by_n(2).page, by_n(2).excerpt.as_str()),
        (first.page, first.text.trim())
    );
    assert!(cite_of(first).0.is_some() && cite_of(second).0.is_some());
    for c in &lesson.citations {
        assert_eq!(c.kind, SourceKind::Pdf);
        let file = format!("page-{}.png", c.page.unwrap());
        let url = c.image_url.clone().unwrap();
        expect_signed(&h, &url, &pdf.id, &file);
        let (status, kind, body) = fetch(&h, &url).await;
        assert_eq!((status, kind.as_str()), (200, "image/png"));
        assert!(body.starts_with(FAKE_PICTURE.as_bytes()));
    }
    // The original PDF is served as one.
    let (status, kind, _) = fetch(&h, pdf.media_url.as_deref().unwrap()).await;
    assert_eq!((status, kind.as_str()), (200, "application/pdf"));
    // A page that doesn't exist, signed, is a 404; a bad name never gets that far.
    let exp = chrono::Utc::now().timestamp() + 600;
    for (file, want) in [("page-9.png", 404), ("page-1.jpg", 404), ("..%2Ffile", 404)] {
        let sig = h.state.source_media_signature(&pdf.id, file, exp);
        let url = format!(
            "{}/media/sources/{}/{file}?exp={exp}&sig={sig}",
            h.url, pdf.id
        );
        assert_eq!(fetch(&h, &url).await.0, want, "{file}");
    }
}

const TALK: &str = "today we derive the fourier coefficient by projecting the signal onto each cosine \
                    and then we look at convergence near jumps where ringing appears";

#[tokio::test(flavor = "multi_thread")]
async fn audio_is_embedded_transcribed_and_cited_with_times() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    h.configure_ai(&t).await;
    keep_every_hit(&h, &t, 8).await;
    let audio = upload_ok(&h, &t, "talk.m4a", TALK).await;
    assert_eq!(audio.kind, SourceKind::Audio);
    let audio = settled(&h, &t, &audio.id).await;
    // 24 words: 24 s of sound, three 10-s windows; one transcript passage (under 30 s).
    assert_eq!(
        audio.status,
        SourceStatus::Ready {
            chunks: 4,
            pages: None,
            warning: None,
        }
    );
    let windows = media_rows(&h, &audio.id).await;
    assert_eq!(windows.len(), 3);
    assert!(
        windows
            .iter()
            .all(|w| w.kind == MediaKind::Audio && w.file.is_none())
    );
    assert_eq!(
        windows
            .iter()
            .map(|w| (w.start_ms.unwrap(), w.end_ms.unwrap()))
            .collect::<Vec<_>>(),
        [(0, 10_000), (10_000, 20_000), (20_000, 24_000)]
    );
    assert!(windows[0].text.starts_with("today we derive"));
    let chunks = chunk_rows(&h, &audio.id).await;
    assert_eq!(chunks.len(), 1);
    assert_eq!(
        (chunks[0].start_ms, chunks[0].end_ms),
        (Some(0), Some(23_800))
    );
    assert_eq!(
        chunks[0].text,
        TALK.split_whitespace().collect::<Vec<_>>().join(" ")
    );
    let (status, kind, _) = fetch(&h, audio.media_url.as_deref().unwrap()).await;
    assert_eq!((status, kind.as_str()), (200, "audio/mp4"));

    *h.generator.lesson_body.lock().unwrap() = Some("Projection onto cosines.[^1]\n".into());
    let lesson = lesson_from(&h, &t, "fourier coefficient projecting cosine", &[&audio]).await;
    let passages = h.generator.lesson_requests.lock().unwrap()[0]
        .passages
        .clone();
    // The transcript passage and the windows overlapping it aren't both sent.
    assert!(
        passages
            .iter()
            .all(|p| p.image.is_none() && p.start_ms.is_some())
    );
    assert_eq!(passages.len(), 1, "{passages:?}");
    let c: &Citation = &lesson.citations[0];
    assert_eq!(c.kind, SourceKind::Audio);
    assert_eq!(
        (c.start_ms, c.end_ms),
        (passages[0].start_ms, passages[0].end_ms)
    );
    assert_eq!(c.excerpt, passages[0].text);
    assert_eq!(c.image_url, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_transcriber_recordings_are_ready_with_a_warning_and_read_again() {
    use std::sync::atomic::Ordering;
    let h = start().await;
    let t = h.sign_in("Mac").await;
    h.source_tools.no_transcriber.store(true, Ordering::SeqCst);
    let audio = upload_ok(&h, &t, "talk.mp3", TALK).await;
    let audio = settled(&h, &t, &audio.id).await;
    match &audio.status {
        SourceStatus::Ready {
            chunks: 3,
            warning: Some(warning),
            ..
        } => {
            assert!(warning.contains("speech model"), "{warning}");
            assert!(warning.contains("wasn't transcribed"), "{warning}");
        }
        other => panic!("expected Ready with a warning, got {other:?}"),
    }
    assert!(audio.status.can_read_again());
    // The sound was embedded all the same, and there's no transcript to show.
    assert_eq!(media_rows(&h, &audio.id).await.len(), 3);
    assert!(
        media_rows(&h, &audio.id)
            .await
            .iter()
            .all(|w| w.text.is_empty())
    );
    assert!(transcript(&h, &t, &audio.id).await.is_empty());

    let video = upload_ok(
        &h,
        &t,
        "lecture.mov",
        "frame: a circle rolling along a line\nsay: watch the point on the rim",
    )
    .await;
    let video = settled(&h, &t, &video.id).await;
    match &video.status {
        SourceStatus::Ready {
            chunks: 2,
            warning: Some(warning),
            ..
        } => assert!(
            warning.starts_with("Its soundtrack wasn't transcribed"),
            "{warning}"
        ),
        other => panic!("expected Ready with a warning, got {other:?}"),
    }

    // Once the transcriber is there, Read again reads it in place: same id and topic,
    // no warning, and its transcript.
    h.source_tools.no_transcriber.store(false, Ordering::SeqCst);
    let r = h
        .post(&t, &routes::source_reprocess(&audio.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let queued: Source = r.json().await.unwrap();
    assert_eq!(
        (queued.id.as_str(), queued.status.clone()),
        (audio.id.as_str(), SourceStatus::Queued)
    );
    let again = eventually(|| async {
        library(&h, &t).await.sources.into_iter().find(|s| {
            s.id == audio.id && matches!(s.status, SourceStatus::Ready { warning: None, .. })
        })
    })
    .await;
    assert_eq!(again.topic_id, audio.topic_id);
    assert_eq!(
        again.status,
        SourceStatus::Ready {
            chunks: 4,
            pages: None,
            warning: None
        }
    );
    // Its rows were replaced, not added to.
    assert_eq!(media_rows(&h, &audio.id).await.len(), 3);
    assert_eq!(chunk_rows(&h, &audio.id).await.len(), 1);
    let lines = transcript(&h, &t, &audio.id).await;
    assert!(!lines.is_empty());
    assert_eq!(lines[0].start_ms, 0);
    assert_eq!(
        lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        TALK.split_whitespace().collect::<Vec<_>>().join(" ")
    );

    // Nothing to read again for a source being read, or one that doesn't exist.
    let r = h
        .post(&t, &routes::source_reprocess(&video.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = h
        .post(&t, &routes::source_reprocess(&video.id))
        .send()
        .await
        .unwrap();
    assert!(matches!(r.status().as_u16(), 200 | 409), "{}", r.status());
    let r = h
        .post(&t, &routes::source_reprocess("nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn transcripts_of_other_kinds_are_empty() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    let image = upload_ok(&h, &t, "square.png", "picture: a square wave").await;
    settled(&h, &t, &image.id).await;
    assert!(transcript(&h, &t, &image.id).await.is_empty());
    let r = h
        .get(&t, &routes::source_transcript("nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn video_frames_go_to_the_writer_with_what_was_said() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    h.configure_ai(&t).await;
    keep_every_hit(&h, &t, 8).await;
    let video = upload_ok(
        &h,
        &t,
        "lectures/cycloid.mp4",
        "frame: title card\n\
         frame: a cycloid traced by a rolling circle\n\
         frame: credits\n\
         say: welcome to the lecture today we draw a cycloid with a rolling circle and then thank everyone",
    )
    .await;
    let video = settled(&h, &t, &video.id).await;
    let rows = media_rows(&h, &video.id).await;
    let frames: Vec<_> = rows.iter().filter(|r| r.kind == MediaKind::Frame).collect();
    assert_eq!(
        frames
            .iter()
            .map(|f| f.file.clone().unwrap())
            .collect::<Vec<_>>(),
        ["frame-2500.jpg", "frame-7500.jpg", "frame-12500.jpg"]
    );
    // Each frame carries what was said in its 5 s ([5 s, 10 s) for the one at 7.5 s).
    assert_eq!(frames[1].start_ms, Some(7_500));
    assert_eq!(frames[1].text, "we draw a cycloid with");
    assert_eq!(frames[1].end_ms, None);
    let windows: Vec<_> = rows.iter().filter(|r| r.kind == MediaKind::Audio).collect();
    assert_eq!(windows.len(), 2);
    assert!(matches!(
        video.status,
        SourceStatus::Ready {
            chunks: 6,
            pages: None,
            warning: None,
        }
    ));

    *h.generator.lesson_body.lock().unwrap() = Some("A cycloid.[^1]\n".into());
    let lesson = lesson_from(&h, &t, "cycloid traced by a rolling circle", &[&video]).await;
    let passages = h.generator.lesson_requests.lock().unwrap()[0]
        .passages
        .clone();
    let frame = passages
        .iter()
        .find(|p| p.image.is_some() && p.start_ms == Some(7_500))
        .expect("the cycloid frame among the passages");
    assert_eq!(frame.start_ms, Some(7_500));
    assert_eq!(frame.text, "we draw a cycloid with");
    assert!(
        frame
            .image
            .as_ref()
            .unwrap()
            .data
            .ends_with(b"a cycloid traced by a rolling circle")
    );
    let cited = &passages[0];
    let c = &lesson.citations[0];
    assert_eq!(c.kind, SourceKind::Video);
    assert_eq!(c.start_ms, cited.start_ms);
    // A picture for every video citation: the frame itself or the one nearest the words.
    let url = c.image_url.clone().unwrap();
    assert!(url.contains("/frame-"), "{url}");
    let (status, kind, _) = fetch(&h, &url).await;
    assert_eq!((status, kind.as_str()), (200, "image/jpeg"));
    let (status, kind, _) = fetch(&h, video.media_url.as_deref().unwrap()).await;
    assert_eq!((status, kind.as_str()), (200, "video/mp4"));
}

#[tokio::test(flavor = "multi_thread")]
async fn pictures_are_capped_by_the_budgets_images_max() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    h.configure_ai(&t).await;
    keep_every_hit(&h, &t, 4).await;
    let mut sources = Vec::new();
    for i in 0..6 {
        sources.push(
            upload_ok(
                &h,
                &t,
                &format!("wave{i}.png"),
                &format!("square wave figure {i}"),
            )
            .await,
        );
    }
    let text = upload_ok(
        &h,
        &t,
        "notes.md",
        "Square wave one.\n\nSquare wave two.\n\nSquare wave three.\n\nSquare wave four.\n\n\
         Square wave five.\n\nSquare wave six.",
    )
    .await;
    sources.push(text);
    for s in &sources {
        settled(&h, &t, &s.id).await;
    }
    let refs: Vec<&Source> = sources.iter().collect();
    lesson_from(&h, &t, "square wave figure", &refs).await;
    let passages = h.generator.lesson_requests.lock().unwrap()[0]
        .passages
        .clone();
    // Six figures match; the budget lets four pictures in, and the notes come too.
    assert_eq!(passages.iter().filter(|p| p.image.is_some()).count(), 4);
    assert!(passages.iter().any(|p| p.image.is_none()), "{passages:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_regeneration_keeps_the_old_citations() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    h.configure_ai(&t).await;
    let image = upload_ok(&h, &t, "diagram.png", DIAGRAM).await;
    let image = settled(&h, &t, &image.id).await;
    *h.generator.lesson_body.lock().unwrap() = Some("Cited.[^1]\n".into());
    let lesson = lesson_from(&h, &t, "Fourier coefficient", &[&image]).await;
    assert_eq!(lesson.citations.len(), 1);

    h.generator
        .fail_lessons
        .lock()
        .unwrap()
        .insert(lesson.title.clone());
    let mut socket = h.socket(&t).await;
    let r = h
        .post(&t, &format!("/api/lessons/{}/regenerate", lesson.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let events = socket
        .until(|e| matches!(e, Event::LessonChanged { lesson } if matches!(lesson.status, LessonStatus::Failed { .. })))
        .await;
    // While it was rewritten the old citations were set aside…
    assert!(events.iter().any(|e| matches!(e, Event::LessonChanged { lesson } if lesson.status == LessonStatus::Generating && lesson.citations.is_empty())));
    // …and they come back with the old body.
    let Some(Event::LessonChanged { lesson: failed }) = events.last() else {
        unreachable!()
    };
    assert_eq!(failed.body_md, "Cited.[^1]\n");
    assert_eq!(failed.citations.len(), 1);
    assert_eq!(failed.citations[0].source_id, image.id);
    expect_signed(
        &h,
        failed.citations[0].image_url.as_deref().unwrap(),
        &image.id,
        "original",
    );
}

/// The real pipeline, when the models and the test media are on this machine (see
/// margin-embed's real-model tests): the ONNX 8-bit model in `MARGIN_EMBED_MODEL_DIR` or
/// `~/.cache/margin-embed-test/onnx-q8`, the int8 speech model in `MARGIN_PARAKEET_DIR`,
/// and the media in `MARGIN_EMBED_MEDIA_DIR` or `~/.cache/margin-embed-test/media`.
#[tokio::test(flavor = "multi_thread")]
async fn real_media_end_to_end_when_the_model_is_here() {
    use margin_embed::transcript_parakeet::{SpeechDtype, SpeechModelFiles};
    if std::env::var("MARGIN_REAL_MODEL_TESTS").map_or(true, |v| v != "1") {
        eprintln!("skipping: real-model tests need MARGIN_REAL_MODEL_TESTS=1");
        return;
    }
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    let model = std::env::var_os("MARGIN_EMBED_MODEL_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| home.join(".cache/margin-embed-test/onnx-q8"));
    let media = std::env::var_os("MARGIN_EMBED_MEDIA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| home.join(".cache/margin-embed-test/media"));
    if !margin_embed::ModelFiles::new(&model).is_complete() || !media.join("lecture.mp4").exists() {
        eprintln!("skipping: no real model or test media");
        return;
    }
    let Some(speech) = std::env::var_os("MARGIN_PARAKEET_DIR")
        .map(std::path::PathBuf::from)
        .filter(|d| SpeechModelFiles::new(d, SpeechDtype::Int8).is_complete())
    else {
        eprintln!("skipping: no speech model in MARGIN_PARAKEET_DIR");
        return;
    };
    let h = start_with(Setup {
        real_model: Some(model),
        ..Default::default()
    })
    .await;
    // The speech model, where the service looks for it (it loads on first use).
    std::os::unix::fs::symlink(
        &speech,
        h.data_dir.join("models").join(SpeechDtype::Int8.dir_name()),
    )
    .unwrap();
    let t = h.sign_in("Mac").await;
    h.configure_ai(&t).await;
    let read = |name: &str| std::fs::read(media.join(name)).unwrap();
    let mut uploaded = Vec::new();
    for name in ["diagram.png", "coast.jpg", "speech.wav", "lecture.mp4"] {
        let r = h.upload(&t, name, None, read(name)).await;
        assert_eq!(r.status(), 200);
        let s: Source = r.json().await.unwrap();
        uploaded.push(s);
    }
    let mut ready = Vec::new();
    for s in &uploaded {
        let s = settled_slowly(&h, &t, &s.id).await;
        eprintln!("{}: {:?}", s.name, s.status);
        assert!(
            matches!(s.status, SourceStatus::Ready { .. }),
            "{}: {:?}",
            s.name,
            s.status
        );
        ready.push(s);
    }
    // The lecture: 3 frames, 2 sound windows, and its spoken sentence.
    let lecture = &ready[3];
    let chunks = chunk_rows(&h, &lecture.id).await;
    assert!(
        chunks
            .iter()
            .any(|c| c.text.to_lowercase().contains("mitochondria")),
        "{chunks:?}"
    );
    assert_eq!(media_rows(&h, &lecture.id).await.len(), 5);

    let refs: Vec<&Source> = ready.iter().collect();
    *h.generator.lesson_body.lock().unwrap() = Some("Shapes.[^1]\n".into());
    let lesson = lesson_from(&h, &t, "a red circle and a blue square", &refs).await;
    let passages = h.generator.lesson_requests.lock().unwrap()[0]
        .passages
        .clone();
    for p in &passages {
        eprintln!(
            "{} page={:?} at={:?} image={} {:?}",
            p.source,
            p.page,
            p.start_ms,
            p.image.as_ref().map_or(0, |i| i.data.len()),
            p.text.chars().take(60).collect::<String>()
        );
    }
    let best = &passages[0];
    assert!(
        best.source == "diagram.png" || best.source == "lecture.mp4",
        "{}",
        best.source
    );
    let picture = best.image.as_ref().expect("the best match is a picture");
    assert_eq!(picture.media_type, "image/jpeg");
    assert_eq!(&picture.data[..2], &[0xFF, 0xD8]);
    assert!(passages.iter().filter(|p| p.image.is_some()).count() <= 4);
    let url = lesson.citations[0].image_url.clone().unwrap();
    assert_eq!(fetch(&h, &url).await.0, 200);

    // A question about the speech finds what was said.
    let ids: Vec<String> = ready.iter().map(|s| s.id.clone()).collect();
    let retrieved =
        margin_service::sources::passages(&h.state, &ids, "what makes energy in the cell").await;
    let passages: Vec<_> = retrieved.iter().map(|r| r.passage.clone()).collect();
    for p in &passages {
        eprintln!(
            "{} at={:?}-{:?} image={} {:?}",
            p.source,
            p.start_ms,
            p.end_ms,
            p.image.is_some(),
            p.text.chars().take(60).collect::<String>()
        );
    }
    let spoken = passages
        .iter()
        .find(|p| p.text.to_lowercase().contains("mitochondria"))
        .expect("the transcript among the passages");
    assert!(spoken.start_ms.is_some());
    eprintln!("first citation: {:?}", retrieved[0].citation);
}

/// Like `settled`, for the real model: compiling the encoders the first time takes a
/// while.
async fn settled_slowly(h: &Harness, token: &str, id: &str) -> Source {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(900);
    loop {
        let found = library(h, token).await.sources.into_iter().find(|s| {
            s.id == id
                && !matches!(
                    s.status,
                    SourceStatus::Queued | SourceStatus::Processing { .. }
                )
        });
        if let Some(s) = found {
            return s;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "source {id} never settled"
        );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

/// A video job is traced under its id: each scene call (`model_call`, route `write`,
/// `job_id`, `step` = attempt) and each render (`job_step` `render`), a failed render
/// with a code, never its message.
#[tokio::test(flavor = "multi_thread")]
async fn video_jobs_are_traced_under_their_id() {
    use margin_shared::context::{TraceKind, TraceRow};
    use margin_shared::pipeline::Route;
    use margin_shared::{Video, VideoStatus};
    use std::sync::atomic::Ordering;

    let h = start().await;
    let t = h.sign_in("Mac").await;
    h.configure_ai(&t).await;
    // One scene failure: the model repairs it and the second render succeeds.
    h.renderer.scene_failures.store(1, Ordering::SeqCst);
    let p = h.propose(&t, &ask_request("traced videos", 1, false)).await;
    assert_eq!(p.status, ProposalStatus::Ready);
    let r = h
        .post(&t, &format!("/api/proposals/{}/accept", p.id))
        .json(&json!({"outline": p.outline.clone().unwrap()}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let topic: Topic = r.json().await.unwrap();
    let lesson = eventually(|| async {
        library(&h, &t)
            .await
            .lessons
            .into_iter()
            .find(|l| l.topic_id == topic.id && l.status == LessonStatus::Ready)
    })
    .await;
    let v: Video = h
        .post(&t, &format!("/api/lessons/{}/video", lesson.id))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    eventually(|| async {
        let v: Video = h
            .get(&t, &format!("/api/videos/{}", v.id))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        (v.status == VideoStatus::Ready).then_some(())
    })
    .await;

    let r = h
        .get(&t, routes::TRACES)
        .query(&[("trace_id", v.id.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body = r.text().await.unwrap();
    let rows: Vec<TraceRow> = body
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(!body.contains("NameError"), "no render message: {body}");
    let scenes: Vec<&TraceRow> = rows
        .iter()
        .filter(|r| r.kind == TraceKind::ModelCall)
        .collect();
    assert_eq!(scenes.len(), 2, "{rows:#?}");
    for (i, s) in scenes.iter().enumerate() {
        assert!(s.ok);
        assert_eq!(s.route, Some(Route::Write));
        assert_eq!(s.job_id.as_deref(), Some(v.id.as_str()));
        assert_eq!(s.step, Some(i as u32 + 1));
        assert_eq!(s.prompt_version.as_deref(), Some("scene-v2"));
        assert_eq!(s.model.as_deref(), Some("test-model"));
        assert!(s.pipeline_hash.is_some());
    }
    let renders: Vec<(bool, Option<&str>)> = rows
        .iter()
        .filter(|r| r.kind == TraceKind::JobStep && r.tool.as_deref() == Some("render"))
        .map(|r| (r.ok, r.error_code.as_deref()))
        .collect();
    assert_eq!(renders, [(false, Some("scene_failed")), (true, None)]);
}
