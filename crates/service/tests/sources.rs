//! Sources and the embedding model, end to end with fakes: FakeEmbedder as the model,
//! fake extraction and chunking, and a fake Hugging Face for the download.

mod common;

use std::sync::atomic::Ordering;

use chrono::Utc;
use common::*;
use margin_service::db::{self, Store};
use margin_service::secrets::{GITHUB_CLIENT_SECRET, MemoryStore};
use margin_service::sources::ChunkRow;
use margin_shared::routes::{self, MAX_MEDIA_SOURCE_BYTES, MAX_OTHER_SOURCE_BYTES};
use margin_shared::{
    ApiError, EmbeddingModel, EmbeddingModelState, Event, Library, Proposal, ProposalStatus,
    Source, SourceKind, SourceStatus, SourceUpload, Topic,
};
use serde_json::json;

const FOURIER: &str = "Fourier series write a periodic signal as a sum of sines and cosines.\n\n\
The coefficient a_n is the projection of the signal onto cos(n x), found by integrating over one period.\n\n\
Gibbs ringing appears near jumps: the partial sums overshoot by about nine percent.";

const COOKING: &str = "Knead the dough for ten minutes.\n\nLet it rise in a warm place.";

async fn api_error(r: reqwest::Response) -> (u16, ApiError) {
    let status = r.status().as_u16();
    (status, r.json().await.unwrap())
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

async fn new_topic(h: &Harness, token: &str, parent: Option<&str>, title: &str) -> Topic {
    let r = h
        .post(token, "/api/topics")
        .json(&json!({"parent_id": parent, "title": title}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

async fn upload_ok(
    h: &Harness,
    token: &str,
    name: &str,
    topic_id: Option<&str>,
    text: &str,
) -> Source {
    let r = h.upload(token, name, topic_id, text.to_string()).await;
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

/// Waits until the library shows the source with a status `done` accepts.
async fn wait_source(
    h: &Harness,
    token: &str,
    id: &str,
    done: impl Fn(&SourceStatus) -> bool,
) -> Source {
    eventually(|| async {
        library(h, token)
            .await
            .sources
            .into_iter()
            .find(|s| s.id == id && done(&s.status))
    })
    .await
}

fn is_ready(s: &SourceStatus) -> bool {
    matches!(s, SourceStatus::Ready { .. })
}

async fn chunks(h: &Harness, source_id: &str) -> Vec<ChunkRow> {
    let mut rows: Vec<ChunkRow> = h
        .state
        .store
        .list_where(db::CHUNK, "source_id", source_id)
        .await
        .unwrap();
    rows.sort_by_key(|c| c.ord);
    rows
}

async fn embedding(h: &Harness, token: &str) -> EmbeddingModel {
    h.get(token, "/api/embedding")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn ask(h: &Harness, token: &str, prompt: &str, source_ids: &[&str]) -> reqwest::Response {
    let mut req = ask_request(prompt, 2, false);
    req.source_ids = source_ids.iter().map(|s| s.to_string()).collect();
    h.post(token, "/api/ask").json(&req).send().await.unwrap()
}

async fn wait_proposal(h: &Harness, token: &str, id: &str) -> Proposal {
    eventually(|| async {
        let p: Proposal = h
            .get(token, &format!("/api/proposals/{id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        (p.status != ProposalStatus::Drafting).then_some(p)
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn uploads_are_read_embedded_and_listed() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    let mut socket = h.socket(&t).await;

    let s = upload_ok(&h, &t, "notes/fourier.md", None, FOURIER).await;
    assert_eq!(s.status, SourceStatus::Queued);
    assert_eq!(s.kind, SourceKind::Text);
    assert_eq!(s.name, "notes/fourier.md");
    assert_eq!(s.topic_id, None);
    assert_eq!(s.bytes, FOURIER.len() as u64);
    // The file is kept under its id; the name lives in the row.
    let stored = std::fs::read(h.data_dir.join("sources").join(&s.id).join("file")).unwrap();
    assert_eq!(stored, FOURIER.as_bytes());

    let events = socket
        .until(|e| matches!(e, Event::SourceChanged { source } if is_ready(&source.status)))
        .await;
    let statuses: Vec<SourceStatus> = events
        .iter()
        .filter_map(|e| match e {
            Event::SourceChanged { source } if source.id == s.id => Some(source.status.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(statuses.first(), Some(&SourceStatus::Queued));
    assert!(
        statuses
            .iter()
            .any(|st| matches!(st, SourceStatus::Processing { .. })),
        "{statuses:?}"
    );
    assert_eq!(
        statuses.last(),
        Some(&SourceStatus::Ready {
            chunks: 3,
            pages: None,
            warning: None,
        })
    );

    let lib = library(&h, &t).await;
    assert_eq!(lib.sources.len(), 1);
    assert_eq!(
        lib.sources[0].status,
        SourceStatus::Ready {
            chunks: 3,
            pages: None,
            warning: None,
        }
    );
    let rows = chunks(&h, &s.id).await;
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1].ord, 1);
    assert!(rows[1].text.starts_with("The coefficient a_n"));
    assert_eq!(rows[1].topic_id, None);
    assert_eq!(rows[1].vector.len(), margin_embed::DIM);
    assert_eq!(h.source_tools.loads.load(Ordering::SeqCst), 1);

    // A "PDF" with two pages, filed under a topic and in a folder.
    let topic = new_topic(&h, &t, None, "Signals").await;
    let r = h
        .post(&t, "/api/sources")
        .query(&[
            ("name", "papers/dsp.pdf"),
            ("folder", "papers"),
            ("topic_id", topic.id.as_str()),
        ])
        .body("Page one text.\x0cPage two, first.\n\nPage two, second.")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let pdf: Source = r.json().await.unwrap();
    assert_eq!(pdf.kind, SourceKind::Pdf);
    assert_eq!(pdf.folder.as_deref(), Some("papers"));
    assert_eq!(pdf.topic_id.as_deref(), Some(topic.id.as_str()));
    let pdf = wait_source(&h, &t, &pdf.id, is_ready).await;
    assert_eq!(
        pdf.status,
        SourceStatus::Ready {
            // Three paragraphs and two pages as pictures.
            chunks: 5,
            pages: Some(2),
            warning: None,
        }
    );
    let rows = chunks(&h, &pdf.id).await;
    assert_eq!(rows[2].page, Some(2));
    assert_eq!(rows[2].topic_id.as_deref(), Some(topic.id.as_str()));
    // The model was loaded once and kept.
    assert_eq!(h.source_tools.loads.load(Ordering::SeqCst), 1);

    // A file that can't be read fails with the reason; one without text fails too.
    let bad = upload_ok(&h, &t, "broken.txt", None, UNREADABLE).await;
    let bad = wait_source(&h, &t, &bad.id, |s| {
        matches!(s, SourceStatus::Failed { .. })
    })
    .await;
    assert!(
        matches!(&bad.status, SourceStatus::Failed { error } if error.contains("damaged")),
        "{:?}",
        bad.status
    );
    let empty = upload_ok(&h, &t, "empty.md", None, "  \n\n ").await;
    let empty = wait_source(&h, &t, &empty.id, |s| {
        matches!(s, SourceStatus::Failed { .. })
    })
    .await;
    assert_eq!(
        empty.status,
        SourceStatus::Failed {
            error: "There's no text in this file.".into()
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unsupported_oversized_and_misfiled_uploads_are_rejected() {
    let h = start().await;
    let t = h.sign_in("Mac").await;

    let (status, err) = api_error(h.upload(&t, "archive.zip", None, "x").await).await;
    assert_eq!(status, 400);
    assert_eq!(err.code, "unsupported_source");
    assert!(err.message.contains("archive.zip"), "{}", err.message);
    let (status, err) = api_error(h.upload(&t, "Cargo.lock", None, "x").await).await;
    assert_eq!((status, err.code.as_str()), (400, "unsupported_source"));
    let (status, _) = api_error(h.upload(&t, "  ", None, "x").await).await;
    assert_eq!(status, 400);
    let (status, err) = api_error(h.upload(&t, "a.md", Some("no-such-topic"), "x").await).await;
    assert_eq!((status, err.code.as_str()), (404, "not_found"));

    // Text, code, PDFs and images: 50 MB. Refused up front by its Content-Length…
    let too_big = vec![b'a'; MAX_OTHER_SOURCE_BYTES as usize + 1];
    let (status, err) = api_error(h.upload(&t, "big.txt", None, too_big.clone()).await).await;
    assert_eq!(status, 413);
    assert_eq!(err.code, "too_large");
    assert_eq!(
        err.message,
        "big.txt is too big: text, code, PDF and image sources can be at most 50 MB."
    );
    // …or, sent without one, once more than that has arrived.
    let chunks: Vec<Result<Vec<u8>, std::io::Error>> = too_big
        .chunks(1024 * 1024)
        .map(|c| Ok(c.to_vec()))
        .collect();
    let streamed = reqwest::Body::wrap_stream(futures_util::stream::iter(chunks));
    let (status, err) = api_error(h.upload(&t, "big.pdf", None, streamed).await).await;
    assert_eq!((status, err.code.as_str()), (413, "too_large"));
    assert!(err.message.contains("50 MB"), "{}", err.message);
    // Exactly the limit is fine.
    let at_limit = vec![b'a'; MAX_OTHER_SOURCE_BYTES as usize];
    let r = h.upload(&t, "big.txt", None, at_limit).await;
    assert_eq!(r.status(), 200);
    let s: Source = r.json().await.unwrap();
    assert_eq!(s.bytes, MAX_OTHER_SOURCE_BYTES);
    // Audio and video may be ten times that.
    let r = h
        .upload(&t, "lecture.mp4", None, vec![0u8; 60 * 1024 * 1024])
        .await;
    assert_eq!(r.status(), 200);
    let video: Source = r.json().await.unwrap();
    assert_eq!(video.kind, SourceKind::Video);
    let r = h
        .post(&t, "/api/source-uploads")
        .query(&[
            ("name", "long.mov"),
            ("bytes", &(MAX_MEDIA_SOURCE_BYTES + 1).to_string()),
        ])
        .send()
        .await
        .unwrap();
    let (status, err) = api_error(r).await;
    assert_eq!((status, err.code.as_str()), (413, "too_large"));
    assert_eq!(
        err.message,
        "long.mov is too big: audio and video sources can be at most 500 MB."
    );

    let lib = library(&h, &t).await;
    assert_eq!(lib.sources.len(), 2);
    assert_eq!(
        std::fs::read_dir(h.data_dir.join("sources"))
            .unwrap()
            .count(),
        2
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn large_files_upload_in_parts() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    let topic = new_topic(&h, &t, None, "Signals").await;
    let start = |name: &str, bytes: usize| {
        h.post(&t, "/api/source-uploads")
            .query(&[
                ("name", name),
                ("folder", "talks"),
                ("topic_id", topic.id.as_str()),
                ("bytes", &bytes.to_string()),
            ])
            .send()
    };
    let r = start("talks/fourier.md", FOURIER.len()).await.unwrap();
    assert_eq!(r.status(), 200);
    let up: SourceUpload = r.json().await.unwrap();
    assert_eq!((up.bytes, up.received), (FOURIER.len() as u64, 0));
    let part = |offset: usize, data: &[u8]| {
        h.put(&t, &routes::source_upload(&up.id))
            .query(&[("offset", offset)])
            .body(data.to_vec())
            .send()
    };
    let (a, b) = FOURIER.as_bytes().split_at(100);
    let r = part(0, a).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<SourceUpload>().await.unwrap().received, 100);
    // Finishing early, or a part at the wrong place, is refused.
    let r = h
        .post(&t, &routes::source_upload_finish(&up.id))
        .send()
        .await
        .unwrap();
    assert_eq!(api_error(r).await.1.code, "incomplete_upload");
    let (status, err) = api_error(part(0, a).await.unwrap()).await;
    assert_eq!((status, err.code.as_str()), (409, "wrong_offset"));
    // A part longer than what's left of the announced size is refused, and leaves the
    // upload where it was.
    let mut long = b.to_vec();
    long.push(b'!');
    let (status, _) = api_error(part(100, &long).await.unwrap()).await;
    assert_eq!(status, 413);
    let r = h
        .get(&t, &routes::source_upload(&up.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.json::<SourceUpload>().await.unwrap().received, 100);

    assert_eq!(part(100, b).await.unwrap().status(), 200);
    let r = h
        .post(&t, &routes::source_upload_finish(&up.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let source: Source = r.json().await.unwrap();
    assert_eq!(
        (
            source.name.as_str(),
            source.folder.as_deref(),
            source.topic_id.as_deref(),
            source.bytes
        ),
        (
            "talks/fourier.md",
            Some("talks"),
            Some(topic.id.as_str()),
            FOURIER.len() as u64
        )
    );
    let ready = wait_source(&h, &t, &source.id, is_ready).await;
    assert_eq!(chunks(&h, &ready.id).await.len(), 3);
    assert_eq!(
        std::fs::read(h.data_dir.join("sources").join(&source.id).join("file")).unwrap(),
        FOURIER.as_bytes()
    );
    // The upload is gone once finished.
    let r = h
        .get(&t, &routes::source_upload(&up.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // Given up on: deleted.
    let r = start("talks/cooking.md", COOKING.len()).await.unwrap();
    let up: SourceUpload = r.json().await.unwrap();
    let r = h
        .delete(&t, &routes::source_upload(&up.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    assert_eq!(
        std::fs::read_dir(h.data_dir.join("uploads"))
            .unwrap()
            .count(),
        0
    );
    // Names are checked up front, and ids can't point outside the folder.
    let r = start("archive.zip", 3).await.unwrap();
    assert_eq!(api_error(r).await.1.code, "unsupported_source");
    let r = h.get(&t, "/api/source-uploads/..").send().await.unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_source_removes_its_file_and_chunks() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    let s = upload_ok(&h, &t, "fourier.md", None, FOURIER).await;
    wait_source(&h, &t, &s.id, is_ready).await;
    assert_eq!(chunks(&h, &s.id).await.len(), 3);
    let keep = upload_ok(&h, &t, "cooking.md", None, COOKING).await;
    wait_source(&h, &t, &keep.id, is_ready).await;

    let mut socket = h.socket(&t).await;
    let r = h
        .delete(&t, &format!("/api/sources/{}", s.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    socket
        .until(|e| matches!(e, Event::SourceDeleted { id } if id == &s.id))
        .await;
    assert!(!h.data_dir.join("sources").join(&s.id).exists());
    assert!(chunks(&h, &s.id).await.is_empty());
    let lib = library(&h, &t).await;
    assert_eq!(
        lib.sources
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>(),
        vec![keep.id.as_str()]
    );
    assert_eq!(chunks(&h, &keep.id).await.len(), 2);

    let (status, err) = api_error(
        h.delete(&t, &format!("/api/sources/{}", s.id))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!((status, err.code.as_str()), (404, "not_found"));
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_topic_deletes_its_sources() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    let subject = new_topic(&h, &t, None, "Signals").await;
    let topic = new_topic(&h, &t, Some(&subject.id), "Fourier").await;
    let other = new_topic(&h, &t, None, "Baking").await;
    let s = upload_ok(&h, &t, "fourier.md", Some(&topic.id), FOURIER).await;
    let kept = upload_ok(&h, &t, "bread.md", Some(&other.id), COOKING).await;
    wait_source(&h, &t, &s.id, is_ready).await;
    wait_source(&h, &t, &kept.id, is_ready).await;

    let mut socket = h.socket(&t).await;
    let r = h
        .delete(&t, &format!("/api/topics/{}", subject.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    socket
        .until(|e| matches!(e, Event::SourceDeleted { id } if id == &s.id))
        .await;
    assert!(!h.data_dir.join("sources").join(&s.id).exists());
    assert!(chunks(&h, &s.id).await.is_empty());
    let lib = library(&h, &t).await;
    assert_eq!(lib.sources.len(), 1);
    assert_eq!(lib.sources[0].id, kept.id);
    assert!(h.data_dir.join("sources").join(&kept.id).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn outlines_and_lessons_use_the_sources_and_accepting_attaches_them() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    h.configure_ai(&t).await;
    let fourier = upload_ok(&h, &t, "notes/fourier.md", None, FOURIER).await;
    let pdf = upload_ok(
        &h,
        &t,
        "dsp.pdf",
        None,
        "Intro page.\x0cThe Fourier coefficient formula for a periodic signal.",
    )
    .await;
    let unrelated = upload_ok(&h, &t, "bread.md", None, COOKING).await;

    // Sources must exist and not belong to another topic.
    let (status, err) = api_error(ask(&h, &t, "Fourier", &["nope"]).await).await;
    assert_eq!((status, err.code.as_str()), (400, "unknown_source"));
    let elsewhere = new_topic(&h, &t, None, "Elsewhere").await;
    let filed = upload_ok(&h, &t, "other.md", Some(&elsewhere.id), "Other text.").await;
    let (status, err) = api_error(ask(&h, &t, "Fourier", &[&filed.id]).await).await;
    assert_eq!((status, err.code.as_str()), (400, "unknown_source"));
    assert!(err.message.contains("other.md"), "{}", err.message);

    // Asked right away: the outline waits for the sources to be read, then uses them.
    let r = ask(
        &h,
        &t,
        "Explain the Fourier coefficient of a periodic signal",
        &[&fourier.id, &pdf.id, &fourier.id],
    )
    .await;
    assert_eq!(r.status(), 200);
    let p: Proposal = r.json().await.unwrap();
    assert_eq!(
        p.request.source_ids,
        vec![fourier.id.clone(), pdf.id.clone()]
    );
    let p = wait_proposal(&h, &t, &p.id).await;
    assert_eq!(p.status, ProposalStatus::Ready);
    let passages = h
        .generator
        .outline_passages
        .lock()
        .unwrap()
        .last()
        .cloned()
        .unwrap();
    // Every chunk of the two named sources (5 of them) and both pages of the PDF as
    // pictures, best first, none of the third.
    assert_eq!(passages.len(), 7, "{passages:?}");
    let pictures: Vec<_> = passages.iter().filter(|p| p.image.is_some()).collect();
    assert_eq!(pictures.len(), 2);
    assert!(
        pictures
            .iter()
            .all(|p| p.source == "dsp.pdf" && p.text.is_empty())
    );
    let mut pages: Vec<_> = pictures.iter().map(|p| p.page.unwrap()).collect();
    pages.sort();
    assert_eq!(pages, [1, 2]);
    assert!(passages.iter().any(|p| p.source == "notes/fourier.md"
        && p.page.is_none()
        && p.text.starts_with("The coefficient a_n is the projection")));
    assert!(passages.iter().any(|p| p.source == "dsp.pdf"
        && p.page == Some(2)
        && p.text == "The Fourier coefficient formula for a periodic signal."));
    assert!(!passages.iter().any(|p| p.source == "bread.md"));
    assert_eq!(
        passages.iter().rfind(|p| p.image.is_none()).unwrap().text,
        "Intro page."
    );

    // Accepting attaches the Ask's sources (and their chunks) to the new topic.
    let mut socket = h.socket(&t).await;
    let r = h
        .post(&t, &format!("/api/proposals/{}/accept", p.id))
        .json(&json!({"outline": p.outline.clone().unwrap()}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let topic: Topic = r.json().await.unwrap();
    let events = socket
        .until(|e| matches!(e, Event::ProposalChanged { .. }))
        .await;
    let attached: Vec<&Source> = events
        .iter()
        .filter_map(|e| match e {
            Event::SourceChanged { source } => Some(source),
            _ => None,
        })
        .collect();
    assert_eq!(attached.len(), 2);
    assert!(
        attached
            .iter()
            .all(|s| s.topic_id.as_deref() == Some(topic.id.as_str()))
    );
    let lib = library(&h, &t).await;
    let topic_of = |id: &str| {
        lib.sources
            .iter()
            .find(|s| s.id == id)
            .unwrap()
            .topic_id
            .clone()
    };
    assert_eq!(topic_of(&fourier.id), Some(topic.id.clone()));
    assert_eq!(topic_of(&pdf.id), Some(topic.id.clone()));
    assert_eq!(topic_of(&unrelated.id), None);
    assert!(
        chunks(&h, &fourier.id)
            .await
            .iter()
            .all(|c| c.topic_id.as_deref() == Some(topic.id.as_str()))
    );

    // Each lesson is written with passages from the topic's sources.
    let lessons = eventually(|| async {
        let lib = library(&h, &t).await;
        let ls: Vec<_> = lib
            .lessons
            .into_iter()
            .filter(|l| l.topic_id == topic.id)
            .collect();
        ls.iter()
            .all(|l| l.status == margin_shared::LessonStatus::Ready)
            .then_some(ls)
    })
    .await;
    assert_eq!(lessons.len(), 2);
    {
        let requests = h.generator.lesson_requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for ctx in requests.iter() {
            assert_eq!(ctx.passages.len(), 7);
            assert!(ctx.passages.iter().any(|p| p.source == "dsp.pdf"));
            assert!(!ctx.passages.iter().any(|p| p.source == "bread.md"));
        }
    }

    // A source added to the topic later is used when a lesson is regenerated.
    upload_ok(
        &h,
        &t,
        "extra.md",
        Some(&topic.id),
        "Fourier extra insight.",
    )
    .await;
    let r = h
        .post(&t, &format!("/api/lessons/{}/regenerate", lessons[0].id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let ctx = eventually(|| async {
        let requests = h.generator.lesson_requests.lock().unwrap();
        (requests.len() == 3).then(|| requests[2].clone())
    })
    .await;
    assert!(
        ctx.passages
            .iter()
            .any(|p| p.source == "extra.md" && p.text == "Fourier extra insight."),
        "{:?}",
        ctx.passages
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sources_wait_for_the_model_download() {
    let h = start_with(Setup {
        model_downloaded: false,
        ..Setup::default()
    })
    .await;
    let t = h.sign_in("Mac").await;
    h.configure_ai(&t).await;
    assert_eq!(
        embedding(&h, &t).await.state,
        EmbeddingModelState::NotDownloaded {
            download_bytes: MODEL_BYTES
        }
    );

    let s = upload_ok(&h, &t, "fourier.md", None, FOURIER).await;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    // Nothing is read without the model, and asking with sources is refused rather
    // than writing an outline that silently ignores them.
    let lib = library(&h, &t).await;
    assert_eq!(lib.sources[0].status, SourceStatus::Queued);
    assert_eq!(h.source_tools.extracts.load(Ordering::SeqCst), 0);
    let r = ask(&h, &t, "Fourier coefficients", &[&s.id]).await;
    assert_eq!(r.status(), 409);
    let err: margin_shared::ApiError = r.json().await.unwrap();
    assert_eq!(err.code, "no_embedding_model");

    let r = h.post(&t, "/api/embedding/download").send().await.unwrap();
    assert_eq!(r.status(), 200);
    let r = ask(&h, &t, "Fourier coefficients", &[&s.id]).await;
    assert_eq!(r.status(), 200);
    let p: Proposal = r.json().await.unwrap();
    let s = wait_source(&h, &t, &s.id, is_ready).await;
    assert_eq!(
        s.status,
        SourceStatus::Ready {
            chunks: 3,
            pages: None,
            warning: None,
        }
    );
    let p = wait_proposal(&h, &t, &p.id).await;
    assert_eq!(p.status, ProposalStatus::Ready);
    let passages = h
        .generator
        .outline_passages
        .lock()
        .unwrap()
        .last()
        .cloned()
        .unwrap();
    assert_eq!(passages.len(), 3);
    assert_eq!(h.source_tools.loads.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_model_downloads_with_progress_and_can_be_deleted() {
    let h = start_with(Setup {
        model_downloaded: false,
        ..Setup::default()
    })
    .await;
    let t = h.sign_in("Mac").await;
    h.hf.part_delay_ms.store(150, Ordering::SeqCst);
    let model_dir = h.data_dir.join("models").join("embeddinggemma-2");

    let m = embedding(&h, &t).await;
    assert_eq!(m.repo, "FluidInference/embeddinggemma-2-coreml");
    assert_eq!(
        m.state,
        EmbeddingModelState::NotDownloaded {
            download_bytes: MODEL_BYTES
        }
    );
    // The size is asked of Hugging Face once.
    embedding(&h, &t).await;
    assert_eq!(h.hf.size_requests.load(Ordering::SeqCst), 1);

    let mut socket = h.socket(&t).await;
    let m: EmbeddingModel = h
        .post(&t, "/api/embedding/download")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        m.state,
        EmbeddingModelState::Downloading {
            received: 0,
            total: MODEL_BYTES
        }
    );
    // Asking again while it downloads changes nothing; deleting is refused.
    let again: EmbeddingModel = h
        .post(&t, "/api/embedding/download")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(matches!(
        again.state,
        EmbeddingModelState::Downloading { .. }
    ));
    let (status, err) = api_error(h.delete(&t, "/api/embedding").send().await.unwrap()).await;
    assert_eq!((status, err.code.as_str()), (409, "model_downloading"));

    let events = socket
        .until(|e| {
            matches!(e, Event::EmbeddingModelChanged { model } if matches!(model.state, EmbeddingModelState::Ready { .. }))
        })
        .await;
    let states: Vec<&EmbeddingModelState> = events
        .iter()
        .filter_map(|e| match e {
            Event::EmbeddingModelChanged { model } => Some(&model.state),
            _ => None,
        })
        .collect();
    assert_eq!(
        states[0],
        &EmbeddingModelState::Downloading {
            received: 0,
            total: MODEL_BYTES
        }
    );
    let partial = states
        .iter()
        .filter(|s| {
            matches!(s, EmbeddingModelState::Downloading { received, total }
                if *received > 0 && received < total && *total == MODEL_BYTES)
        })
        .count();
    // About 1.2 s of download, sent at most twice a second.
    assert!((1..=3).contains(&partial), "{states:?}");
    assert_eq!(
        states.last().unwrap(),
        &&EmbeddingModelState::Ready { bytes: MODEL_BYTES }
    );
    assert_eq!(h.hf.downloads.load(Ordering::SeqCst), 1);
    assert!(model_dir.join(MODEL_FILE).is_file());
    assert_eq!(
        embedding(&h, &t).await.state,
        EmbeddingModelState::Ready { bytes: MODEL_BYTES }
    );
    // Downloaded already: nothing to do.
    h.post(&t, "/api/embedding/download").send().await.unwrap();
    assert_eq!(h.hf.downloads.load(Ordering::SeqCst), 1);

    // Using it loads it; deleting removes the files and unloads it.
    let s = upload_ok(&h, &t, "a.md", None, "Some text.").await;
    wait_source(&h, &t, &s.id, is_ready).await;
    assert!(h.state.embedder.lock().await.is_some());
    let m: EmbeddingModel = h
        .delete(&t, "/api/embedding")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        m.state,
        EmbeddingModelState::NotDownloaded {
            download_bytes: MODEL_BYTES
        }
    );
    assert!(!model_dir.exists());
    assert!(h.state.embedder.lock().await.is_none());

    // A failed download says why, and can be retried.
    h.hf.part_delay_ms.store(0, Ordering::SeqCst);
    h.hf.fail.store(true, Ordering::SeqCst);
    h.post(&t, "/api/embedding/download").send().await.unwrap();
    let failed = eventually(|| async {
        let m = embedding(&h, &t).await;
        matches!(m.state, EmbeddingModelState::Failed { .. }).then_some(m)
    })
    .await;
    assert!(
        matches!(&failed.state, EmbeddingModelState::Failed { error } if error.contains("500")),
        "{failed:?}"
    );
    h.hf.fail.store(false, Ordering::SeqCst);
    h.post(&t, "/api/embedding/download").send().await.unwrap();
    eventually(|| async {
        matches!(
            embedding(&h, &t).await.state,
            EmbeddingModelState::Ready { .. }
        )
        .then_some(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_rereads_interrupted_sources_and_drops_old_unattached_ones() {
    let store = Store::memory().await.unwrap();
    let now = Utc::now();
    let topic = Topic {
        id: "t1".into(),
        parent_id: None,
        title: "Signals".into(),
        position: 0,
        created_at: now,
        updated_at: now,
    };
    store.put(db::TOPIC, "t1", &topic).await.unwrap();
    let source = |id: &str, topic_id: Option<&str>, status: SourceStatus, age_h: i64| Source {
        media_url: None,
        id: id.into(),
        topic_id: topic_id.map(Into::into),
        name: format!("{id}.md"),
        folder: None,
        kind: SourceKind::Text,
        bytes: FOURIER.len() as u64,
        status,
        created_at: now - chrono::Duration::hours(age_h),
    };
    let rows = [
        source(
            "s-proc",
            Some("t1"),
            SourceStatus::Processing { done: 1, total: 3 },
            48,
        ),
        source("s-queued", None, SourceStatus::Queued, 1),
        source(
            "s-old",
            None,
            SourceStatus::Ready {
                chunks: 3,
                pages: None,
                warning: None,
            },
            25,
        ),
    ];
    for s in &rows {
        store.put(db::SOURCE, &s.id, s).await.unwrap();
    }
    // A chunk left by the interrupted run.
    let leftover = ChunkRow {
        source_id: "s-proc".into(),
        topic_id: Some("t1".into()),
        ord: 0,
        page: None,
        start_ms: None,
        end_ms: None,
        text: "stale".into(),
        vector: vec![0.0; margin_embed::DIM],
    };
    store.put(db::CHUNK, "s-proc-0", &leftover).await.unwrap();
    store.put(db::CHUNK, "s-old-0", &leftover).await.unwrap();

    let files = ["s-proc", "s-queued", "s-old"]
        .iter()
        .map(|id| (format!("sources/{id}/file"), FOURIER.as_bytes().to_vec()))
        .collect();
    let h = start_with(Setup {
        store: Some(store),
        secrets: MemoryStore::new().with(GITHUB_CLIENT_SECRET, CLIENT_SECRET),
        files,
        ..Setup::default()
    })
    .await;
    let t = h.sign_in("Mac").await;

    let proc = wait_source(&h, &t, "s-proc", is_ready).await;
    assert_eq!(
        proc.status,
        SourceStatus::Ready {
            chunks: 3,
            pages: None,
            warning: None,
        }
    );
    let rows = chunks(&h, "s-proc").await;
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|c| c.text != "stale"));
    wait_source(&h, &t, "s-queued", is_ready).await;

    let lib = library(&h, &t).await;
    assert!(!lib.sources.iter().any(|s| s.id == "s-old"));
    assert!(!h.data_dir.join("sources").join("s-old").exists());
    assert!(chunks(&h, "s-old").await.is_empty());
}
