mod common;

use std::sync::atomic::Ordering;

use chrono::Utc;
use common::*;
use margin_service::db::{self, Store};
use margin_service::secrets::{GITHUB_CLIENT_SECRET, MemoryStore, OPENAI_API_KEY, SecretStore};
use margin_service::state::{INTERRUPTED, SETTINGS_ID, SettingsRow, StoredLesson, StoredVideo};
use margin_shared::pipeline::PipelineConfig;
use margin_shared::{
    ApiError, Device, Event, Lesson, LessonStatus, Level, Library, LoginState, Me, Note, Proposal,
    ProposalStatus, SearchHit, SearchKind, Settings, Topic, Video, VideoStatus,
};
use serde_json::json;

async fn api_error(r: reqwest::Response) -> (u16, ApiError) {
    let status = r.status().as_u16();
    (status, r.json().await.unwrap())
}

// ---------------------------------------------------------------------------
// Sign-in
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn sign_in_approves_once_and_the_token_works() {
    let h = start().await;
    let s = h.start_login("MacBook").await;
    let authorize = reqwest::Url::parse(&s.authorize_url).unwrap();
    assert!(s.authorize_url.starts_with(&format!(
        "{}/login/oauth/authorize?",
        h.state.config.github_oauth_url
    )));
    let q: std::collections::HashMap<_, _> = authorize.query_pairs().into_owned().collect();
    assert_eq!(q["client_id"], CLIENT_ID);
    assert_eq!(q["redirect_uri"], format!("{}/auth/github/callback", h.url));
    assert_eq!(q["state"], s.login_id);
    assert_eq!(q["scope"], "read:user");
    assert!((s.expires_at - Utc::now()).num_seconds() > 590);

    assert_eq!(h.poll(&s).await, LoginState::Pending);

    // A wrong poll secret can't collect it.
    let r = h
        .http
        .post(h.u("/auth/poll"))
        .json(&json!({"login_id": s.login_id, "poll_secret": "nope"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    let page = h.callback(OWNER_CODE, &s.login_id).await;
    assert_eq!(page.status(), 200);
    assert!(
        page.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let html = page.text().await.unwrap();
    assert!(html.contains("Signed in to Margin. You can return to the app."));

    let token = match h.poll(&s).await {
        LoginState::Approved {
            token,
            github_login,
        } => {
            assert_eq!(github_login, "nkr1SHNA");
            token
        }
        other => panic!("{other:?}"),
    };
    // Exactly once.
    assert_eq!(h.poll(&s).await, LoginState::Expired);

    let me: Me = h
        .get(&token, "/api/me")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me.github_login, "nkr1SHNA");
    assert_eq!(me.device.name, "MacBook");
    assert!(me.device.current);

    // Stored hashed, never in clear.
    let rows: Vec<serde_json::Value> = h.state.store.list(db::DEVICE).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].to_string().contains(&token));
}

#[tokio::test(flavor = "multi_thread")]
async fn sign_in_is_denied_for_another_github_user() {
    let h = start().await;
    let s = h.start_login("Phone").await;
    let page = h.callback(OTHER_CODE, &s.login_id).await;
    assert_eq!(page.status(), 403);
    assert!(page.text().await.unwrap().contains("@someone-else isn"));
    match h.poll(&s).await {
        LoginState::Denied { reason } => assert!(reason.contains("someone-else")),
        other => panic!("{other:?}"),
    }
    assert!(
        h.state
            .store
            .list::<serde_json::Value>(db::DEVICE)
            .await
            .unwrap()
            .is_empty()
    );

    // A bad code is denied too, and the callback for an unknown state is "expired".
    let s2 = h.start_login("Phone").await;
    assert_eq!(h.callback("bad-code", &s2.login_id).await.status(), 403);
    assert!(matches!(h.poll(&s2).await, LoginState::Denied { .. }));
    let page = h.callback(OWNER_CODE, "not-a-login").await;
    assert_eq!(page.status(), 410);
}

#[tokio::test(flavor = "multi_thread")]
async fn logins_expire() {
    let h = start().await;
    let s = h.start_login("Tablet").await;
    h.state
        .logins
        .lock()
        .unwrap()
        .get_mut(&s.login_id)
        .unwrap()
        .expires_at = Utc::now() - chrono::Duration::seconds(1);
    let page = h.callback(OWNER_CODE, &s.login_id).await;
    assert_eq!(page.status(), 410);
    assert!(page.text().await.unwrap().contains("expired"));
    assert_eq!(h.poll(&s).await, LoginState::Expired);

    // Approved but collected too late is also expired.
    let s = h.start_login("Tablet").await;
    assert_eq!(h.callback(OWNER_CODE, &s.login_id).await.status(), 200);
    h.state
        .logins
        .lock()
        .unwrap()
        .get_mut(&s.login_id)
        .unwrap()
        .expires_at = Utc::now() - chrono::Duration::seconds(1);
    assert_eq!(h.poll(&s).await, LoginState::Expired);
    assert!(
        h.state
            .store
            .list::<serde_json::Value>(db::DEVICE)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_start_is_rate_limited() {
    let h = start().await;
    for _ in 0..10 {
        h.start_login("x").await;
    }
    let r = h
        .http
        .post(h.u("/auth/start"))
        .json(&json!({"device_name": "x"}))
        .send()
        .await
        .unwrap();
    let (status, e) = api_error(r).await;
    assert_eq!((status, e.code.as_str()), (429, "rate_limited"));
}

#[tokio::test(flavor = "multi_thread")]
async fn api_requires_a_valid_token() {
    let h = start().await;
    assert_eq!(
        h.http
            .get(h.u("/health"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "ok"
    );
    let r = h.http.get(h.u("/api/library")).send().await.unwrap();
    let (status, e) = api_error(r).await;
    assert_eq!((status, e.code.as_str()), (401, "unauthorized"));
    let (status, _) = api_error(h.get("not-a-token", "/api/me").send().await.unwrap()).await;
    assert_eq!(status, 401);
    // The socket upgrade needs it too.
    let req = format!("{}/api/events", h.url.replace("http://", "ws://"));
    assert!(tokio_tungstenite::connect_async(req).await.is_err());
    // Bad JSON is an ApiError.
    let token = h.sign_in("d").await;
    let r = h
        .post(&token, "/api/topics")
        .header("content-type", "application/json")
        .body("{")
        .send()
        .await
        .unwrap();
    let (status, e) = api_error(r).await;
    assert_eq!((status, e.code.as_str()), (400, "bad_request"));
}

#[tokio::test(flavor = "multi_thread")]
async fn revoking_a_device_rejects_its_token_and_closes_its_socket() {
    let h = start().await;
    let a = h.sign_in("Mac").await;
    let b = h.sign_in("Phone").await;

    let devices: Vec<Device> = h
        .get(&a, "/api/devices")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(devices.len(), 2);
    let mine: Vec<_> = devices.iter().filter(|d| d.current).collect();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].name, "Mac");
    let phone = devices.iter().find(|d| d.name == "Phone").unwrap().clone();

    let mut sock_b = h.socket(&b).await;
    let mut sock_a = h.socket(&a).await;
    let r = h
        .delete(&a, &format!("/api/devices/{}", phone.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    // After presence and revocation events.
    assert_eq!(sock_b.closed().await, Some(1008));
    assert_eq!(h.get(&b, "/api/me").send().await.unwrap().status(), 401);
    assert_eq!(h.get(&a, "/api/me").send().await.unwrap().status(), 200);
    assert_eq!(
        h.delete(&a, &format!("/api/devices/{}", phone.id))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    // Revoking the current device is allowed.
    let me: Me = h
        .get(&a, "/api/me")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        h.delete(&a, &format!("/api/devices/{}", me.device.id))
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    assert_eq!(sock_a.closed().await, Some(1008));
    assert_eq!(h.get(&a, "/api/me").send().await.unwrap().status(), 401);
}

// ---------------------------------------------------------------------------
// Library
// ---------------------------------------------------------------------------

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

async fn library(h: &Harness, token: &str) -> Library {
    h.get(token, "/api/library")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Has the agent propose a course, then accepts it; returns the topic and the proposal.
async fn create_lessons(
    h: &Harness,
    token: &str,
    prompt: &str,
    n: u32,
    with_videos: bool,
) -> (Topic, Proposal) {
    let p = h.propose(token, &ask_request(prompt, n, with_videos)).await;
    assert_eq!(p.status, ProposalStatus::Ready);
    let outline = p.outline.clone().unwrap();
    let r = h
        .post(token, &format!("/api/proposals/{}/accept", p.id))
        .json(&json!({"outline": outline}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    (r.json().await.unwrap(), p)
}

async fn wait_lessons_ready(h: &Harness, token: &str, topic_id: &str) -> Vec<Lesson> {
    eventually(|| async {
        let lib = library(h, token).await;
        let lessons: Vec<Lesson> = lib
            .lessons
            .into_iter()
            .filter(|l| l.topic_id == topic_id)
            .collect();
        lessons
            .iter()
            .all(|l| l.status == LessonStatus::Ready)
            .then_some(lessons)
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn library_crud_and_cascading_deletes() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    h.configure_ai(&t).await;

    let subject = new_topic(&h, &t, None, "Physics").await;
    let a = new_topic(&h, &t, Some(&subject.id), "Optics").await;
    let b = new_topic(&h, &t, Some(&subject.id), "Waves").await;
    let child = new_topic(&h, &t, Some(&a.id), "Lenses").await;
    assert_eq!(
        (subject.position, a.position, b.position, child.position),
        (0, 0, 1, 0)
    );
    let r = h
        .post(&t, "/api/topics")
        .json(&json!({"parent_id": null, "title": "  "}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let r = h
        .post(&t, "/api/topics")
        .json(&json!({"parent_id": "nope", "title": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // Rename, move (goes last among the new siblings), and refuse cycles.
    let renamed: Topic = h
        .patch(&t, &format!("/api/topics/{}", b.id))
        .json(&json!({"title": "Wave motion"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(renamed.title, "Wave motion");
    let moved: Topic = h
        .patch(&t, &format!("/api/topics/{}", b.id))
        .json(&json!({"parent_id": a.id}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (moved.parent_id.as_deref(), moved.position),
        (Some(a.id.as_str()), 1)
    );
    // An omitted parent_id leaves the parent unchanged.
    let same: Topic = h
        .patch(&t, &format!("/api/topics/{}", b.id))
        .json(&json!({"title": "B"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(same.parent_id.as_deref(), Some(a.id.as_str()));
    // `null` moves the topic to the root.
    let rooted: Topic = h
        .patch(&t, &format!("/api/topics/{}", b.id))
        .json(&json!({"parent_id": null}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rooted.parent_id, None);
    let moved: Topic = h
        .patch(&t, &format!("/api/topics/{}", b.id))
        .json(&json!({"parent_id": subject.id}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (moved.parent_id.as_deref(), moved.position),
        (Some(subject.id.as_str()), 1)
    );
    let r = h
        .patch(&t, &format!("/api/topics/{}", a.id))
        .json(&json!({"parent_id": child.id}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    // Lessons come from an accepted outline, filed under the child topic's subject.
    let (topic, _) = create_lessons(&h, &t, "lenses", 2, false).await;
    let lessons = wait_lessons_ready(&h, &t, &topic.id).await;
    // Move the generated topic under "Lenses" so deleting "Optics" must cascade two levels.
    let r = h
        .patch(&t, &format!("/api/topics/{}", topic.id))
        .json(&json!({"parent_id": child.id}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let note: Note = h
        .post(&t, "/api/notes")
        .json(&json!({"topic_id": topic.id, "lesson_id": lessons[0].id, "title": "Focal length", "body_md": "f = 1/...", "quote": "explains"}))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(note.quote.as_deref(), Some("explains"));
    let topic_note: Note = h
        .post(&t, "/api/notes")
        .json(&json!({"topic_id": child.id, "lesson_id": null, "title": "General", "body_md": "text", "quote": null}))
        .send().await.unwrap().json().await.unwrap();
    let r = h
        .post(&t, "/api/notes")
        .json(&json!({"topic_id": a.id, "lesson_id": lessons[0].id, "title": "x", "body_md": "", "quote": null}))
        .send().await.unwrap();
    assert_eq!(r.status(), 400, "lesson from another topic");
    let edited: Note = h
        .patch(&t, &format!("/api/notes/{}", note.id))
        .json(&json!({"body_md": "edited"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(edited.body_md, "edited");

    // Lesson edits.
    let l: Lesson = h
        .patch(&t, &format!("/api/lessons/{}", lessons[1].id))
        .json(&json!({"title": "Renamed", "body_md": "new body", "position": 7}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (l.title.as_str(), l.body_md.as_str(), l.position),
        ("Renamed", "new body", 7)
    );

    // A video for lesson 0, so the cascade has files to remove.
    let v: Video = h
        .post(&t, &format!("/api/lessons/{}/video", lessons[0].id))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let v = eventually(|| async {
        let v: Video = h
            .get(&t, &format!("/api/videos/{}", v.id))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        (v.status == VideoStatus::Ready).then_some(v)
    })
    .await;
    let video_dir = h.data_dir.join("videos").join(&v.id);
    assert!(video_dir.join("video.mp4").is_file());

    // Deleting lesson 1 removes only it.
    assert_eq!(
        h.delete(&t, &format!("/api/lessons/{}", lessons[1].id))
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    let lib = library(&h, &t).await;
    assert_eq!(lib.lessons.len(), 1);

    let mut sock = h.socket(&t).await;
    // Deleting "Optics" cascades: Lenses, the generated topic, its lesson, notes, video + files.
    assert_eq!(
        h.delete(&t, &format!("/api/topics/{}", a.id))
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    let lib = library(&h, &t).await;
    let ids: Vec<_> = lib.topics.iter().map(|t| t.id.as_str()).collect();
    assert!(ids.contains(&subject.id.as_str()) && ids.contains(&b.id.as_str()));
    assert!(
        !ids.contains(&a.id.as_str())
            && !ids.contains(&child.id.as_str())
            && !ids.contains(&topic.id.as_str())
    );
    assert!(lib.lessons.is_empty());
    assert!(lib.notes.is_empty());
    assert!(lib.videos.is_empty());
    assert!(!video_dir.exists());
    let events = sock
        .until(|e| matches!(e, Event::TopicDeleted { id } if *id == a.id))
        .await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::VideoDeleted { id } if *id == v.id))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::NoteDeleted { id } if *id == note.id))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::NoteDeleted { id } if *id == topic_note.id))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::LessonDeleted { id } if *id == lessons[0].id))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::TopicDeleted { id } if *id == child.id))
    );

    assert_eq!(
        h.delete(&t, &format!("/api/topics/{}", a.id))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        h.delete(&t, &format!("/api/notes/{}", note.id))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}

// ---------------------------------------------------------------------------
// Ask
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ask_accept_streams_lessons_over_the_socket() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    h.configure_ai(&t).await;
    let mut sock = h.socket(&t).await;

    // The old one-shot Ask is gone: outlines come from the agent's propose_course.
    let r = h
        .post(&t, "/api/ask")
        .json(&ask_request("x", 1, false))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    for (prompt, n, why) in [
        (" ", 3, "Say what the course is for."),
        ("x", 0, "An outline needs between 1 and 12 lessons."),
        ("x", 13, "An outline needs between 1 and 12 lessons."),
    ] {
        let err = h
            .propose_in_chat(&t, &ask_request(prompt, n, false))
            .await
            .unwrap_err();
        assert_eq!(err, why);
    }

    let p = h.propose(&t, &ask_request("Fourier", 5, false)).await;
    let events = sock.until(|e| matches!(e, Event::ProposalChanged { proposal } if proposal.status == ProposalStatus::Ready)).await;
    let Event::ProposalChanged { proposal: ready } = events.last().unwrap() else {
        unreachable!()
    };
    assert_eq!(ready.id, p.id);
    assert!(ready.chat_id.is_some(), "the proposal knows its chat");
    assert_eq!(ready.request.lesson_count, 5);
    assert_eq!(ready.request.level, Level::Intermediate);
    let mut outline = ready.outline.clone().unwrap();
    assert_eq!(outline.lessons.len(), 5);
    // The user edits the outline before accepting.
    outline.lessons[0].title = "Edited first lesson".into();
    outline.subject_title = "signals & SYSTEMS".into();

    let topic: Topic = h
        .post(&t, &format!("/api/proposals/{}/accept", p.id))
        .json(&json!({"outline": outline}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let (status, e) = api_error(
        h.post(&t, &format!("/api/proposals/{}/accept", p.id))
            .json(&json!({"outline": outline}))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!((status, e.code.as_str()), (409, "already_accepted"));

    let mut deltas: std::collections::HashMap<String, String> = Default::default();
    let mut ready_bodies: std::collections::HashMap<String, String> = Default::default();
    let mut generating_seen = 0;
    let mut subject_created = None;
    let mut queued = Vec::new();
    while ready_bodies.len() < 5 {
        match sock.next_event().await.expect("socket open") {
            Event::TopicChanged { topic: tc } if tc.parent_id.is_none() => {
                subject_created = Some(tc)
            }
            Event::LessonChanged { lesson } => match lesson.status {
                LessonStatus::Queued => queued.push(lesson),
                LessonStatus::Generating => {
                    assert_eq!(lesson.body_md, "");
                    generating_seen += 1;
                }
                LessonStatus::Ready => {
                    ready_bodies.insert(lesson.id.clone(), lesson.body_md.clone());
                }
                LessonStatus::Failed { error } => panic!("lesson failed: {error}"),
            },
            Event::LessonDelta { lesson_id, text } => {
                deltas.entry(lesson_id).or_default().push_str(&text)
            }
            _ => {}
        }
    }
    let subject = subject_created.expect("subject created");
    assert_eq!(subject.title, "signals & SYSTEMS");
    assert_eq!(topic.parent_id.as_deref(), Some(subject.id.as_str()));
    assert_eq!(queued.len(), 5);
    assert_eq!(queued[0].title, "Edited first lesson");
    assert_eq!(
        queued.iter().map(|l| l.position).collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4]
    );
    assert!(queued.iter().all(|l| l.level == Level::Intermediate));
    assert_eq!(generating_seen, 5);
    for (id, body) in &ready_bodies {
        assert_eq!(
            deltas.get(id),
            Some(body),
            "deltas add up to the final body"
        );
        assert!(body.contains("signals & SYSTEMS / Topic: Fourier"));
    }
    let max = h.generator.max_active.load(Ordering::SeqCst);
    assert!(
        (1..=3).contains(&max),
        "at most 3 lessons at once, saw {max}"
    );

    let accepted: Proposal = h
        .get(&t, &format!("/api/proposals/{}", p.id))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        accepted.status,
        ProposalStatus::Accepted {
            topic_id: topic.id.clone()
        }
    );

    // A second course files under the existing subject (case-insensitive).
    let (topic2, _) = create_lessons(&h, &t, "Laplace", 1, false).await;
    assert_eq!(topic2.parent_id.as_deref(), Some(subject.id.as_str()));
    let lib = library(&h, &t).await;
    assert_eq!(
        lib.topics.iter().filter(|t| t.parent_id.is_none()).count(),
        1
    );

    // Regenerate replaces the body; a failure keeps the old one.
    let lesson = wait_lessons_ready(&h, &t, &topic2.id).await.remove(0);
    sock.close().await;
    let r = h
        .patch(&t, &format!("/api/lessons/{}", lesson.id))
        .json(&json!({"body_md": "hand-written"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let mut sock = h.socket(&t).await;
    let r: Lesson = h
        .post(&t, &format!("/api/lessons/{}/regenerate", lesson.id))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r.status, LessonStatus::Queued);
    let (status, e) = api_error(
        h.post(&t, &format!("/api/lessons/{}/regenerate", lesson.id))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!((status, e.code.as_str()), (409, "lesson_busy"));
    let done = sock.until(|e| matches!(e, Event::LessonChanged { lesson: l } if l.id == lesson.id && l.status == LessonStatus::Ready)).await;
    assert!(done.iter().any(|e| matches!(e, Event::LessonChanged { lesson: l } if l.status == LessonStatus::Generating && l.body_md.is_empty())));
    let Event::LessonChanged {
        lesson: regenerated,
    } = done.last().unwrap()
    else {
        unreachable!()
    };
    assert!(regenerated.body_md.starts_with("# Lesson 1"));

    h.generator
        .fail_lessons
        .lock()
        .unwrap()
        .insert("Lesson 1".into());
    let r = h
        .patch(&t, &format!("/api/lessons/{}", lesson.id))
        .json(&json!({"body_md": "keep me"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    h.post(&t, &format!("/api/lessons/{}/regenerate", lesson.id))
        .send()
        .await
        .unwrap();
    let done = sock.until(|e| matches!(e, Event::LessonChanged { lesson: l } if l.id == lesson.id && matches!(l.status, LessonStatus::Failed { .. }))).await;
    let Event::LessonChanged { lesson: failed } = done.last().unwrap() else {
        unreachable!()
    };
    assert_eq!(failed.body_md, "keep me");
}

#[tokio::test(flavor = "multi_thread")]
async fn generation_needs_an_api_key_and_a_model() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    h.configure_ai(&t).await;
    let p = h.propose(&t, &ask_request("x", 2, false)).await;
    let accept = || {
        h.post(&t, &format!("/api/proposals/{}/accept", p.id))
            .json(&json!({"outline": p.outline.clone().unwrap()}))
            .send()
    };

    // Lessons can't be written without a key or a model, so Create lessons refuses.
    h.patch(&t, "/api/settings")
        .json(&json!({"openai_api_key": ""}))
        .send()
        .await
        .unwrap();
    let (status, e) = api_error(accept().await.unwrap()).await;
    assert_eq!((status, e.code.as_str()), (400, "no_api_key"));
    assert!(e.message.contains("API key"));
    h.patch(&t, "/api/settings")
        .json(&json!({"openai_api_key": API_KEY, "openai_model": ""}))
        .send()
        .await
        .unwrap();
    let (status, e) = api_error(accept().await.unwrap()).await;
    assert_eq!((status, e.code.as_str()), (400, "no_model"));

    h.configure_ai(&t).await;
    let (topic, _) = create_lessons(&h, &t, "x", 1, false).await;
    let lesson = wait_lessons_ready(&h, &t, &topic.id).await.remove(0);

    // Remove the key: regenerate and video refuse.
    h.patch(&t, "/api/settings")
        .json(&json!({"openai_api_key": ""}))
        .send()
        .await
        .unwrap();
    let (status, e) = api_error(
        h.post(&t, &format!("/api/lessons/{}/regenerate", lesson.id))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!((status, e.code.as_str()), (400, "no_api_key"));
    let (status, e) = api_error(
        h.post(&t, &format!("/api/lessons/{}/video", lesson.id))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!((status, e.code.as_str()), (400, "no_api_key"));
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_switch_the_pipeline_by_name() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    let s: Settings = h
        .get(&t, "/api/settings")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s.pipeline, PipelineConfig::default());
    assert_eq!(s.trace_retention_days, 90);
    assert_eq!(
        h.state.pipeline.load().hash,
        PipelineConfig::default().hash()
    );

    // An unknown stage is refused and changes nothing.
    let r = h
        .patch(&t, "/api/settings")
        .json(&json!({"pipeline": {"retriever": {"name": "telepathy"}}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let e: ApiError = r.json().await.unwrap();
    assert_eq!(e.code, "bad_pipeline");
    assert!(e.message.contains("telepathy"), "{}", e.message);
    assert_eq!(h.state.pipeline.load().retriever.name(), "hybrid-rrf");

    // A known one is stored, sent back and used from now on.
    let s: Settings = h
        .patch(&t, "/api/settings")
        .json(&json!({"pipeline": {"retriever": {"name": "dense-brute"}, "budget": {"images_max": 2}}, "trace_retention_days": 30}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s.pipeline.retriever.name, "dense-brute");
    assert_eq!(s.pipeline.budget.images_max, 2);
    assert_eq!(s.trace_retention_days, 30);
    let live = h.state.pipeline.load();
    assert_eq!(live.retriever.name(), "dense-brute");
    assert_eq!(live.hash, s.pipeline.hash());
    for bad in [
        json!({"trace_retention_days": 0}),
        json!({"trace_retention_days": 5000}),
    ] {
        assert_eq!(
            h.patch(&t, "/api/settings")
                .json(&bad)
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_store_the_key_but_never_return_it() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    let s: Settings = h
        .get(&t, "/api/settings")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s.openai_base_url, "https://api.openai.com/v1");
    assert_eq!(s.openai_model, "");
    assert!(!s.has_api_key);
    assert_eq!(
        (s.render_width, s.render_height, s.render_fps),
        (1920, 1080, 30)
    );

    let mut sock = h.socket(&t).await;
    let r = h
        .patch(&t, "/api/settings")
        .json(&json!({"openai_api_key": API_KEY, "openai_model": "m1", "openai_base_url": "http://localhost:9000/v1/", "voice": "alba", "render_width": 1280, "render_height": 720, "render_fps": 24}))
        .send()
        .await
        .unwrap();
    let text = r.text().await.unwrap();
    assert!(!text.contains(API_KEY));
    let s: Settings = serde_json::from_str(&text).unwrap();
    assert!(s.has_api_key);
    assert_eq!(s.openai_base_url, "http://localhost:9000/v1");
    assert_eq!(s.voice.as_deref(), Some("alba"));
    assert_eq!(
        (s.render_width, s.render_height, s.render_fps),
        (1280, 720, 24)
    );
    assert_eq!(
        h.secrets.get(OPENAI_API_KEY).unwrap().as_deref(),
        Some(API_KEY)
    );
    match sock.next_frame().await {
        Frame::Event(e) => {
            assert!(!serde_json::to_string(&e).unwrap().contains(API_KEY));
            assert!(matches!(e, Event::SettingsChanged { settings } if settings.has_api_key));
        }
        Frame::Closed(_) => panic!("closed"),
    }
    let all = h
        .get(&t, "/api/settings")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!all.contains(API_KEY));
    let lib = h
        .get(&t, "/api/library")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!lib.contains(API_KEY));

    for bad in [
        json!({"render_width": 1281}),
        json!({"render_fps": 0}),
        json!({"openai_base_url": "ftp://x"}),
    ] {
        assert_eq!(
            h.patch(&t, "/api/settings")
                .json(&bad)
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
    }
    let s: Settings = h
        .patch(&t, "/api/settings")
        .json(&json!({"openai_api_key": "", "voice": ""}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!s.has_api_key);
    assert_eq!(s.voice, None);
    assert!(!has_key(&h.secrets));
}

// ---------------------------------------------------------------------------
// Video + media
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn videos_render_through_the_queue_with_signed_media() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    h.configure_ai(&t).await;
    // One scene failure: the model repairs it and the second render succeeds.
    h.renderer.scene_failures.store(1, Ordering::SeqCst);
    let mut sock = h.socket(&t).await;

    let (topic, _) = create_lessons(&h, &t, "videos", 2, true).await;
    let mut ready: Vec<Video> = Vec::new();
    let mut statuses = Vec::new();
    while ready.len() < 2 {
        if let Event::VideoChanged { video } = sock.next_event().await.expect("open") {
            statuses.push(video.status.clone());
            match &video.status {
                VideoStatus::Ready => ready.push(video),
                VideoStatus::Failed { error } => panic!("video failed: {error}"),
                _ => assert!(video.media.is_none()),
            }
        }
    }
    assert!(statuses.contains(&VideoStatus::Queued));
    assert!(statuses.contains(&VideoStatus::Scripting));
    assert!(
        statuses
            .iter()
            .any(|s| matches!(s, VideoStatus::Voicing { .. }))
    );
    assert!(
        statuses
            .iter()
            .any(|s| matches!(s, VideoStatus::Rendering { progress: Some(_) }))
    );
    // Two lessons, one repair → 3 scenes written, the second with the failed attempt.
    let scenes = h.generator.scene_requests.lock().unwrap().clone();
    assert_eq!(scenes.len(), 3);
    let repair = scenes
        .iter()
        .find(|s| s.previous_attempt.is_some())
        .expect("a repair request");
    assert!(
        repair
            .previous_attempt
            .as_ref()
            .unwrap()
            .error
            .contains("NameError")
    );
    assert!(
        repair
            .previous_attempt
            .as_ref()
            .unwrap()
            .code
            .contains("attempt 1")
    );
    let cfg = h.renderer.configs.lock().unwrap()[0].clone();
    assert_eq!(cfg.sandbox, margin_video::SandboxMode::Unsandboxed);
    assert_eq!((cfg.width, cfg.height, cfg.fps), (1920, 1080, 30));
    assert_eq!(
        cfg.manimgx,
        std::path::PathBuf::from("/opt/fake/bin/manimgx")
    );

    let v = &ready[0];
    assert_eq!(v.duration_seconds, Some(12.5));
    assert!(v.scene_code.as_deref().unwrap().contains("LessonScene"));
    assert!(v.log.lines().count() <= 200);
    assert!(v.log.contains("render line 249"));
    let media = v.media.clone().unwrap();
    assert!(
        media
            .mp4_url
            .starts_with(&format!("{}/media/videos/{}/video.mp4?exp=", h.url, v.id))
    );
    let hours = (media.expires_at - Utc::now()).num_minutes();
    assert!((355..=360).contains(&hours));

    // The library and GET /api/videos sign URLs too.
    let lib = library(&h, &t).await;
    assert!(lib.videos.iter().all(|v| v.media.is_some()));
    assert_eq!(
        lib.lessons
            .iter()
            .filter(|l| l.topic_id == topic.id)
            .count(),
        2
    );
    let fetched: Video = h
        .get(&t, &format!("/api/videos/{}", v.id))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(fetched.media.is_some());

    // Full file, ranges, types.
    let r = h.http.get(&media.mp4_url).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "video/mp4");
    assert_eq!(r.bytes().await.unwrap().to_vec(), mp4_content());
    let r = h
        .http
        .get(&media.mp4_url)
        .header("range", "bytes=100-199")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    assert_eq!(
        r.headers()["content-range"],
        format!("bytes 100-199/{MP4_BYTES}")
    );
    assert_eq!(
        r.bytes().await.unwrap().to_vec(),
        mp4_content()[100..200].to_vec()
    );
    let r = h.http.get(&media.storyboard_url).send().await.unwrap();
    assert_eq!(
        (
            r.status().as_u16(),
            r.headers()["content-type"].to_str().unwrap()
        ),
        (200, "image/png")
    );
    let r = h
        .http
        .get(media.captions_url.as_ref().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("Hello"));

    // The poster: signed, served as PNG, range-capable.
    let poster_url = media.poster_url.clone().expect("a poster url");
    assert!(poster_url.starts_with(&format!("{}/media/videos/{}/poster.png?exp=", h.url, v.id)));
    let q: std::collections::HashMap<String, String> = reqwest::Url::parse(&poster_url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    let exp: i64 = q["exp"].parse().unwrap();
    assert_eq!(q["sig"], h.state.media_signature(&v.id, "poster.png", exp));
    let r = h.http.get(&poster_url).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "image/png");
    assert_eq!(r.bytes().await.unwrap().to_vec(), poster_content());
    let r = h
        .http
        .get(&poster_url)
        .header("range", "bytes=8-15")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    assert_eq!(r.headers()["content-type"], "image/png");
    assert_eq!(
        r.headers()["content-range"],
        format!("bytes 8-15/{POSTER_BYTES}")
    );
    assert_eq!(
        r.bytes().await.unwrap().to_vec(),
        poster_content()[8..16].to_vec()
    );
    let unsigned = poster_url.split('?').next().unwrap().to_string();
    let (status, _) = api_error(h.http.get(&unsigned).send().await.unwrap()).await;
    assert_eq!(status, 403);

    // Tampered and expired links fail.
    let tampered = media.mp4_url.replace("video.mp4", "storyboard.png");
    let (status, e) = api_error(h.http.get(&tampered).send().await.unwrap()).await;
    assert_eq!((status, e.code.as_str()), (403, "bad_signature"));
    let (status, _) = api_error(
        h.http
            .get(format!("{}/media/videos/{}/video.mp4", h.url, v.id))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, 403);
    let past = Utc::now().timestamp() - 10;
    let sig = h.state.media_signature(&v.id, "video.mp4", past);
    let (status, e) = api_error(
        h.http
            .get(format!(
                "{}/media/videos/{}/video.mp4?exp={past}&sig={sig}",
                h.url, v.id
            ))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!((status, e.code.as_str()), (403, "link_expired"));
    let future = Utc::now().timestamp() + 60;
    let sig = h.state.media_signature(&v.id, "../db", future);
    let r = h
        .http
        .get(format!(
            "{}/media/videos/{}/..%2Fdb?exp={future}&sig={sig}",
            h.url, v.id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // Re-render keeps the id and goes through the pipeline again.
    let again: Video = h
        .post(&t, &format!("/api/lessons/{}/video", v.lesson_id))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (again.id.as_str(), &again.status),
        (v.id.as_str(), &VideoStatus::Queued)
    );
    assert!(again.media.is_none());
    sock.until(|e| matches!(e, Event::VideoChanged { video } if video.id == v.id && video.status == VideoStatus::Ready)).await;
}

async fn wait_video_ready(h: &Harness, token: &str, id: &str, renders: u32) -> Video {
    eventually(|| async {
        let v: Video = h
            .get(token, &format!("/api/videos/{id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        (v.status == VideoStatus::Ready && h.renderer.renders.load(Ordering::SeqCst) >= renders)
            .then_some(v)
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn rerender_clears_the_poster_and_a_render_without_one_has_no_poster_url() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    h.configure_ai(&t).await;
    let (topic, _) = create_lessons(&h, &t, "posters", 1, false).await;
    let lesson = wait_lessons_ready(&h, &t, &topic.id).await.remove(0);

    let v: Video = h
        .post(&t, &format!("/api/lessons/{}/video", lesson.id))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let v = wait_video_ready(&h, &t, &v.id, 1).await;
    let old_poster = v.media.unwrap().poster_url.expect("a poster url");
    assert_eq!(h.http.get(&old_poster).send().await.unwrap().status(), 200);

    // Re-render with a scene that has no poster: the flag drops as soon as it queues.
    h.renderer.no_poster.store(true, Ordering::SeqCst);
    let again: Video = h
        .post(&t, &format!("/api/lessons/{}/video", lesson.id))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(again.id, v.id);
    let stored: StoredVideo = h.state.store.get(db::VIDEO, &v.id).await.unwrap().unwrap();
    assert!(!stored.has_poster);
    let again = wait_video_ready(&h, &t, &v.id, 2).await;
    let media = again.media.unwrap();
    assert!(media.captions_url.is_some());
    assert_eq!(media.poster_url, None);
    let (status, _) = api_error(h.http.get(&old_poster).send().await.unwrap()).await;
    assert_eq!(status, 404);
    let poster_path = h.data_dir.join("videos").join(&v.id).join("poster.png");
    assert!(!poster_path.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn videos_stored_before_posters_existed_still_load() {
    let store = Store::memory().await.unwrap();
    let now = Utc::now();
    let id = "00000000-0000-4000-8000-0000000000a1";
    let mut row = serde_json::to_value(StoredVideo {
        video: Video {
            id: id.into(),
            lesson_id: "l-gone".into(),
            status: VideoStatus::Ready,
            scene_code: None,
            log: String::new(),
            duration_seconds: Some(3.0),
            media: None,
            created_at: now,
            updated_at: now,
        },
        has_captions: true,
        has_poster: true,
    })
    .unwrap();
    assert!(row.as_object_mut().unwrap().remove("has_poster").is_some());
    store.put(db::VIDEO, id, &row).await.unwrap();
    let loaded: StoredVideo = store.get(db::VIDEO, id).await.unwrap().unwrap();
    assert!(loaded.has_captions);
    assert!(!loaded.has_poster);

    let h = start_with(Setup {
        store: Some(store),
        ..Setup::default()
    })
    .await;
    let t = h.sign_in("Mac").await;
    let v: Video = h
        .get(&t, &format!("/api/videos/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let media = v.media.expect("a Ready video carries media");
    assert!(media.captions_url.is_some());
    assert_eq!(media.poster_url, None);
    // Without the flag the poster isn't served, even with a valid signature.
    let exp = Utc::now().timestamp() + 60;
    let sig = h.state.media_signature(id, "poster.png", exp);
    let (status, _) = api_error(
        h.http
            .get(format!(
                "{}/media/videos/{id}/poster.png?exp={exp}&sig={sig}",
                h.url
            ))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_tools_fail_the_video() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    h.configure_ai(&t).await;
    h.renderer
        .missing
        .lock()
        .unwrap()
        .insert("fluidaudiocli".into());
    let (topic, _) = create_lessons(&h, &t, "x", 1, false).await;
    let lesson = wait_lessons_ready(&h, &t, &topic.id).await.remove(0);
    let tools: Vec<margin_shared::ToolStatus> = h
        .get(&t, "/api/tools")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        tools
            .iter()
            .any(|t| t.name == "fluidaudiocli" && !t.found && !t.install_commands.is_empty())
    );

    let v: Video = h
        .post(&t, &format!("/api/lessons/{}/video", lesson.id))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let v = eventually(|| async {
        let v: Video = h
            .get(&t, &format!("/api/videos/{}", v.id))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        matches!(v.status, VideoStatus::Failed { .. }).then_some(v)
    })
    .await;
    assert_eq!(
        v.status,
        VideoStatus::Failed {
            error: "fluidaudiocli isn't installed".into()
        }
    );
    assert_eq!(h.renderer.renders.load(Ordering::SeqCst), 0);
    assert!(h.generator.scene_requests.lock().unwrap().is_empty());

    // A lesson that isn't ready can't get a video.
    h.generator
        .fail_lessons
        .lock()
        .unwrap()
        .insert("Lesson 1".into());
    h.post(&t, &format!("/api/lessons/{}/regenerate", lesson.id))
        .send()
        .await
        .unwrap();
    eventually(|| async {
        let lib = library(&h, &t).await;
        lib.lessons
            .iter()
            .any(|l| matches!(l.status, LessonStatus::Failed { .. }))
            .then_some(())
    })
    .await;
    let (status, e) = api_error(
        h.post(&t, &format!("/api/lessons/{}/video", lesson.id))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!((status, e.code.as_str()), (409, "lesson_not_ready"));
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn search_orders_topics_lessons_notes_with_snippets() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    h.configure_ai(&t).await;
    let subject = new_topic(&h, &t, None, "Signals").await;
    let topic = new_topic(&h, &t, Some(&subject.id), "Fourier analysis").await;
    let (gen_topic, _) = create_lessons(&h, &t, "fourier", 1, false).await;
    let lesson = wait_lessons_ready(&h, &t, &gen_topic.id).await.remove(0);
    let long = format!(
        "{} The FOURIER transform turns signals into frequencies. {}",
        "Intro words. ".repeat(20),
        "More. ".repeat(40)
    );
    h.patch(&t, &format!("/api/lessons/{}", lesson.id))
        .json(&json!({"body_md": long}))
        .send()
        .await
        .unwrap();
    h.post(&t, "/api/notes")
        .json(&json!({"topic_id": topic.id, "lesson_id": null, "title": "My note", "body_md": "nothing here", "quote": "a fourier quote"}))
        .send().await.unwrap();

    let hits: Vec<SearchHit> = h
        .get(&t, "/api/search")
        .query(&[("q", "fourier")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let kinds: Vec<SearchKind> = hits.iter().map(|h| h.kind).collect();
    let first_lesson = kinds.iter().position(|k| *k == SearchKind::Lesson).unwrap();
    let first_note = kinds.iter().position(|k| *k == SearchKind::Note).unwrap();
    assert!(
        kinds[..first_lesson]
            .iter()
            .all(|k| *k == SearchKind::Topic)
    );
    assert!(first_lesson < first_note);
    let topic_hit = hits.iter().find(|h| h.id == topic.id).unwrap();
    assert_eq!(topic_hit.context, "Signals");
    let lesson_hit = hits.iter().find(|h| h.id == lesson.id).unwrap();
    assert!(lesson_hit.snippet.contains("FOURIER transform"));
    assert!(lesson_hit.snippet.starts_with('…') && lesson_hit.snippet.ends_with('…'));
    assert!(lesson_hit.snippet.chars().count() <= 162);
    assert_eq!(
        lesson_hit.context,
        format!("Signals & Systems / {}", gen_topic.title)
    );
    let note_hit = hits.iter().find(|h| h.kind == SearchKind::Note).unwrap();
    assert_eq!(note_hit.snippet, "a fourier quote");
    assert_eq!(note_hit.context, "Signals / Fourier analysis");

    let none: Vec<SearchHit> = h
        .get(&t, "/api/search")
        .query(&[("q", "zzz")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(none.is_empty());
    for i in 0..35 {
        new_topic(&h, &t, None, &format!("Many {i}")).await;
    }
    let many: Vec<SearchHit> = h
        .get(&t, "/api/search")
        .query(&[("q", "many")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(many.len(), 30);
}

// ---------------------------------------------------------------------------
// Startup recovery
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn startup_marks_interrupted_jobs_failed_and_resumes_queued_work() {
    let store = Store::memory().await.unwrap();
    let now = Utc::now();
    let topic = Topic {
        id: "00000000-0000-4000-8000-000000000001".into(),
        parent_id: None,
        title: "T".into(),
        position: 0,
        created_at: now,
        updated_at: now,
    };
    store.put(db::TOPIC, &topic.id, &topic).await.unwrap();
    let lesson =
        |id: &str, pos: i64, status: LessonStatus, body: &str, prev: Option<&str>| StoredLesson {
            lesson: Lesson {
                citations: Vec::new(),
                id: id.into(),
                topic_id: topic.id.clone(),
                title: format!("L{pos}"),
                summary: String::new(),
                position: pos,
                level: Level::Intro,
                minutes: 3,
                body_md: body.into(),
                status,
                created_at: now,
                updated_at: now,
            },
            proposal_id: None,
            previous_body: prev.map(Into::into),
            previous_citations: None,
            author: Default::default(),
            prompt: None,
            with_quiz: false,
        };
    store
        .put(
            db::LESSON,
            "l-gen",
            &lesson(
                "l-gen",
                0,
                LessonStatus::Generating,
                "half",
                Some("old body"),
            ),
        )
        .await
        .unwrap();
    store
        .put(
            db::LESSON,
            "l-queued",
            &lesson("l-queued", 1, LessonStatus::Queued, "", None),
        )
        .await
        .unwrap();
    store
        .put(
            db::LESSON,
            "l-ready",
            &lesson("l-ready", 2, LessonStatus::Ready, "done", None),
        )
        .await
        .unwrap();
    let video = |id: &str, status: VideoStatus| StoredVideo {
        video: Video {
            id: id.into(),
            lesson_id: "l-ready".into(),
            status,
            scene_code: None,
            log: String::new(),
            duration_seconds: None,
            media: None,
            created_at: now,
            updated_at: now,
        },
        has_captions: false,
        has_poster: false,
    };
    store
        .put(
            db::VIDEO,
            "v-render",
            &video(
                "v-render",
                VideoStatus::Rendering {
                    progress: Some(0.3),
                },
            ),
        )
        .await
        .unwrap();
    let proposal = Proposal {
        id: "p1".into(),
        request: ask_request("x", 2, false),
        status: ProposalStatus::Drafting,
        outline: None,
        created_at: now,
        chat_id: None,
    };
    store.put(db::PROPOSAL, "p1", &proposal).await.unwrap();
    store
        .put(
            db::SETTINGS,
            SETTINGS_ID,
            &SettingsRow {
                openai_model: "m".into(),
                ..SettingsRow::default()
            },
        )
        .await
        .unwrap();

    let secrets = MemoryStore::new()
        .with(GITHUB_CLIENT_SECRET, CLIENT_SECRET)
        .with(OPENAI_API_KEY, API_KEY);
    let h = start_with(Setup {
        store: Some(store),
        secrets,
        ..Setup::default()
    })
    .await;
    let t = h.sign_in("Mac").await;

    let lib = library(&h, &t).await;
    let interrupted = lib.lessons.iter().find(|l| l.id == "l-gen").unwrap();
    assert_eq!(
        interrupted.status,
        LessonStatus::Failed {
            error: INTERRUPTED.into()
        }
    );
    assert_eq!(interrupted.body_md, "old body");
    let v = lib.videos.iter().find(|v| v.id == "v-render").unwrap();
    assert_eq!(
        v.status,
        VideoStatus::Failed {
            error: INTERRUPTED.into()
        }
    );
    let p: Proposal = h
        .get(&t, "/api/proposals/p1")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        p.status,
        ProposalStatus::Failed {
            error: INTERRUPTED.into()
        }
    );
    assert_eq!(
        lib.lessons
            .iter()
            .find(|l| l.id == "l-ready")
            .unwrap()
            .status,
        LessonStatus::Ready
    );

    // The queued lesson is written after restart.
    let resumed = eventually(|| async {
        let lib = library(&h, &t).await;
        let l = lib
            .lessons
            .into_iter()
            .find(|l| l.id == "l-queued")
            .unwrap();
        (l.status == LessonStatus::Ready).then_some(l)
    })
    .await;
    assert!(resumed.body_md.starts_with("# L1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn store_persists_on_disk() {
    let dir = std::env::temp_dir().join(format!("margin-disk-{}", uuid::Uuid::new_v4()));
    {
        let s = Store::open(&dir).await.unwrap();
        s.put(db::NOTE, "n1", &json!({"id": "n1", "x": 1}))
            .await
            .unwrap();
    }
    // Reopening needs the first handle's background tasks to release the files.
    let s = eventually(|| async { Store::open(&dir).await.ok() }).await;
    let v: Option<serde_json::Value> = s.get(db::NOTE, "n1").await.unwrap();
    assert_eq!(v, Some(json!({"id": "n1", "x": 1})));
    drop(s);
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// Socket lifecycle
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_lagging_socket_is_closed_so_the_app_reloads() {
    let h = start().await;
    let t = h.sign_in("Mac").await;
    let mut sock = h.socket(&t).await;
    let settings: Settings = h
        .get(&t, "/api/settings")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    for _ in 0..50_000 {
        h.state.emit(Event::SettingsChanged {
            settings: settings.clone(),
        });
    }
    loop {
        match sock.next_frame().await {
            Frame::Event(_) => {}
            Frame::Closed(code) => {
                assert_eq!(code, Some(1013));
                break;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_closes_sockets() {
    let mut h = start().await;
    let t = h.sign_in("Mac").await;
    let mut sock = h.socket(&t).await;
    h.stop.take().unwrap().send(()).unwrap();
    assert!(matches!(sock.next_frame().await, Frame::Closed(Some(1001))));
}

// ---------------------------------------------------------------------------
// R1 retrieval: the context API and the agent's search tools (passages seeded
// directly through the store; ingest is R1-store's)
// ---------------------------------------------------------------------------

mod retrieval_api {
    use super::*;
    use margin_agent::{NoDevices, ToolContext};
    use margin_embed::{Embedder, Purpose, fake::FakeEmbedder};
    use margin_service::db::PassageRow;
    use margin_shared::context::{
        ContextChanges, Document, DocumentOutline, DocumentStatus, Hit, Origin, Project,
    };
    use margin_shared::{Card, Chat, DeviceRef, SourceKind};
    use serde_json::Value;
    use std::sync::Arc;

    fn vector(text: &str) -> Vec<f32> {
        FakeEmbedder
            .embed(&[text.to_string()], Purpose::DOCUMENT)
            .unwrap()
            .remove(0)
    }

    fn passage(
        id: &str,
        doc: &str,
        scope: &[&str],
        ord: i64,
        section: &str,
        text: &str,
    ) -> PassageRow {
        PassageRow {
            id: id.into(),
            project_id: "my-library".into(),
            document_id: doc.into(),
            origin: "agent".into(),
            scope_ids: scope.iter().map(|s| s.to_string()).collect(),
            access: vec!["company".into()],
            unit_id: format!("{doc}:{id}"),
            modality: "text".into(),
            ord,
            page: None,
            start_line: Some(ord * 10 + 1),
            end_line: Some(ord * 10 + 8),
            start_ms: None,
            end_ms: None,
            section_path: section.into(),
            text: text.into(),
            file: None,
            chunk_hash: format!("h-{id}"),
            embedder: "fake".into(),
            vector: vector(text),
        }
    }

    fn document(id: &str, name: &str, generation: u64, status: DocumentStatus) -> Document {
        Document {
            id: id.into(),
            project_id: "my-library".into(),
            origin: Origin::Agent {
                name: "test".into(),
            },
            name: name.into(),
            kind: SourceKind::Text,
            status,
            generation,
            bytes: 10,
            actor: "agent:test".into(),
            access: vec!["company".into()],
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    /// A topic with two documents in it and one outside it.
    async fn seeded(h: &Harness, t: &str) -> Topic {
        let topic = new_topic(h, t, None, "Thermodynamics").await;
        let store = &h.state.store;
        for (id, name, generation) in [("carnot", "carnot.md", 1), ("bio", "bio.md", 2)] {
            let d = document(id, name, generation, DocumentStatus::Ready { passages: 3 });
            store.put(db::DOCUMENT, id, &d).await.unwrap();
        }
        store
            .insert_many(
                db::PASSAGE,
                &[
                    passage(
                        "c0",
                        "carnot",
                        &[&topic.id],
                        0,
                        "Engines",
                        "The Carnot engine bounds thermal efficiency between two reservoirs.",
                    ),
                    passage(
                        "c1",
                        "carnot",
                        &[&topic.id],
                        1,
                        "Engines",
                        "Efficiency equals one minus the cold over the hot temperature.",
                    ),
                    passage(
                        "c2",
                        "carnot",
                        &[&topic.id],
                        2,
                        "Entropy",
                        "Entropy never decreases in an isolated system.",
                    ),
                    passage(
                        "b0",
                        "bio",
                        &[],
                        0,
                        "Plants",
                        "Photosynthesis stores light energy as sugar in leaves.",
                    ),
                ],
            )
            .await
            .unwrap();
        topic
    }

    async fn context_search(h: &Harness, t: &str, body: Value) -> reqwest::Response {
        h.post(t, "/api/context/search")
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn context_search_retrieves_inside_the_scope() {
        let h = start().await;
        let t = h.sign_in("Mac").await;
        h.configure_ai(&t).await;
        let topic = seeded(&h, &t).await;

        let r = context_search(
            &h,
            &t,
            json!({
                "text": "Carnot engine efficiency",
                "scope": {"scope": "topic", "id": topic.id}, "k": 5
            }),
        )
        .await;
        assert_eq!(r.status(), 200);
        let hits: Vec<Hit> = r.json().await.unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].document_id, "carnot");
        assert!(hits.iter().all(|h| h.document_id == "carnot"), "{hits:?}");
        // Adjacent chunks of one document come back merged, with the first's locator.
        assert!(hits[0].text.contains("Carnot engine") && hits[0].start_line == Some(1));
        assert!(hits.iter().any(|h| h.highlight.is_some()));

        // The whole library reaches the other document.
        let hits: Vec<Hit> = context_search(
            &h,
            &t,
            json!({
                "text": "photosynthesis leaves", "scope": {"scope": "library"}, "k": 3
            }),
        )
        .await
        .json()
        .await
        .unwrap();
        assert_eq!(hits[0].document_id, "bio");

        // A tiny budget packs fewer passages: no literal cap, a share of the window.
        let hits: Vec<Hit> = context_search(&h, &t, json!({
            "text": "Carnot engine efficiency entropy",
            "scope": {"scope": "topic", "id": topic.id}, "k": 5,
            "budget": {"share_of_window": 0.0001, "min_score_pct": 0, "images_max": 0, "passages_min": 0}
        }))
        .await
        .json()
        .await
        .unwrap();
        assert!(hits.len() <= 1, "{hits:?}");

        // Access pre-filter: a reader who may only see finance finds nothing here.
        let hits: Vec<Hit> = context_search(
            &h,
            &t,
            json!({
                "text": "Carnot", "scope": {"scope": "library"}, "k": 5,
                "access": ["company/finance"]
            }),
        )
        .await
        .json()
        .await
        .unwrap();
        assert!(hits.is_empty());

        let (status, e) = api_error(
            context_search(
                &h,
                &t,
                json!({"text": "x", "scope": {"scope": "topic", "id": "nope"}, "k": 5}),
            )
            .await,
        )
        .await;
        assert_eq!((status, e.code.as_str()), (400, "bad_scope"));
        let (status, _) = api_error(
            context_search(
                &h,
                &t,
                json!({"text": " ", "scope": {"scope": "library"}, "k": 5}),
            )
            .await,
        )
        .await;
        assert_eq!(status, 400);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn context_documents_projects_changes_and_ingest() {
        let h = start().await;
        let t = h.sign_in("Mac").await;
        seeded(&h, &t).await;

        let projects: Vec<Project> = h
            .get(&t, "/api/projects")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "My library");

        let outline: DocumentOutline = h
            .get(&t, "/api/context/documents/carnot/outline")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let sections: Vec<(&str, u32, u32)> = outline
            .sections
            .iter()
            .map(|s| (s.section_path.as_str(), s.from_ord, s.to_ord))
            .collect();
        assert_eq!(sections, [("Engines", 0, 1), ("Entropy", 2, 2)]);

        let rows: Vec<Hit> = h
            .get(&t, "/api/context/documents/carnot/passages")
            .query(&[("from", "1"), ("to", "50")])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].section_path, "Engines");

        let (status, _) = api_error(
            h.get(&t, "/api/context/documents/nope/outline")
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, 404);
        let d = document(
            "busy",
            "busy.md",
            3,
            DocumentStatus::Indexing { done: 0, total: 4 },
        );
        h.state.store.put(db::DOCUMENT, "busy", &d).await.unwrap();
        let (status, e) = api_error(
            h.get(&t, "/api/context/documents/busy/passages")
                .query(&[("from", "0"), ("to", "1")])
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!((status, e.code.as_str()), (409, "not_ready"));

        let changes: ContextChanges = h
            .get(&t, "/api/context/changes")
            .query(&[("since", "1")])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(changes.generation, 3);
        let ids: Vec<&str> = changes.changed.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["bio", "busy"]);

        let (status, e) = api_error(
            h.post(&t, "/api/context/ingest")
                .json(&json!({"name": "x.md", "origin": {"origin": "source", "source_id": "s"}, "text": "hi"}))
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!((status, e.code.as_str()), (400, "bad_origin"));

        // Deleting an agent's document: its passages go and the feed says so.
        use axum::extract::{Path, State};
        use margin_service::context_api::delete_document;
        let deleted = delete_document(State(h.state.clone()), Path("carnot".into()))
            .await
            .unwrap();
        assert_eq!(deleted, axum::http::StatusCode::NO_CONTENT);
        let left: Vec<serde_json::Value> = h
            .state
            .store
            .query_rows(
                "SELECT id FROM passage WHERE document_id = 'carnot'",
                vec![],
            )
            .await
            .unwrap();
        assert!(left.is_empty());
        let changes: ContextChanges = h
            .get(&t, "/api/context/changes")
            .query(&[("since", "3")])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(changes.deleted, ["carnot"]);
        assert_eq!(changes.generation, 4);
        assert!(changes.changed.is_empty());
        let err = delete_document(State(h.state.clone()), Path("carnot".into()))
            .await
            .unwrap_err();
        assert_eq!(err.code, "not_found");
        let mut owned = document("mine", "mine.md", 5, DocumentStatus::Ready { passages: 0 });
        owned.origin = Origin::Source {
            source_id: "s".into(),
        };
        h.state
            .store
            .put(db::DOCUMENT, "mine", &owned)
            .await
            .unwrap();
        let err = delete_document(State(h.state.clone()), Path("mine".into()))
            .await
            .unwrap_err();
        assert_eq!((err.status.as_u16(), err.code), (403, "forbidden"));
    }

    fn ctx(chat_id: &str) -> ToolContext {
        ToolContext {
            chat_id: chat_id.into(),
            turn_id: "turn".into(),
            call_id: "call".into(),
            origin_device: DeviceRef {
                device_id: "d".into(),
                name: "Mac".into(),
            },
            workspaces: Vec::new(),
            devices: Arc::new(NoDevices),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_search_tool_family_reads_retrieval() {
        let h = start().await;
        let t = h.sign_in("Mac").await;
        h.configure_ai(&t).await;
        let topic = seeded(&h, &t).await;
        let chat: Chat = serde_json::from_value(json!({
            "id": "chat1", "title": "c", "state": {"state": "idle"},
            "created_at": Utc::now(), "updated_at": Utc::now(),
        }))
        .unwrap();
        h.state.store.put(db::CHAT, "chat1", &chat).await.unwrap();
        let registry = margin_service::agent_tools::registry(&h.state);
        assert!(
            registry.get("search_sources").is_none(),
            "replaced by search"
        );
        let ctx = ctx("chat1");
        let run = |name: &'static str, args: Value| {
            let tool = registry.get(name).unwrap().clone();
            let ctx = ctx.clone();
            async move {
                let out = tool.run(&ctx, args).await;
                let v: Value = if out.ok {
                    serde_json::from_str(&out.output).unwrap()
                } else {
                    json!({"error": out.summary})
                };
                (out, v)
            }
        };

        // A chat with nothing attached and no topic has nothing to search by default.
        let (out, _) = run(
            "search",
            json!({"query": "carnot", "scope": null, "k": null, "modalities": null}),
        )
        .await;
        assert!(!out.ok);
        assert!(out.summary.starts_with("There are no sources to search"));

        let (out, v) = run(
            "search",
            json!({"query": "Carnot engine efficiency", "scope": {"topic_id": topic.id, "source_ids": null, "workspace_ids": null, "chat_files": null, "library": true}, "k": 4, "modalities": null}),
        )
        .await;
        assert!(out.ok, "{}", out.output);
        assert!(out.output.len() <= margin_service::agent_tools::RETRIEVAL_OUTPUT_MAX_BYTES);
        assert_eq!(v["passages"][0]["n"], 1);
        assert_eq!(v["passages"][0]["document_id"], "carnot");
        assert_eq!(v["passages"][0]["source"], "carnot.md");
        assert_eq!(v["passages"][0]["section"], "Engines");
        assert!(v.get("library").is_none() || v["library"].is_array());
        match out.cards.as_slice() {
            [Card::Passages(card)] => {
                assert_eq!(card.passages.len(), v["passages"].as_array().unwrap().len());
                assert_eq!(card.passages[0].document_id.as_deref(), Some("carnot"));
                assert_eq!(card.passages[0].start_line, Some(1));
            }
            other => panic!("{other:?}"),
        }

        let (out, v) = run(
            "read_passage",
            json!({"document_id": "carnot", "page": null, "from_ord": 1, "to_ord": null}),
        )
        .await;
        assert!(out.ok);
        assert_eq!(v["passages"].as_array().unwrap().len(), 2);
        assert_eq!(v["passages"][0]["ord"], 1);

        let (out, v) = run("document_outline", json!({"document_id": "carnot"})).await;
        assert_eq!(out.summary, "2 sections");
        assert_eq!(v["sections"][1]["section"], "Entropy");

        let (out, _) = run(
            "view_picture",
            json!({"document_id": "carnot", "page": null}),
        )
        .await;
        assert!(!out.ok);
        assert_eq!(out.summary, "That document has no picture.");
        let (out, _) = run(
            "read_passage",
            json!({"document_id": "nope", "page": null, "from_ord": null, "to_ord": null}),
        )
        .await;
        assert!(!out.ok);
    }
}
