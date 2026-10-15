//! Job calls are retried (`docs/phase5-brief.md` D4, §5 "Job retries"): a lesson whose
//! first call is rate limited (OpenAI, `retry-after`) or overloaded (Claude, 529)
//! waits, shows the wait in its progress text and is written; every attempt is its own
//! `model_call` row. A limit waiting doesn't cure (Claude's spend limit) fails the
//! lesson at once. A title call that keeps hitting rate limits leaves the first-line
//! title and the turn alone. Fake providers only.

mod common;

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use common::*;
use margin_agent::RetryPolicy;
use margin_agent::scripted::{Scripted, text_reply};
use margin_ai::AiError;
use margin_service::ask::{NewLesson, append_lesson};
use margin_service::db;
use margin_service::providers::Endpoints;
use margin_service::secrets::{GITHUB_CLIENT_SECRET, MemoryStore};
use margin_service::state::StoredLesson;
use margin_shared::context::{TraceKind, TraceRow};
use margin_shared::pipeline::Route;
use margin_shared::{
    ChatDetail, ChatState, Event, Lesson, LessonStatus, Level, TitleSource, Topic, routes,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;

// ---------------------------------------------------------------------------
// One fake for both APIs: Responses (`/v1/responses`) and Messages (`/v1/messages`)
// ---------------------------------------------------------------------------

enum Reply {
    /// A status, headers and a JSON error body.
    Error(u16, Vec<(&'static str, String)>, Value),
    /// A streamed answer.
    Sse(String),
}

#[derive(Default)]
struct Fake {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<Value>>,
}

impl Fake {
    fn push(&self, reply: Reply) {
        self.replies.lock().unwrap().push_back(reply);
    }

    fn requests(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

async fn answer(State(fake): State<Arc<Fake>>, Json(body): Json<Value>) -> Response {
    fake.requests.lock().unwrap().push(body);
    match fake.replies.lock().unwrap().pop_front() {
        Some(Reply::Error(status, headers, body)) => {
            let mut resp = (StatusCode::from_u16(status).unwrap(), Json(body)).into_response();
            for (name, value) in headers {
                resp.headers_mut().insert(name, value.parse().unwrap());
            }
            resp
        }
        Some(Reply::Sse(body)) => {
            ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
        }
        None => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"message": "no reply scripted"}})),
        )
            .into_response(),
    }
}

async fn claude_models() -> Json<Value> {
    let data: Vec<Value> = ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"]
        .iter()
        .map(|id| {
            json!({"type": "model", "id": id, "display_name": id,
                "max_input_tokens": 200_000, "max_tokens": 64_000})
        })
        .collect();
    Json(json!({"data": data, "has_more": false}))
}

/// The fake's base URL (no path) and its state.
async fn start_fake() -> (String, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    let app = Router::new()
        .route("/v1/responses", post(answer))
        .route("/v1/messages", post(answer))
        .route("/v1/models", get(claude_models))
        .with_state(fake.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, fake)
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|v| format!("event: {}\ndata: {}\n\n", v["type"].as_str().unwrap(), v))
        .collect()
}

