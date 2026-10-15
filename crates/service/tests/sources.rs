//! Sources and the embedding model, end to end with fakes: FakeEmbedder as the model,
//! fake extraction and chunking, and a fake Hugging Face for the download.

mod common;

use std::sync::atomic::Ordering;

use chrono::Utc;
use common::*;
use margin_service::db::PassageRow;
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

/// A source's text passages, in order.
async fn chunks(h: &Harness, source_id: &str) -> Vec<PassageRow> {
    h.state
        .store
        .passages_of(source_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|p| p.modality == "text")
        .collect()
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

/// Has the agent propose a two-lesson course drawing on the sources; `Err` is the
/// tool's error.
async fn propose(
    h: &Harness,
    token: &str,
    prompt: &str,
    source_ids: &[&str],
) -> Result<Proposal, String> {
    let mut req = ask_request(prompt, 2, false);
    req.source_ids = source_ids.iter().map(|s| s.to_string()).collect();
    h.propose_in_chat(token, &req).await.map(|(p, _)| p)
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
    assert!(rows[1].scope_ids.is_empty());
    assert_eq!(rows[1].document_id, s.id);
    assert_eq!(rows[1].embedder, margin_service::ingest::EMBEDDER_VERSION);
    assert_eq!(rows[1].access, ["company"]);
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
    assert_eq!(rows[2].scope_ids, std::slice::from_ref(&topic.id));
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
    // Every hit is packed whatever its score: the test checks where passages come from.
    let r = h
        .patch(&t, "/api/settings")
        .json(&json!({"pipeline": {"budget": {"min_score_pct": 0}}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
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
    let err = propose(&h, &t, "Fourier", &["nope"]).await.unwrap_err();
    assert_eq!(
        err,
        "One of the sources isn't on the service any more. Add it again."
    );
    let elsewhere = new_topic(&h, &t, None, "Elsewhere").await;
    let filed = upload_ok(&h, &t, "other.md", Some(&elsewhere.id), "Other text.").await;
    let err = propose(&h, &t, "Fourier", &[&filed.id]).await.unwrap_err();
    assert!(err.contains("other.md"), "{err}");

    // Proposed right away (duplicates dropped); the lessons wait for the sources to be
    // read.
    let p = propose(
        &h,
        &t,
        "Explain the Fourier coefficient of a periodic signal",
        &[&fourier.id, &pdf.id, &fourier.id],
    )
    .await
    .unwrap();
    assert_eq!(
        p.request.source_ids,
        vec![fourier.id.clone(), pdf.id.clone()]
    );
    assert_eq!(p.status, ProposalStatus::Ready);

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
            .all(|c| c.scope_ids.contains(&topic.id))
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
            // Seven chunks match; adjacent ones of a document come merged.
            assert!((1..=7).contains(&ctx.passages.len()), "{:?}", ctx.passages);
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
    let err = propose(&h, &t, "Fourier coefficients", &[&s.id])
        .await
        .unwrap_err();
    assert_eq!(
        err,
        "Sources need the local embedding model. Download it in Settings → Sources."
    );

    let r = h.post(&t, "/api/embedding/download").send().await.unwrap();
    assert_eq!(r.status(), 200);
    let p = propose(&h, &t, "Fourier coefficients", &[&s.id])
        .await
        .unwrap();
    assert_eq!(p.status, ProposalStatus::Ready);
    let s = wait_source(&h, &t, &s.id, is_ready).await;
    assert_eq!(
        s.status,
        SourceStatus::Ready {
            chunks: 3,
            pages: None,
            warning: None,
        }
    );
    // Its lessons are written with the source's passages once it's read.
    let r = h
        .post(&t, &format!("/api/proposals/{}/accept", p.id))
        .json(&json!({"outline": p.outline.clone().unwrap()}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let passages = eventually(|| async {
        let requests = h.generator.lesson_requests.lock().unwrap();
        requests.first().map(|ctx| ctx.passages.clone())
    })
    .await;
    // All three chunks, adjacent ones merged into one passage.
    let sent: String = passages.iter().map(|p| p.text.as_str()).collect();
    for row in h.state.store.passages_of(&s.id).await.unwrap() {
        assert!(sent.contains(row.text.trim()), "{} not in {sent}", row.text);
    }
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
    let model_dir = h.data_dir.join("models").join("embeddinggemma-2-onnx-q8");

    let m = embedding(&h, &t).await;
    assert_eq!(m.repo, "onnx-community/embeddinggemma-2-ONNX");
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
        access: Vec::new(),
        deny: Vec::new(),
        owner: None,
        id: "t1".into(),
        parent_id: None,
        title: "Signals".into(),
        position: 0,
        created_at: now,
        updated_at: now,
    };
    store.put(db::TOPIC, "t1", &topic).await.unwrap();
    let source = |id: &str, topic_id: Option<&str>, status: SourceStatus, age_h: i64| Source {
        access: Vec::new(),
        deny: Vec::new(),
        owner: None,
        media_url: None,
        id: id.into(),
        topic_id: topic_id.map(Into::into),
        name: format!("{id}.md"),
        folder: None,
        kind: SourceKind::Text,
        bytes: FOURIER.len() as u64,
        status,
        created_at: now - chrono::Duration::hours(age_h),
        workspace_id: None,
        modified: None,
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

// ---------------------------------------------------------------------------
// R1: the passage index (ingest, documents, migration)
// ---------------------------------------------------------------------------

use margin_embed::Embedder as _;
use margin_shared::Note;
use margin_shared::context::{
    ContextIngest, Document, DocumentStatus, LibraryKind, Origin, Project,
};

const SECTIONS: &str = "# Fourier\n\nFourier series write a periodic signal as a sum of sines.\n\n\
The coefficient a_n is the projection onto cos(n x).\n\n\
# Gibbs\n\nPartial sums overshoot near jumps by about nine percent.";

async fn document(h: &Harness, id: &str) -> Document {
    margin_service::ingest::document(&h.state, id)
        .await
        .unwrap()
        .expect("the document exists")
}

#[tokio::test(flavor = "multi_thread")]
async fn sources_become_documents_with_located_passages_in_the_default_project() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    let topic = new_topic(&h, &t, None, "Signals").await;
    let mut socket = h.socket(&t).await;
    let s = upload_ok(&h, &t, "waves.md", Some(&topic.id), SECTIONS).await;
    let events = socket
        .until(|e| matches!(e, Event::DocumentChanged { document } if document.id == s.id))
        .await;
    let Some(Event::DocumentChanged { document: doc }) = events.last() else {
        unreachable!()
    };
    assert_eq!(doc.status, DocumentStatus::Ready { passages: 3 });
    assert_eq!(
        doc.origin,
        Origin::Source {
            source_id: s.id.clone()
        }
    );
    assert_eq!(doc.actor, "owner");
    assert_eq!(doc.access, ["company"]);
    assert!(doc.generation > 0);
    // One project, "My library", holds it.
    let projects: Vec<Project> = h.state.store.list(db::PROJECT).await.unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].name, "My library");
    assert_eq!(doc.project_id, projects[0].id);
    // The source is Ready like before; the pipeline's chunker was asked for by name.
    wait_source(&h, &t, &s.id, is_ready).await;
    assert!(
        h.source_tools
            .chunkers
            .lock()
            .unwrap()
            .iter()
            .all(|c| c == "sections-v2")
    );

    let rows = h.state.store.passages_of(&s.id).await.unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].section_path, "Fourier");
    assert_eq!((rows[0].start_line, rows[0].end_line), (Some(3), Some(3)));
    assert_eq!(rows[2].section_path, "Gibbs");
    assert_eq!(rows[2].start_line, Some(9));
    assert!(rows.iter().all(|r| r.project_id == projects[0].id
        && r.scope_ids == [topic.id.clone()]
        && r.access == ["company"]
        && r.origin == "source"
        && r.chunk_hash.len() == 64
        && r.id == format!("{}:{}", s.id, r.chunk_hash)
        && r.unit_id.starts_with(&format!("{}:text:", s.id))));
    assert_eq!(rows.iter().map(|r| r.ord).collect::<Vec<_>>(), [0, 1, 2]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_embedder_re_embeds_every_document_once_keeping_its_passages() {
    use margin_service::ingest::{EMBEDDER_MIGRATION_PREFIX, embedder_migration, reembed};
    use margin_service::state::SETTINGS_ID;
    let h = start().await;
    let t = h.sign_in("Mac").await;
    let s = upload_ok(&h, &t, "waves.md", None, SECTIONS).await;
    wait_source(&h, &t, &s.id, is_ready).await;
    let before = h.state.store.passages_of(&s.id).await.unwrap();
    // The worker's first start recorded the configured embedder.
    let marker = embedder_migration(h.state.model_files.backend.embedder_version());
    let settings = h.state.settings_row().await.unwrap();
    assert!(
        settings.migrations.contains(&marker),
        "{:?}",
        settings.migrations
    );

    // As if written by another backend: other vectors, another version, and the pass
    // not yet run for this one.
    let older: Vec<PassageRow> = before
        .iter()
        .map(|r| PassageRow {
            embedder: "older-embedder".into(),
            vector: vec![0.5; r.vector.len()],
            ..r.clone()
        })
        .collect();
    h.state.store.rewrite_vectors(&older).await.unwrap();
    let mut row = h.state.settings_row().await.unwrap();
    row.migrations
        .retain(|m| !m.starts_with(EMBEDDER_MIGRATION_PREFIX));
    row.migrations.push(embedder_migration("older-embedder"));
    h.state
        .store
        .put(db::SETTINGS, SETTINGS_ID, &row)
        .await
        .unwrap();
    assert!(h.state.store.passages_of(&s.id).await.unwrap()[0].embedder == "older-embedder");

    let mut socket = h.socket(&t).await;
    reembed(&h.state).await.unwrap();
    // Settings → Sources shows the pass while it runs, and nothing once it's done.
    let events = socket
        .until(|e| matches!(e, Event::EmbeddingModelChanged { model } if model.reindex.is_none()))
        .await;
    let progress: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::EmbeddingModelChanged { model } => Some(model.reindex),
            _ => None,
        })
        .collect();
    assert_eq!(
        progress,
        [
            Some(margin_shared::ReindexProgress { done: 0, total: 1 }),
            None
        ]
    );
    assert_eq!(embedding(&h, &t).await.reindex, None);
    let after = h.state.store.passages_of(&s.id).await.unwrap();
    // Same rows (ids, texts, places), the current model's vectors and version again.
    assert_eq!(after, before);
    let migrations = h.state.settings_row().await.unwrap().migrations;
    assert!(migrations.contains(&marker));
    assert!(!migrations.contains(&embedder_migration("older-embedder")));
    assert_eq!(
        document(&h, &s.id).await.status,
        DocumentStatus::Ready { passages: 3 }
    );

    // Done once: a second call reads nothing.
    let extracts = h.source_tools.extracts.load(Ordering::SeqCst);
    reembed(&h.state).await.unwrap();
    assert_eq!(h.source_tools.extracts.load(Ordering::SeqCst), extracts);
}

#[tokio::test(flavor = "multi_thread")]
async fn reading_again_embeds_only_what_changed_and_moves_the_generation() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    let s = upload_ok(&h, &t, "waves.md", None, SECTIONS).await;
    wait_source(&h, &t, &s.id, is_ready).await;
    let before = h.state.store.passages_of(&s.id).await.unwrap();
    let gen_before = document(&h, &s.id).await.generation;
    let since = margin_service::ingest::changes_since(&h.state, 0)
        .await
        .unwrap()
        .generation;

    // Unchanged: nothing embedded, nothing moved, the generation stays.
    let report = margin_service::ingest::ingest(&h.state, &s.id)
        .await
        .unwrap();
    assert_eq!(
        (report.chunks_new, report.chunks_kept, report.chunks_deleted),
        (0, 3, 0)
    );
    assert_eq!(document(&h, &s.id).await.generation, gen_before);

    // One section edited in place, a line longer, above the last section.
    let edited = SECTIONS.replace(
        "The coefficient a_n is the projection onto cos(n x).",
        "The coefficient b_n is the projection onto sin(n x),\nfound over one period.",
    );
    std::fs::write(h.data_dir.join("sources").join(&s.id).join("file"), &edited).unwrap();
    let mut socket = h.socket(&t).await;
    let report = margin_service::ingest::ingest(&h.state, &s.id)
        .await
        .unwrap();
    assert_eq!(
        (report.chunks_new, report.chunks_kept, report.chunks_deleted),
        (1, 2, 1)
    );
    let after = h.state.store.passages_of(&s.id).await.unwrap();
    assert_eq!(after.len(), 3);
    // Kept passages keep their ids and vectors; the moved one has its new lines.
    assert_eq!(after[0], before[0]);
    assert_eq!(after[2].id, before[2].id);
    assert_eq!(after[2].vector, before[2].vector);
    assert_eq!(after[2].start_line, Some(10));
    assert!(after[1].text.starts_with("The coefficient b_n"));
    assert_ne!(after[1].id, before[1].id);
    let doc = document(&h, &s.id).await;
    assert!(doc.generation > gen_before);
    assert_eq!(doc.status, DocumentStatus::Ready { passages: 3 });
    // DocumentChanged once, and the change feed lists it.
    let events = socket
        .until(|e| matches!(e, Event::DocumentChanged { .. }))
        .await;
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::DocumentChanged { .. }))
            .count(),
        1
    );
    let changes = margin_service::ingest::changes_since(&h.state, since)
        .await
        .unwrap();
    assert_eq!(
        changes
            .changed
            .iter()
            .map(|d| d.id.as_str())
            .collect::<Vec<_>>(),
        [s.id.as_str()]
    );
    assert_eq!(changes.generation, doc.generation);
    assert!(changes.deleted.is_empty());
}

