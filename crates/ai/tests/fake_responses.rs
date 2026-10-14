//! margin-ai against a local fake Responses API (axum). No real network.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use futures_util::StreamExt;
use margin_ai::{
    AiError, FailedScene, LessonContext, ResponsesClient, SceneRequest, propose_outline,
    write_lesson, write_scene,
};
use margin_shared::{AskRequest, Level, OutlineLesson};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// The fake server
// ---------------------------------------------------------------------------

enum Reply {
    /// An SSE body sent as these exact byte chunks, a few milliseconds apart so they
    /// reach the client as separate reads.
    Sse(Vec<Vec<u8>>),
    Json(StatusCode, Value),
    Text(StatusCode, String),
}

#[derive(Default)]
struct Fake {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<(Option<String>, Value)>>,
}

async fn responses(State(fake): State<Arc<Fake>>, headers: HeaderMap, body: Bytes) -> Response {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let json: Value = serde_json::from_slice(&body).expect("the client sends JSON");
    fake.requests.lock().unwrap().push((auth, json));
    let reply = fake
        .replies
        .lock()
        .unwrap()
        .pop_front()
        .expect("a reply was queued for this request");
    match reply {
        Reply::Sse(chunks) => {
            let stream = futures_util::stream::iter(chunks).then(|c| async move {
                tokio::time::sleep(Duration::from_millis(1)).await;
                Ok::<_, Infallible>(Bytes::from(c))
            });
            Response::builder()
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }
        Reply::Json(status, v) => (status, axum::Json(v)).into_response(),
        Reply::Text(status, t) => (status, t).into_response(),
    }
}

async fn spawn(replies: Vec<Reply>) -> (ResponsesClient, Arc<Fake>) {
    let fake = Arc::new(Fake {
        replies: Mutex::new(replies.into()),
        ..Default::default()
    });
    let app = Router::new()
        .route("/v1/responses", post(responses))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = ResponsesClient::new(format!("http://{addr}/v1/"), "test-key", "test-model");
    (client, fake)
}

fn last_request(fake: &Fake) -> (Option<String>, Value) {
    fake.requests
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("a request was made")
}

// ---------------------------------------------------------------------------
// SSE helpers
// ---------------------------------------------------------------------------

fn event(v: Value) -> String {
    format!("event: {}\ndata: {}\n\n", v["type"].as_str().unwrap(), v)
}

fn delta(text: &str) -> String {
    event(
        json!({"type": "response.output_text.delta", "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": text}),
    )
}

fn created() -> String {
    event(
        json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress", "output": []}}),
    )
}

fn completed(text: &str) -> String {
    event(json!({"type": "response.completed", "response": {
        "id": "resp_1", "status": "completed",
        "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text, "annotations": []}]}]
    }}))
}

/// The SSE body for a stream of `deltas`, then `response.completed`.
fn text_stream(deltas: &[&str]) -> String {
    let mut s = created();
    for d in deltas {
        s.push_str(&delta(d));
    }
    s.push_str(&completed(&deltas.concat()));
    s
}

/// Splits `body` into chunks of `size` bytes: cuts land mid-line, mid-field and, for
/// non-ASCII text, mid-character.
fn chunked(body: &str, size: usize) -> Reply {
    Reply::Sse(body.as_bytes().chunks(size).map(<[u8]>::to_vec).collect())
}

fn whole(body: String) -> Reply {
    Reply::Sse(vec![body.into_bytes()])
}

fn message_response(text: &str) -> Value {
    json!({
        "id": "resp_1", "object": "response", "status": "completed",
        "output": [
            {"type": "reasoning", "id": "rs_1", "summary": []},
            {"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
             "content": [{"type": "output_text", "text": text, "annotations": []}]}
        ]
    })
}