/// A Responses API answer streaming `pieces`.
fn openai_text(pieces: &[&str]) -> Reply {
    let mut events = vec![
        json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress", "output": []}}),
    ];
    for p in pieces {
        events.push(
            json!({"type": "response.output_text.delta", "item_id": "msg_1",
            "output_index": 0, "content_index": 0, "delta": p}),
        );
    }
    events.push(json!({"type": "response.completed", "response": {
        "id": "resp_1", "status": "completed", "model": "test-model",
        "output": [{"type": "message", "role": "assistant", "content": [
            {"type": "output_text", "text": pieces.concat(), "annotations": []}]}],
        "usage": {"input_tokens": 100, "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": 20, "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 120}
    }}));
    Reply::Sse(sse(&events))
}

/// A Messages API answer streaming `pieces`.
fn claude_text(pieces: &[&str]) -> Reply {
    let mut events = vec![
        json!({"type": "message_start", "message": {
            "id": "msg_x", "type": "message", "role": "assistant", "model": "claude-sonnet-5-5",
            "content": [], "stop_reason": null,
            "usage": {"input_tokens": 100, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
    ];
    for p in pieces {
        events.push(json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": p}}));
    }
    events.push(json!({"type": "content_block_stop", "index": 0}));
    events.push(json!({"type": "message_delta",
        "delta": {"stop_reason": "end_turn", "stop_sequence": null}, "usage": {"output_tokens": 20}}));
    events.push(json!({"type": "message_stop"}));
    Reply::Sse(sse(&events))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A service whose lesson jobs call the fake through the real clients, retrying with
/// no computed pauses (a `retry-after` is still waited).
async fn service() -> (Harness, String) {
    let h = start_with(Setup {
        secrets: MemoryStore::new().with(GITHUB_CLIENT_SECRET, CLIENT_SECRET),
        ..Setup::default()
    })
    .await;
    h.generator.jobs_over_http.store(true, Ordering::SeqCst);
    *h.state.turns.retry.lock().unwrap() = RetryPolicy::immediate();
    let t = h.sign_in("Mac").await;
    (h, t)
}

async fn use_openai(h: &Harness, t: &str, fake: &str) {
    h.configure_ai(t).await;
    let r = h
        .patch(t, "/api/settings")
        .json(&json!({"openai_base_url": format!("{fake}/v1")}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
}

async fn use_claude(h: &Harness, t: &str, fake: &str) {
    h.state.providers.set_endpoints(Endpoints {
        anthropic: fake.to_string(),
        ..Endpoints::default()
    });
    let r = h
        .patch(t, "/api/settings")
        .json(&json!({"anthropic_api_key": CLAUDE_KEY, "default_provider": "claude"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
}

/// Appends a lesson to a new topic (its job starts at once).
async fn new_lesson(h: &Harness, t: &str) -> Lesson {
    let topic: Topic = h
        .post(t, "/api/topics")
        .json(&json!({"parent_id": null, "title": "Fourier series"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let owner = margin_service::principal::principal_for_device_token(&h.state, t)
        .await
        .unwrap()
        .expect("the owner's device token");
    append_lesson(
        &h.state,
        &owner,
        NewLesson {
            topic_id: topic.id,
            title: "Coefficients".into(),
            summary: "Computing them.".into(),
            minutes: 8,
            level: Level::Intro,
            with_quiz: false,
            workspace_ids: Vec::new(),
        },
    )
    .await
    .unwrap()
}

/// The lesson once its job has ended.
async fn finished(h: &Harness, id: &str) -> Lesson {
    eventually(|| async {
        let row = h
            .state
            .store
            .get::<StoredLesson>(db::LESSON, id)
            .await
            .unwrap()?;
        matches!(
            row.lesson.status,
            LessonStatus::Ready | LessonStatus::Failed { .. }
        )
        .then_some(row.lesson)
    })
    .await
}

/// The `model_call` rows under `trace_id`, once `n` are written.
async fn model_calls(h: &Harness, t: &str, trace_id: &str, n: usize) -> Vec<TraceRow> {
    eventually(|| async {
        let r = h
            .get(t, routes::TRACES)
            .query(&[("trace_id", trace_id)])
            .send()
            .await
            .unwrap();
        let rows: Vec<TraceRow> = r
            .text()
            .await
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .filter(|r: &TraceRow| r.kind == TraceKind::ModelCall)
            .collect();
        (rows.len() >= n).then_some(rows)
    })
    .await
}

/// The bodies the lesson was shown with, in order (`LessonChanged`).
fn shown_bodies(
    events: &mut tokio::sync::broadcast::Receiver<margin_service::state::Emitted>,
    id: &str,
) -> Vec<String> {
    let mut bodies = Vec::new();
    while let Ok(emitted) = events.try_recv() {
        if let Event::LessonChanged { lesson } = emitted.event
            && lesson.id == id
        {
            bodies.push(lesson.body_md);
        }
    }
    bodies
}

const WAITING: &str = "Waiting for the model until ";

// ---------------------------------------------------------------------------
// Lessons
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_rate_limited_lesson_waits_shows_it_and_is_written() {
    let (fake_url, fake) = start_fake().await;
    let (h, t) = service().await;
    use_openai(&h, &t, &fake_url).await;
    fake.push(Reply::Error(
        429,
        vec![("retry-after", "1".into())],
        json!({"error": {"message": "Rate limit reached.", "type": "requests", "code": "rate_limit_exceeded"}}),
    ));
    fake.push(openai_text(&["## Idea\n\n", "Sums of waves.\n"]));
    let mut events = h.state.events.subscribe();
    let started = std::time::Instant::now();
    let lesson = new_lesson(&h, &t).await;
    let done = finished(&h, &lesson.id).await;
    assert_eq!(done.status, LessonStatus::Ready, "{done:?}");
    assert!(done.body_md.contains("Sums of waves."));
    // The `retry-after` was waited in full.
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert_eq!(fake.requests(), 2);

    // The progress text carried the wait, then went back to the (empty) body before
    // the text streamed.
    let bodies = shown_bodies(&mut events, &lesson.id);
    let at = bodies
        .iter()
        .position(|b| b.starts_with(WAITING))
        .unwrap_or_else(|| panic!("no wait was shown: {bodies:?}"));
    assert!(
        bodies[at].ends_with("UTC. The model API asked Margin to slow down."),
        "{}",
        bodies[at]
    );
    assert_eq!(bodies[at + 1], "", "{bodies:?}");
    assert!(
        bodies.last().unwrap().contains("Sums of waves."),
        "{bodies:?}"
    );

    // Two model_call rows: the rate-limited attempt and the one that wrote it.
    let rows = model_calls(&h, &t, &lesson.id, 2).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    let failed: Vec<_> = rows.iter().filter(|r| !r.ok).collect();
    assert_eq!(failed.len(), 1, "{rows:?}");
    assert_eq!(failed[0].error_code.as_deref(), Some("rate_limited"));
    assert_eq!(failed[0].job_id.as_deref(), Some(lesson.id.as_str()));
    assert_eq!(failed[0].route, Some(Route::Write));
    assert!(failed[0].usage.is_none());
    let ok = rows.iter().find(|r| r.ok).unwrap();
    assert!(ok.usage.is_some());
    assert_eq!(
        (ok.prompt_version.clone(), ok.provider.clone()),
        (failed[0].prompt_version.clone(), failed[0].provider.clone())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_overloaded_claude_lesson_is_retried_and_written() {
    let (fake_url, fake) = start_fake().await;
    let (h, t) = service().await;
    use_claude(&h, &t, &fake_url).await;
    fake.push(Reply::Error(
        529,
        vec![],
        json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
    ));
    fake.push(claude_text(&["## Idea\n\n", "Sums of waves.\n"]));
    let mut events = h.state.events.subscribe();
    let lesson = new_lesson(&h, &t).await;
    let done = finished(&h, &lesson.id).await;
    assert_eq!(done.status, LessonStatus::Ready, "{done:?}");
    assert_eq!(fake.requests(), 2);
    let bodies = shown_bodies(&mut events, &lesson.id);
    assert!(
        bodies
            .iter()
            .any(|b| b.starts_with(WAITING) && b.ends_with("The model API had a problem.")),
        "{bodies:?}"
    );
    let rows = model_calls(&h, &t, &lesson.id, 2).await;
    let codes: Vec<_> = rows.iter().map(|r| (r.ok, r.error_code.clone())).collect();
    assert!(
        codes.contains(&(false, Some("server_error".into()))),
        "{codes:?}"
    );
    assert!(codes.contains(&(true, None)), "{codes:?}");
    assert!(rows.iter().all(|r| r.provider.as_deref() == Some("claude")));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_spend_limit_fails_the_lesson_at_once() {
    let (fake_url, fake) = start_fake().await;
    let (h, t) = service().await;
    use_claude(&h, &t, &fake_url).await;
    fake.push(Reply::Error(
        429,
        vec![("retry-after", "30".into())],
        json!({"type": "error", "error": {"type": "rate_limit_error",
            "message": "You have reached your spend limit.",
            "details": {"error_code": "enforced_spend_limit_reached"}}}),
    ));
    let mut events = h.state.events.subscribe();
    let started = std::time::Instant::now();
    let lesson = new_lesson(&h, &t).await;
    let done = finished(&h, &lesson.id).await;
    assert!(
        matches!(&done.status, LessonStatus::Failed { error } if error.contains("spend limit")),
        "{done:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(20), "no wait");
    assert_eq!(fake.requests(), 1);
    assert!(
        !shown_bodies(&mut events, &lesson.id)
            .iter()
            .any(|b| b.starts_with(WAITING))
    );
    let rows = model_calls(&h, &t, &lesson.id, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].error_code.as_deref(), Some("spend_limit_reached"));
}

// ---------------------------------------------------------------------------
// Titles
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_title_call_that_stays_rate_limited_leaves_the_first_line_and_the_turn() {
    let (h, t) = service().await;
    h.configure_ai(&t).await;
    let limited = || {
        Scripted::Fail(AiError::Http {
            status: 429,
            message: "Rate limit reached.".into(),
            retry_after: None,
            code: Some("rate_limit_exceeded".into()),
        })
    };
    // The first call and its three retries.
    for _ in 0..4 {
        h.generator.titles.push(limited());
    }
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Sums of waves."])));
    let r = h
        .post(&t, "/api/chats")
        .json(&json!({"message": {"text": "Teach me Fourier series"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let chat: ChatDetail = r.json().await.unwrap();
    let id = chat.chat.id.clone();
    eventually(|| async { (h.generator.text_calls.lock().unwrap().len() == 4).then_some(()) })
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(h.generator.text_calls.lock().unwrap().len(), 4, "no more");

    let d: ChatDetail = h
        .get(&t, &format!("/api/chats/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (d.chat.title.as_str(), d.chat.title_by),
        ("Teach me Fourier series", TitleSource::FirstLine)
    );
    assert_eq!(d.chat.state, ChatState::Idle);
    assert_eq!(d.chat.waiting, None);
    assert_eq!(d.items.len(), 2, "the answer is untouched");

    // Each attempt is its own row on the small route.
    let turn = d.items[0].turn_id.clone();
    let rows = eventually(|| async {
        let rows: Vec<TraceRow> = model_calls(&h, &t, &turn, 1)
            .await
            .into_iter()
            .filter(|r| r.route == Some(Route::Small))
            .collect();
        (rows.len() == 4).then_some(rows)
    })
    .await;
    assert!(
        rows.iter()
            .all(|r| !r.ok && r.error_code.as_deref() == Some("rate_limited"))
    );
}