/// A writer bumps the generation counter and puts the document in a second statement.
/// The change feed must not hand out a cursor past a generation whose row isn't written
/// yet, or the reader skips that document for good.
#[tokio::test(flavor = "multi_thread")]
async fn the_change_feed_never_passes_a_generation_still_being_written() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    let s = upload_ok(&h, &t, "waves.md", None, SECTIONS).await;
    wait_source(&h, &t, &s.id, is_ready).await;
    let since = margin_service::ingest::changes_since(&h.state, 0)
        .await
        .unwrap()
        .generation;

    // A writer mid-way: the counter has moved, the document isn't put yet.
    let w = h.state.write_lock.lock().await;
    let mut doc = document(&h, &s.id).await;
    doc.generation = h.state.store.next_generation().await.unwrap();
    let state = h.state.clone();
    let feed = tokio::spawn(async move {
        margin_service::ingest::changes_since(&state, since)
            .await
            .unwrap()
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    h.state.store.put(db::DOCUMENT, &s.id, &doc).await.unwrap();
    drop(w);

    let changes = feed.await.unwrap();
    assert_eq!(changes.generation, doc.generation);
    assert_eq!(
        changes
            .changed
            .iter()
            .map(|d| d.id.as_str())
            .collect::<Vec<_>>(),
        [s.id.as_str()]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_documents_access_is_copied_onto_its_passages() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    let s = upload_ok(&h, &t, "waves.md", None, SECTIONS).await;
    wait_source(&h, &t, &s.id, is_ready).await;
    let mut doc = document(&h, &s.id).await;
    doc.access = vec!["company/engineering".into(), "company/research".into()];
    h.state.store.put(db::DOCUMENT, &s.id, &doc).await.unwrap();
    let report = margin_service::ingest::ingest(&h.state, &s.id)
        .await
        .unwrap();
    // Nothing is embedded again; every passage carries the new paths.
    assert_eq!(report.chunks_new, 0);
    let rows = h.state.store.passages_of(&s.id).await.unwrap();
    assert!(rows.iter().all(|r| r.access == doc.access));
    // A query filtered by access finds them through the index.
    let hits: Vec<serde_json::Value> = h
        .state
        .store
        .query_rows(
            "SELECT id FROM passage WHERE access CONTAINSANY $paths",
            vec![("paths", json!(["company/research"]))],
        )
        .await
        .unwrap();
    assert_eq!(hits.len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_and_attaching_sources_follow_through_to_documents() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    let s = upload_ok(&h, &t, "fourier.md", None, FOURIER).await;
    wait_source(&h, &t, &s.id, is_ready).await;
    let since = margin_service::ingest::changes_since(&h.state, 0)
        .await
        .unwrap()
        .generation;
    // Filed under a topic: its passages are searchable from the topic.
    let topic = new_topic(&h, &t, None, "Signals").await;
    let events = {
        let _w = h.state.write_lock.lock().await;
        margin_service::sources::attach_locked(&h.state, std::slice::from_ref(&s.id), &topic)
            .await
            .unwrap()
    };
    assert!(
        events.iter().any(
            |e| matches!(&e.event, Event::DocumentChanged { document } if document.id == s.id)
        )
    );
    assert!(
        chunks(&h, &s.id)
            .await
            .iter()
            .all(|c| c.scope_ids == [topic.id.clone()])
    );
    assert!(document(&h, &s.id).await.generation > since);

    let r = h
        .delete(&t, &format!("/api/sources/{}", s.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    assert!(
        margin_service::ingest::document(&h.state, &s.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(h.state.store.passages_of(&s.id).await.unwrap().is_empty());
    let changes = margin_service::ingest::changes_since(&h.state, since)
        .await
        .unwrap();
    assert_eq!(changes.deleted, std::slice::from_ref(&s.id));
    assert!(changes.changed.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn pictures_and_pages_share_units_and_pages_render_when_needed() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    let pdf = upload_ok(
        &h,
        &t,
        "dsp.pdf",
        None,
        "Aliasing folds high frequencies.\x0cNyquist rate is twice the bandwidth.",
    )
    .await;
    wait_source(&h, &t, &pdf.id, is_ready).await;
    let rows = h.state.store.passages_of(&pdf.id).await.unwrap();
    let page2: Vec<&PassageRow> = rows.iter().filter(|r| r.page == Some(2)).collect();
    assert_eq!(page2.len(), 2);
    assert!(
        page2
            .iter()
            .all(|r| r.unit_id == format!("{}:page:2", pdf.id))
    );
    let picture = page2.iter().find(|r| r.modality == "picture").unwrap();
    // The page picture carries the page's text.
    assert_eq!(picture.text, "Nyquist rate is twice the bandwidth.");
    assert_eq!(picture.file.as_deref(), Some("page-2.png"));

    // A page picture that isn't on disk is rendered when retrieval needs it.
    let page = h
        .data_dir
        .join("sources")
        .join(&pdf.id)
        .join("media")
        .join("page-2.png");
    std::fs::remove_file(&page).unwrap();
    let found = margin_service::sources::passages(
        &h.state,
        std::slice::from_ref(&pdf.id),
        "Nyquist bandwidth",
    )
    .await;
    assert!(!found.is_empty());
    assert!(page.is_file());
    assert!(h.source_tools.page_renders.load(Ordering::SeqCst) > 0);
    assert!(
        found
            .iter()
            .all(|p| p.citation.document_id.as_deref() == Some(pdf.id.as_str()))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn agents_documents_and_library_items_are_indexed_too() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    // An agent's text document.
    let req = ContextIngest {
        name: "runbook.md".into(),
        origin: Origin::Agent {
            name: "test".into(),
        },
        kind: SourceKind::Text,
        text: "Restart the queue worker.\n\nThen drain the dead letters.".into(),
    };
    let doc = margin_service::ingest::ingest_text(&h.state, &req)
        .await
        .unwrap();
    assert_eq!(doc.actor, "agent:test");
    let ready = eventually(|| async {
        let d = document(&h, &doc.id).await;
        matches!(d.status, DocumentStatus::Ready { .. }).then_some(d)
    })
    .await;
    assert_eq!(ready.status, DocumentStatus::Ready { passages: 2 });
    let rows = h.state.store.passages_of(&doc.id).await.unwrap();
    assert!(
        rows.iter()
            .all(|r| r.origin == "agent" && r.scope_ids.is_empty())
    );
    // The same agent and name updates that document rather than adding one.
    let again = margin_service::ingest::ingest_text(
        &h.state,
        &ContextIngest {
            text: "Restart the queue worker.".into(),
            ..req.clone()
        },
    )
    .await
    .unwrap();
    assert_eq!(again.id, doc.id);
    eventually(|| async {
        let d = document(&h, &doc.id).await;
        (d.status == DocumentStatus::Ready { passages: 1 }).then_some(())
    })
    .await;
    // Only agents add text.
    assert!(
        margin_service::ingest::ingest_text(
            &h.state,
            &ContextIngest {
                origin: Origin::Source {
                    source_id: "x".into()
                },
                ..req.clone()
            }
        )
        .await
        .is_err()
    );

    // A note, searchable from its topic, and gone with it.
    let topic = new_topic(&h, &t, None, "Ops").await;
    let note: Note = h
        .post(&t, "/api/notes")
        .json(
            &json!({"topic_id": topic.id, "lesson_id": null, "title": "Pager",
            "body_md": "Escalate after fifteen minutes.", "quote": null}),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ndoc = margin_service::ingest::index_library(&h.state, LibraryKind::Note, &note.id)
        .await
        .unwrap();
    eventually(|| async {
        let d = document(&h, &ndoc.id).await;
        matches!(d.status, DocumentStatus::Ready { .. }).then_some(())
    })
    .await;
    let rows = h.state.store.passages_of(&ndoc.id).await.unwrap();
    assert!(!rows.is_empty());
    assert!(
        rows.iter()
            .all(|r| r.origin == "library" && r.scope_ids.is_empty())
    );
    let r = h
        .delete(&t, &format!("/api/notes/{}", note.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    assert!(
        margin_service::ingest::document(&h.state, &ndoc.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        h.state
            .store
            .passages_of(&ndoc.id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn folder_files_are_folder_documents_searchable_from_their_folder() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    let now = Utc::now();
    let folder_file = Source {
        access: Vec::new(),
        deny: Vec::new(),
        owner: None,
        media_url: None,
        id: "f1".into(),
        topic_id: None,
        name: "notes/waves.md".into(),
        folder: Some("Notes".into()),
        kind: SourceKind::Text,
        bytes: SECTIONS.len() as u64,
        status: SourceStatus::Queued,
        created_at: now,
        workspace_id: Some("w1".into()),
        modified: Some(now),
    };
    let dir = h.data_dir.join("sources").join("f1");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("file"), SECTIONS).unwrap();
    h.state
        .store
        .put(db::SOURCE, "f1", &folder_file)
        .await
        .unwrap();
    margin_service::ingest::queue_source(&h.state, "f1")
        .await
        .unwrap();
    // A folder's files aren't in the library; read the row.
    eventually(|| async {
        let s: Source = h.state.store.get(db::SOURCE, "f1").await.unwrap()?;
        matches!(s.status, SourceStatus::Ready { .. }).then_some(())
    })
    .await;
    // …and they never are.
    assert!(library(&h, &t).await.sources.iter().all(|s| s.id != "f1"));
    let doc = document(&h, "f1").await;
    assert_eq!(
        doc.origin,
        Origin::FolderFile {
            workspace_id: "w1".into(),
            path: "notes/waves.md".into()
        }
    );
    let rows = h.state.store.passages_of("f1").await.unwrap();
    assert!(
        rows.iter()
            .all(|r| r.origin == "folder" && r.scope_ids == ["w1"])
    );
}

/// A store as the service left it before the passage index: Ready sources with `chunk`
/// and `media` rows, attached to a chat (so the unattached clean-up keeps them).
async fn old_store(ids: &[&str]) -> Store {
    let store = Store::memory().await.unwrap();
    let now = Utc::now();
    for (i, id) in ids.iter().enumerate() {
        let s = Source {
            access: Vec::new(),
            deny: Vec::new(),
            owner: None,
            media_url: None,
            id: id.to_string(),
            topic_id: None,
            name: format!("{id}.md"),
            folder: None,
            kind: SourceKind::Text,
            bytes: FOURIER.len() as u64,
            status: SourceStatus::Ready {
                chunks: 2,
                pages: None,
                warning: None,
            },
            created_at: now - chrono::Duration::minutes(10 - i as i64),
            workspace_id: None,
            modified: None,
        };
        store.put(db::SOURCE, id, &s).await.unwrap();
        let text = format!("old passage of {id} about Gibbs ringing");
        let old = ChunkRow {
            source_id: id.to_string(),
            topic_id: None,
            ord: 0,
            page: None,
            start_ms: None,
            end_ms: None,
            vector: margin_embed::fake::FakeEmbedder
                .embed(std::slice::from_ref(&text), margin_embed::Purpose::DOCUMENT)
                .unwrap()
                .remove(0),
            text,
        };
        store
            .put(db::CHUNK, &format!("{id}-0"), &old)
            .await
            .unwrap();
        let media = margin_service::sources::MediaRow {
            source_id: id.to_string(),
            topic_id: None,
            ord: 0,
            kind: margin_service::sources::MediaKind::Audio,
            file: None,
            page: None,
            start_ms: Some(0),
            end_ms: Some(10_000),
            text: String::new(),
            vector: vec![0.0; margin_embed::DIM],
        };
        store
            .put(db::MEDIA, &format!("{id}-m0"), &media)
            .await
            .unwrap();
    }
    let chat: margin_shared::Chat = serde_json::from_value(json!({
        "id": "c1", "title": "Old", "state": {"state": "idle"},
        "source_ids": ids,
        "created_at": now, "updated_at": now
    }))
    .unwrap();
    store.put(db::CHAT, "c1", &chat).await.unwrap();
    store
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_start_moves_old_sources_to_the_passage_index_in_one_pass() {
    let store = old_store(&["s-a", "s-gone"]).await;
    let h = start_with(Setup {
        store: Some(store),
        // Only s-a's file is still there.
        files: vec![("sources/s-a/file".into(), FOURIER.as_bytes().to_vec())],
        ..Setup::default()
    })
    .await;
    let t = h.sign_in("Mac").await;
    // The library is served meanwhile.
    let r = h.get(&t, "/api/library").send().await.unwrap();
    assert_eq!(r.status(), 200);

    eventually(|| async {
        let row = h.state.settings_row().await.unwrap();
        row.migrations
            .contains(&margin_service::ingest::PASSAGES_MIGRATION.to_string())
            .then_some(())
    })
    .await;
    let a = wait_source(&h, &t, "s-a", is_ready).await;
    assert_eq!(
        a.status,
        SourceStatus::Ready {
            chunks: 3,
            pages: None,
            warning: None,
        }
    );
    let rows = chunks(&h, "s-a").await;
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|r| r.scope_ids == ["c1"]));
    // A source whose file is gone is Failed, to be read again.
    let gone = wait_source(&h, &t, "s-gone", |s| {
        matches!(s, SourceStatus::Failed { .. })
    })
    .await;
    assert_eq!(
        gone.status,
        SourceStatus::Failed {
            error: margin_service::ingest::FILE_GONE.into()
        }
    );
    // The old tables are gone.
    assert!(!h.state.store.table_exists(db::CHUNK).await.unwrap());
    assert!(!h.state.store.table_exists(db::MEDIA).await.unwrap());
    // Retrieval reads the passages.
    let found = margin_service::sources::passages(&h.state, &["s-a".into()], "Gibbs").await;
    assert!(!found.is_empty());
    assert!(
        found
            .iter()
            .all(|p| !p.passage.text.starts_with("old passage"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_model_the_migration_waits_and_retrieval_reads_the_old_rows() {
    let store = old_store(&["s-a"]).await;
    let h = start_with(Setup {
        store: Some(store),
        model_downloaded: false,
        files: vec![("sources/s-a/file".into(), FOURIER.as_bytes().to_vec())],
        ..Setup::default()
    })
    .await;
    let t = h.sign_in("Mac").await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !h.state
            .settings_row()
            .await
            .unwrap()
            .migrations
            .contains(&margin_service::ingest::PASSAGES_MIGRATION.to_string())
    );
    assert!(h.state.store.table_exists(db::CHUNK).await.unwrap());
    assert!(h.state.store.passages_of("s-a").await.unwrap().is_empty());
    // The old rows still answer, as passages without a document.
    let old = margin_service::sources::candidates(&h.state, "s-a")
        .await
        .unwrap();
    assert_eq!(
        old.iter().map(|p| p.modality.as_str()).collect::<Vec<_>>(),
        ["text", "sound"]
    );

    // Once the model is there, the pass runs.
    let r = h.post(&t, "/api/embedding/download").send().await.unwrap();
    assert_eq!(r.status(), 200);
    eventually(|| async {
        let row = h.state.settings_row().await.unwrap();
        row.migrations
            .contains(&margin_service::ingest::PASSAGES_MIGRATION.to_string())
            .then_some(())
    })
    .await;
    assert_eq!(chunks(&h, "s-a").await.len(), 3);
    assert!(!h.state.store.table_exists(db::CHUNK).await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_speech_model_downloads_shows_and_can_be_deleted_where_the_service_uses_one() {
    use margin_shared::SpeechModel;
    let h = start().await;
    let t = h.sign_in("Mac").await;
    // Source tools without a speech model (a fake that has none) offer none.
    assert_eq!(embedding(&h, &t).await.speech, None);
    for r in [
        h.post(&t, routes::SPEECH_MODEL_DOWNLOAD),
        h.delete(&t, routes::SPEECH_MODEL),
    ] {
        let (status, err) = api_error(r.send().await.unwrap()).await;
        assert_eq!((status, err.code.as_str()), (409, "no_speech_model"));
    }

    h.source_tools
        .has_speech_model
        .store(true, Ordering::SeqCst);
    let not_downloaded = Some(SpeechModel {
        repo: FAKE_SPEECH_REPO.into(),
        state: EmbeddingModelState::NotDownloaded {
            download_bytes: FAKE_SPEECH_BYTES,
        },
    });
    assert_eq!(embedding(&h, &t).await.speech, not_downloaded);
    // Without it, recordings are read without a transcript and say where to get it.
    let audio = upload_ok(&h, &t, "talk.mp3", None, "waves carry energy").await;
    let audio = wait_source(&h, &t, &audio.id, is_ready).await;
    match &audio.status {
        SourceStatus::Ready {
            warning: Some(warning),
            ..
        } => assert!(
            warning.contains("the speech model isn't downloaded (Settings → Sources"),
            "{warning}"
        ),
        other => panic!("expected Ready with a warning, got {other:?}"),
    }
    h.source_tools.speech.fail.store(true, Ordering::SeqCst);
    let mut socket = h.socket(&t).await;
    let m: EmbeddingModel = h
        .post(&t, routes::SPEECH_MODEL_DOWNLOAD)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(matches!(
        m.speech.unwrap().state,
        EmbeddingModelState::Downloading { .. }
    ));
    let failed = eventually(|| async {
        let s = embedding(&h, &t).await.speech.unwrap();
        matches!(s.state, EmbeddingModelState::Failed { .. }).then_some(s)
    })
    .await;
    assert!(
        matches!(&failed.state, EmbeddingModelState::Failed { error } if error.contains("offline")),
        "{failed:?}"
    );

    // Retried: progress arrives as events, the embedding model's part unchanged.
    h.post(&t, routes::SPEECH_MODEL_DOWNLOAD)
        .send()
        .await
        .unwrap();
    // Deleting while it downloads is refused.
    let (status, err) = api_error(h.delete(&t, routes::SPEECH_MODEL).send().await.unwrap()).await;
    assert_eq!((status, err.code.as_str()), (409, "model_downloading"));
    let events = socket
        .until(|e| {
            matches!(e, Event::EmbeddingModelChanged { model }
                if matches!(model.speech.as_ref().map(|s| &s.state), Some(EmbeddingModelState::Ready { .. })))
        })
        .await;
    let models: Vec<&EmbeddingModel> = events
        .iter()
        .filter_map(|e| match e {
            Event::EmbeddingModelChanged { model } => Some(model),
            _ => None,
        })
        .collect();
    assert!(
        models.iter().any(|m| matches!(
            m.speech.as_ref().unwrap().state,
            EmbeddingModelState::Downloading { received, total: FAKE_SPEECH_BYTES } if received > 0
        )),
        "{models:?}"
    );
    assert!(models.iter().all(|m| m.state == models[0].state));
    assert_eq!(
        embedding(&h, &t).await.speech.unwrap().state,
        EmbeddingModelState::Ready {
            bytes: FAKE_SPEECH_BYTES
        }
    );
    // Downloaded already: nothing to do.
    h.post(&t, routes::SPEECH_MODEL_DOWNLOAD)
        .send()
        .await
        .unwrap();
    assert_eq!(h.source_tools.speech.downloads.load(Ordering::SeqCst), 2);

    let m: EmbeddingModel = h
        .delete(&t, routes::SPEECH_MODEL)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(m.speech, not_downloaded);
    assert_eq!(h.source_tools.speech.removes.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_ocr_pictures_are_ready_with_a_warning() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    h.source_tools.no_ocr.store(true, Ordering::SeqCst);
    let image = upload_ok(&h, &t, "board.png", None, "a whiteboard").await;
    let image = wait_source(&h, &t, &image.id, is_ready).await;
    match &image.status {
        SourceStatus::Ready {
            warning: Some(warning),
            ..
        } => {
            assert!(warning.contains("without the text in it"), "{warning}");
            assert!(warning.contains("tesseract"), "{warning}");
        }
        other => panic!("expected Ready with a warning, got {other:?}"),
    }
    // Text needs no OCR.
    let text = upload_ok(&h, &t, "notes.md", None, FOURIER).await;
    let text = wait_source(&h, &t, &text.id, is_ready).await;
    assert!(
        matches!(text.status, SourceStatus::Ready { warning: None, .. }),
        "{:?}",
        text.status
    );
}