async fn collect(client: &ResponsesClient) -> (Result<String, AiError>, Vec<String>) {
    let mut deltas = Vec::new();
    let r = client
        .stream_text("Be brief.", "Say hello.", |d| deltas.push(d.to_string()))
        .await;
    (r, deltas)
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

#[tokio::test]
async fn streams_deltas_split_across_chunks() {
    let pieces = ["Héllo", ", wörld", " — ∫ x dx", "!"];
    let body = text_stream(&pieces);
    for size in [1, 3, 7, 64] {
        let (client, fake) = spawn(vec![chunked(&body, size)]).await;
        let (r, deltas) = collect(&client).await;
        assert_eq!(r.unwrap(), "Héllo, wörld — ∫ x dx!", "chunk size {size}");
        assert_eq!(deltas, pieces, "chunk size {size}");

        let (auth, req) = last_request(&fake);
        assert_eq!(auth.as_deref(), Some("Bearer test-key"));
        assert_eq!(req["model"], "test-model");
        assert_eq!(req["instructions"], "Be brief.");
        assert_eq!(req["input"], "Say hello.");
        assert_eq!(req["stream"], true);
        assert_eq!(req["store"], false);
        assert!(req.get("text").is_none());
    }
}

#[tokio::test]
async fn handles_crlf_comments_and_multi_line_data() {
    // The JSON of the second delta is spread over three `data:` lines, which the
    // parser joins with newlines (valid JSON whitespace).
    let body = format!(
        ": keep-alive\r\n\r\n{}event: response.output_text.delta\r\ndata: {{\"type\":\r\ndata: \"response.output_text.delta\",\r\ndata: \"delta\": \" two\"}}\r\n\r\n{}",
        delta("one").replace('\n', "\r\n"),
        completed("one two").replace('\n', "\r\n"),
    );
    let (client, _) = spawn(vec![chunked(&body, 5)]).await;
    let (r, deltas) = collect(&client).await;
    assert_eq!(r.unwrap(), "one two");
    assert_eq!(deltas, ["one", " two"]);
}

#[tokio::test]
async fn completed_without_deltas_uses_the_final_response() {
    let body = format!("{}{}", created(), completed("all at once"));
    let (client, _) = spawn(vec![whole(body)]).await;
    let (r, deltas) = collect(&client).await;
    assert_eq!(r.unwrap(), "all at once");
    assert_eq!(deltas, ["all at once"]);
}

#[tokio::test]
async fn incomplete_stream_is_incomplete() {
    let body = format!(
        "{}{}{}",
        created(),
        delta("partial"),
        event(json!({"type": "response.incomplete", "response": {
            "id": "resp_1", "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"}, "output": []
        }}))
    );
    let (client, _) = spawn(vec![whole(body)]).await;
    let (r, deltas) = collect(&client).await;
    assert_eq!(deltas, ["partial"]);
    match r {
        Err(AiError::Incomplete(m)) => assert!(m.contains("output token limit"), "{m}"),
        other => panic!("expected Incomplete, got {other:?}"),
    }
}

#[tokio::test]
async fn failed_stream_carries_the_error_message() {
    let body = format!(
        "{}{}",
        created(),
        event(json!({"type": "response.failed", "response": {
            "id": "resp_1", "status": "failed",
            "error": {"code": "server_error", "message": "The model crashed."}, "output": []
        }}))
    );
    let (client, _) = spawn(vec![whole(body)]).await;
    match collect(&client).await.0 {
        Err(AiError::Incomplete(m)) => assert!(m.contains("The model crashed."), "{m}"),
        other => panic!("expected Incomplete, got {other:?}"),
    }
}

#[tokio::test]
async fn error_event_ends_the_stream() {
    let body = format!(
        "{}{}",
        created(),
        event(
            json!({"type": "error", "code": "rate_limit_exceeded", "message": "Slow down.", "param": null, "sequence_number": 2})
        )
    );
    let (client, _) = spawn(vec![whole(body)]).await;
    match collect(&client).await.0 {
        Err(AiError::Incomplete(m)) => assert!(m.contains("Slow down."), "{m}"),
        other => panic!("expected Incomplete, got {other:?}"),
    }
}

#[tokio::test]
async fn refusal_deltas_are_incomplete() {
    let body = format!(
        "{}{}{}{}",
        created(),
        event(
            json!({"type": "response.refusal.delta", "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": "I can't "})
        ),
        event(
            json!({"type": "response.refusal.delta", "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": "help with that."})
        ),
        event(
            json!({"type": "response.refusal.done", "item_id": "msg_1", "output_index": 0, "content_index": 0, "refusal": "I can't help with that."})
        ),
    );
    let (client, _) = spawn(vec![whole(body)]).await;
    match collect(&client).await.0 {
        Err(AiError::Incomplete(m)) => assert!(m.contains("I can't help with that."), "{m}"),
        other => panic!("expected Incomplete, got {other:?}"),
    }
}

#[tokio::test]
async fn refusal_then_completed_is_incomplete() {
    let body = format!(
        "{}{}{}",
        created(),
        event(json!({"type": "response.refusal.delta", "delta": "No."})),
        completed(""),
    );
    let (client, _) = spawn(vec![whole(body)]).await;
    assert!(matches!(collect(&client).await.0, Err(AiError::Incomplete(m)) if m.contains("No.")));
}

#[tokio::test]
async fn stream_closed_early_is_a_network_error() {
    let body = format!("{}{}", created(), delta("half"));
    let (client, _) = spawn(vec![whole(body)]).await;
    assert!(matches!(collect(&client).await.0, Err(AiError::Network(_))));
}

#[tokio::test]
async fn error_status_with_json_body_is_http() {
    let (client, _) = spawn(vec![Reply::Json(
        StatusCode::UNAUTHORIZED,
        json!({"error": {"message": "Incorrect API key provided.", "type": "invalid_request_error", "code": "invalid_api_key", "param": null}}),
    )])
    .await;
    match collect(&client).await.0 {
        Err(AiError::Http { status, message }) => {
            assert_eq!(status, 401);
            assert_eq!(message, "Incorrect API key provided.");
        }
        other => panic!("expected Http, got {other:?}"),
    }
}

#[tokio::test]
async fn error_status_with_text_body_is_http() {
    let (client, _) = spawn(vec![Reply::Text(
        StatusCode::BAD_GATEWAY,
        "upstream down".into(),
    )])
    .await;
    match collect(&client).await.0 {
        Err(AiError::Http { status, message }) => {
            assert_eq!(status, 502);
            assert_eq!(message, "upstream down");
        }
        other => panic!("expected Http, got {other:?}"),
    }
}

#[tokio::test]
async fn unreachable_server_is_a_network_error() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let client = ResponsesClient::new(format!("http://{addr}/v1"), "k", "m");
    assert!(matches!(collect(&client).await.0, Err(AiError::Network(_))));
}

// ---------------------------------------------------------------------------
// Structured output and the outline
// ---------------------------------------------------------------------------

fn ask(lesson_count: u32) -> AskRequest {
    AskRequest {
        prompt: "I want to understand Fourier series".into(),
        lesson_count,
        level: Level::Intermediate,
        with_videos: false,
        with_quiz: true,
        parent_topic_id: None,
        source_ids: vec![],
    }
}

fn outline_json(n: usize) -> String {
    json!({
        "topic_title": "Fourier series",
        "subject_title": "Signals & Systems",
        "lessons": (0..n).map(|i| json!({
            "title": format!("Lesson {}", i + 1),
            "summary": "What it covers.",
            "minutes": 9 + i,
        })).collect::<Vec<_>>(),
    })
    .to_string()
}

#[tokio::test]
async fn propose_outline_parses_structured_output() {
    let (client, fake) = spawn(vec![Reply::Json(
        StatusCode::OK,
        message_response(&outline_json(3)),
    )])
    .await;
    let subjects = vec![
        "Signals & Systems".to_string(),
        "Linear Algebra".to_string(),
    ];
    let outline = propose_outline(&client, &ask(3), &subjects).await.unwrap();
    assert_eq!(outline.topic_title, "Fourier series");
    assert_eq!(outline.subject_title, "Signals & Systems");
    assert_eq!(outline.lessons.len(), 3);
    assert_eq!(outline.lessons[2].minutes, 11);

    let (auth, req) = last_request(&fake);
    assert_eq!(auth.as_deref(), Some("Bearer test-key"));
    assert_eq!(req["stream"], false);
    assert_eq!(req["store"], false);
    let format = &req["text"]["format"];
    assert_eq!(format["type"], "json_schema");
    assert_eq!(format["strict"], true);
    assert_eq!(format["name"], "course_outline");
    assert_eq!(format["schema"]["additionalProperties"], false);
    assert_eq!(
        format["schema"]["properties"]["lessons"]["items"]["required"],
        json!(["title", "summary", "minutes"])
    );
    let input = req["input"].as_str().unwrap();
    assert!(input.contains("Fourier series"));
    assert!(input.contains("- Linear Algebra"));
    assert!(
        req["instructions"]
            .as_str()
            .unwrap()
            .contains("exactly 3 lessons")
    );
}

#[tokio::test]
async fn propose_outline_rejects_the_wrong_lesson_count() {
    let (client, _) = spawn(vec![Reply::Json(
        StatusCode::OK,
        message_response(&outline_json(4)),
    )])
    .await;
    match propose_outline(&client, &ask(3), &[]).await {
        Err(AiError::Parse(m)) => assert!(m.contains("asked for 3 lessons"), "{m}"),
        other => panic!("expected Parse, got {other:?}"),
    }
}

#[tokio::test]
async fn propose_outline_rejects_empty_fields() {
    let bad = json!({"topic_title": "", "subject_title": "S", "lessons": [{"title": "t", "summary": "s", "minutes": 5}]});
    let (client, _) = spawn(vec![Reply::Json(
        StatusCode::OK,
        message_response(&bad.to_string()),
    )])
    .await;
    assert!(matches!(
        propose_outline(&client, &ask(1), &[]).await,
        Err(AiError::Parse(_))
    ));
}

#[tokio::test]
async fn structured_output_that_violates_the_schema_is_parse() {
    // Wrong types, and plain prose instead of JSON.
    let wrong_type = json!({"topic_title": 7, "subject_title": "S", "lessons": []}).to_string();
    let missing = json!({"topic_title": "T", "lessons": []}).to_string();
    let (client, _) = spawn(vec![
        Reply::Json(StatusCode::OK, message_response(&wrong_type)),
        Reply::Json(StatusCode::OK, message_response(&missing)),
        Reply::Json(
            StatusCode::OK,
            message_response("Sure! Here is an outline."),
        ),
        Reply::Text(StatusCode::OK, "not json at all".into()),
    ])
    .await;
    for _ in 0..4 {
        match propose_outline(&client, &ask(1), &[]).await {
            Err(AiError::Parse(_)) => {}
            other => panic!("expected Parse, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn structured_refusal_and_incomplete_are_incomplete() {
    let refusal = json!({"status": "completed", "output": [{"type": "message", "content": [{"type": "refusal", "refusal": "I won't."}]}]});
    let incomplete = json!({"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}, "output": []});
    let failed = json!({"status": "failed", "error": {"code": "server_error", "message": "Boom."}, "output": []});
    let (client, _) = spawn(vec![
        Reply::Json(StatusCode::OK, refusal),
        Reply::Json(StatusCode::OK, incomplete),
        Reply::Json(StatusCode::OK, failed),
    ])
    .await;
    for expected in ["I won't.", "token limit", "Boom."] {
        match propose_outline(&client, &ask(1), &[]).await {
            Err(AiError::Incomplete(m)) => assert!(m.contains(expected), "{m}"),
            other => panic!("expected Incomplete, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn structured_error_status_is_http() {
    let (client, _) = spawn(vec![Reply::Json(
        StatusCode::BAD_REQUEST,
        json!({"error": {"message": "Invalid schema for response_format 'course_outline'.", "type": "invalid_request_error", "code": null}}),
    )])
    .await;
    match propose_outline(&client, &ask(1), &[]).await {
        Err(AiError::Http {
            status: 400,
            message,
        }) => assert!(message.contains("Invalid schema")),
        other => panic!("expected Http, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Lessons and scenes
// ---------------------------------------------------------------------------

fn lesson_ctx(index: usize) -> LessonContext {
    LessonContext {
        subject_title: "Signals & Systems".into(),
        topic_title: "Fourier series".into(),
        prompt: "Fourier series".into(),
        level: Level::Intro,
        with_quiz: true,
        outline: vec![
            OutlineLesson {
                title: "Waves as sums".into(),
                summary: "Why sums of sines.".into(),
                minutes: 8,
            },
            OutlineLesson {
                title: "Coefficients".into(),
                summary: "Computing them.".into(),
                minutes: 12,
            },
        ],
        index,
    }
}

#[tokio::test]
async fn write_lesson_streams_and_cleans_the_body() {
    let body = text_stream(&[
        "```markdown\n# Coefficients\n\n",
        "## Idea\n\nInline $a_n$.\n",
        "```",
    ]);
    let (client, fake) = spawn(vec![chunked(&body, 11)]).await;
    let mut deltas = String::new();
    let text = write_lesson(&client, &lesson_ctx(1), |d| deltas.push_str(d))
        .await
        .unwrap();
    assert_eq!(text, "## Idea\n\nInline $a_n$.\n");
    assert!(deltas.starts_with("```markdown"));

    let (_, req) = last_request(&fake);
    assert_eq!(req["stream"], true);
    let ins = req["instructions"].as_str().unwrap();
    assert!(ins.contains("## Check yourself"));
    assert!(ins.contains("> **Key idea.**"));
    let input = req["input"].as_str().unwrap();
    assert!(input.contains("Write lesson 2 of 2: \"Coefficients\""));
    assert!(input.contains("\"Waves as sums\""));
}

#[tokio::test]
async fn write_lesson_rejects_an_index_outside_the_outline() {
    let (client, fake) = spawn(vec![]).await;
    assert!(matches!(
        write_lesson(&client, &lesson_ctx(2), |_| {}).await,
        Err(AiError::Parse(_))
    ));
    assert!(fake.requests.lock().unwrap().is_empty());
}

const SCENE: &str = "import manimgx as m\n\n\nclass LessonScene(m.Scene):\n    def construct(self) -> None:\n        self.say(\"Hello [there].\", m.Write(m.Text(\"Hi\")))\n";

#[tokio::test]
async fn write_scene_strips_fences() {
    let fenced = format!("```python\n{SCENE}```\n");
    let (mid, rest) = fenced.split_at(20);
    let (client, fake) = spawn(vec![
        chunked(&text_stream(&[mid, rest]), 9),
        whole(text_stream(&[&format!(
            "Here you go:\n\n```python\n{SCENE}```\nEnjoy."
        )])),
        whole(text_stream(&[SCENE])),
    ])
    .await;
    let req = SceneRequest {
        lesson_title: "Greeting".into(),
        lesson_body_md: "## Hello\n\nSay hi.".into(),
        previous_attempt: None,
    };
    for _ in 0..3 {
        assert_eq!(write_scene(&client, &req).await.unwrap(), SCENE);
    }
    let (_, sent) = last_request(&fake);
    let ins = sent["instructions"].as_str().unwrap();
    assert!(ins.contains("class LessonScene(m.Scene)"));
    assert!(ins.contains("self.say"));
    assert!(ins.contains("x in [-7, 7], y in [-4, 4]"));
    assert!(
        sent["input"]
            .as_str()
            .unwrap()
            .contains("Lesson title: Greeting")
    );
}

#[tokio::test]
async fn write_scene_sends_the_failed_attempt_for_repair() {
    let (client, fake) = spawn(vec![whole(text_stream(&[SCENE]))]).await;
    let req = SceneRequest {
        lesson_title: "Greeting".into(),
        lesson_body_md: "Say hi.".into(),
        previous_attempt: Some(FailedScene {
            code: "import manimgx as m\nm.ShowCreation".into(),
            error: "AttributeError: ShowCreation is Create in ManimGX".into(),
        }),
    };
    assert_eq!(write_scene(&client, &req).await.unwrap(), SCENE);
    let input = last_request(&fake).1["input"].as_str().unwrap().to_string();
    assert!(input.contains("Repair it"));
    assert!(input.contains("ShowCreation is Create in ManimGX"));
    assert!(input.contains("m.ShowCreation"));
}

#[tokio::test]
async fn write_scene_without_lesson_scene_is_parse() {
    let (client, _) = spawn(vec![whole(text_stream(&[
        "import manimgx as m\n\nclass Intro(m.Scene):\n    pass\n",
    ]))])
    .await;
    let req = SceneRequest {
        lesson_title: "T".into(),
        lesson_body_md: "B".into(),
        previous_attempt: None,
    };
    assert!(matches!(
        write_scene(&client, &req).await,
        Err(AiError::Parse(_))
    ));
}
