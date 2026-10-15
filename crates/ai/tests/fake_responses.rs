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
use margin_ai::evals::{self, QuestionKind, Rubric, SynthRequest, Winner};
use margin_ai::prompts::{LESSON_CONSTANT, SCENE_CONSTANT};
use margin_ai::{
    AiError, Backoff, CallOptions, DocumentContents, FailedScene, Image, LessonContext, Passage,
    PlanRequest, Ranked, ReasoningConfig, ReasoningSummary, RerankCandidate, ResponseRequest,
    ResponseStream, ResponsesClient, ResponsesToolChoice, SceneRequest, StreamEvent, plan_course,
    rerank, write_lesson, write_lesson_with, write_scene, write_scene_with,
};
use margin_shared::pipeline::Route;
use margin_shared::{Level, OutlineLesson, ReasoningEffort, Usage};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// The fake server
// ---------------------------------------------------------------------------

enum Reply {
    /// An SSE body sent as these exact byte chunks, a few milliseconds apart so they
    /// reach the client as separate reads.
    Sse(Vec<Vec<u8>>),
    /// An SSE body sent as these chunks, then the connection is cut (the body errors).
    SseThenDrop(Vec<Vec<u8>>),
    Json(StatusCode, Value),
    Text(StatusCode, String),
    /// A status with these headers and a JSON body (e.g. 429 with `Retry-After`).
    WithHeaders(StatusCode, Vec<(&'static str, String)>, Value),
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
        Reply::SseThenDrop(chunks) => {
            let stream = futures_util::stream::iter(chunks.into_iter().map(Some).chain([None]))
                .then(|c| async move {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    c.map(Bytes::from)
                        .ok_or_else(|| std::io::Error::other("connection cut"))
                });
            Response::builder()
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }
        Reply::Json(status, v) => (status, axum::Json(v)).into_response(),
        Reply::WithHeaders(status, headers, v) => {
            let mut resp = (status, axum::Json(v)).into_response();
            for (name, value) in headers {
                resp.headers_mut().insert(name, value.parse().unwrap());
            }
            resp
        }
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

async fn collect(client: &ResponsesClient) -> (Result<String, AiError>, Vec<String>) {
    let mut deltas = Vec::new();
    let r = client
        .stream_text(
            "Be brief.",
            "Say hello.",
            &CallOptions::new(Route::Small),
            |d| deltas.push(d.to_string()),
        )
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
        Err(AiError::Http {
            status, message, ..
        }) => {
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
        Err(AiError::Http {
            status, message, ..
        }) => {
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
// Lessons and scenes
// ---------------------------------------------------------------------------

fn passages() -> Vec<Passage> {
    vec![Passage {
        source: "dsp.pdf".into(),
        page: Some(12),
        text: "The coefficient a_n is the projection onto cos(n x).".into(),
        ..Default::default()
    }]
}

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
        passages: vec![],
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
    assert!(!input.contains("SOURCE PASSAGES"));
}

#[tokio::test]
async fn write_lesson_sends_source_passages() {
    let (client, fake) = spawn(vec![chunked(&text_stream(&["## Idea\n\nText.\n"]), 64)]).await;
    let mut ctx = lesson_ctx(1);
    ctx.passages = passages();
    write_lesson(&client, &ctx, |_| {}).await.unwrap();
    let (_, req) = last_request(&fake);
    let ins = req["instructions"].as_str().unwrap();
    assert!(ins.contains("Markdown footnote references"));
    let input = req["input"].as_str().unwrap();
    assert!(
        input.contains("[1] dsp.pdf, p. 12:\nThe coefficient a_n is the projection onto cos(n x).")
    );
}

/// Pictures go as `input_image` parts (base64 data URLs) inside one user message, each
/// right after its passage's heading.
#[tokio::test]
async fn write_lesson_sends_pictures_as_image_inputs() {
    let (client, fake) = spawn(vec![chunked(&text_stream(&["## Idea\n\nText.[^2]\n"]), 64)]).await;
    let mut ctx = lesson_ctx(1);
    ctx.passages = passages();
    ctx.passages.push(Passage {
        source: "board.jpg".into(),
        image: Some(Image {
            media_type: "image/jpeg".into(),
            data: vec![0xFF, 0xD8, 0xFF, 0xE0],
        }),
        ..Default::default()
    });
    let body = write_lesson(&client, &ctx, |_| {}).await.unwrap();
    assert_eq!(body, "## Idea\n\nText.[^2]\n");
    let (_, req) = last_request(&fake);
    let input = req["input"].as_array().unwrap();
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["role"], "user");
    let content = input[0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 3);
    assert_eq!(content[0]["type"], "input_text");
    let before = content[0]["text"].as_str().unwrap();
    assert!(before.contains("[1] dsp.pdf, p. 12:"));
    assert!(before.ends_with("[2] board.jpg, picture attached below:\n"));
    assert_eq!(content[1]["type"], "input_image");
    assert_eq!(content[1]["image_url"], "data:image/jpeg;base64,/9j/4A==");
    assert_eq!(content[1]["detail"], "auto");
    assert_eq!(content[2]["type"], "input_text");
    assert!(
        content[2]["text"]
            .as_str()
            .unwrap()
            .contains("=== END SOURCE PASSAGES ===")
    );
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

// ---------------------------------------------------------------------------
// Prompt caching (§2.6), call options, structured calls
// ---------------------------------------------------------------------------

/// `text_stream` whose `response.completed` carries usage.
fn text_stream_with_usage(text: &str, input: u64, cached: u64, output: u64) -> String {
    let mut s = created();
    s.push_str(&delta(text));
    s.push_str(&event(json!({"type": "response.completed", "response": {
        "id": "resp_1", "status": "completed",
        "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text, "annotations": []}]}],
        "usage": {"input_tokens": input, "input_tokens_details": {"cached_tokens": cached}, "output_tokens": output, "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": input + output}
    }})));
    s
}

fn json_reply(v: Value) -> Reply {
    whole(text_stream(&[&v.to_string()]))
}

fn all_requests(fake: &Fake) -> Vec<Value> {
    fake.requests
        .lock()
        .unwrap()
        .iter()
        .map(|(_, v)| v.clone())
        .collect()
}

/// The CI gate of §3.3: across varied lessons (level, quiz, passages, pictures, first,
/// middle and last lesson) and scenes (first try and repair), every request's
/// instructions start with its version's constant block, byte for byte, the variable
/// part comes after it and nothing else does, and the prompt cache key is the one per
/// prompt version.
#[tokio::test]
async fn lesson_and_scene_prefixes_are_byte_identical_across_calls() {
    let mut contexts = Vec::new();
    for index in 0..2 {
        for with_quiz in [false, true] {
            let mut ctx = lesson_ctx(index);
            ctx.with_quiz = with_quiz;
            ctx.level = if with_quiz {
                Level::Advanced
            } else {
                Level::Intro
            };
            if index == 1 {
                ctx.passages = passages();
            }
            contexts.push(ctx);
        }
    }
    contexts[3].passages.push(Passage {
        source: "board.jpg".into(),
        image: Some(Image {
            media_type: "image/jpeg".into(),
            data: vec![1, 2, 3],
        }),
        ..Default::default()
    });
    let scenes = [
        SceneRequest {
            lesson_title: "Waves as sums".into(),
            lesson_body_md: "## Idea\n\nSines add.\n\n## Check yourself\n\nQ".into(),
            previous_attempt: None,
        },
        SceneRequest {
            lesson_title: "Coefficients".into(),
            lesson_body_md: "## Idea\n\nProject.".into(),
            previous_attempt: Some(FailedScene {
                code: SCENE.into(),
                error: "NameError".into(),
            }),
        },
    ];
    let replies = contexts
        .iter()
        .map(|_| whole(text_stream(&["## Idea\n\nText.\n"])))
        .chain(scenes.iter().map(|_| whole(text_stream(&[SCENE]))))
        .collect();
    let (client, fake) = spawn(replies).await;
    for ctx in &contexts {
        write_lesson(&client, ctx, |_| {}).await.unwrap();
    }
    for req in &scenes {
        write_scene(&client, req).await.unwrap();
    }
    let sent = all_requests(&fake);
    assert_eq!(sent.len(), 6);
    for (i, body) in sent.iter().enumerate() {
        let ins = body["instructions"].as_str().unwrap();
        let (constant, key) = if i < 4 {
            (LESSON_CONSTANT, "lesson:lesson-v2")
        } else {
            (SCENE_CONSTANT.as_str(), "scene:scene-v2")
        };
        assert!(ins.starts_with(constant), "request {i}");
        assert_eq!(body["prompt_cache_key"], key, "request {i}");
        let rest = &ins[constant.len()..];
        if i < 4 {
            assert!(
                rest.starts_with("\n\nThis lesson:\n"),
                "request {i}: {rest}"
            );
            assert!(
                !rest.contains("Signals & Systems"),
                "the request goes in the input"
            );
        } else {
            assert_eq!(rest, "", "scenes have no variable instructions");
        }
        // Nothing but the instructions carries the constant block.
        assert!(
            !body["input"]
                .to_string()
                .contains("How the app renders your answer")
        );
    }
    // The variable parts really differ, so the identity above isn't vacuous.
    let variable =
        |i: usize| sent[i]["instructions"].as_str().unwrap()[LESSON_CONSTANT.len()..].to_string();
    assert_ne!(variable(0), variable(3));
    assert!(variable(3).contains("2 source passages"));
    // Both prefixes clear OpenAI's 1 024-token cache minimum (≈ 4 characters a token).
    assert!(LESSON_CONSTANT.len() >= 4 * 1024);
    assert!(SCENE_CONSTANT.len() >= 4 * 1024);
    // The scene input leaves the lesson's quiz out.
    assert!(
        !sent[4]["input"]
            .as_str()
            .unwrap()
            .contains("Check yourself")
    );
}

#[tokio::test]
async fn call_options_set_the_cache_key_and_output_cap_and_usage_comes_back() {
    let (client, fake) = spawn(vec![
        whole(text_stream_with_usage(
            "## Idea\n\nText.\n",
            1500,
            1200,
            300,
        )),
        whole(text_stream_with_usage(SCENE, 5000, 4096, 900)),
        chunked(&text_stream(&["Hi."]), 7),
    ])
    .await;
    let opts = CallOptions::new(Route::Write)
        .with_cache_key("lesson:lesson-v2:topic-7")
        .with_max_output_tokens(8000);
    let out = write_lesson_with(&client, &lesson_ctx(0), &opts, |_| {})
        .await
        .unwrap();
    assert_eq!(out.value, "## Idea\n\nText.\n");
    assert_eq!(
        out.usage,
        Usage {
            input_tokens: 1500,
            cached_input_tokens: 1200,
            output_tokens: 300,
            reasoning_tokens: 0,
            model_calls: 1,
            ..Default::default()
        }
    );
    let (_, body) = last_request(&fake);
    assert_eq!(body["prompt_cache_key"], "lesson:lesson-v2:topic-7");
    assert_eq!(body["max_output_tokens"], 8000);
    assert_eq!(body["store"], false);
    assert!(body.get("text").is_none());

    let req = SceneRequest {
        lesson_title: "T".into(),
        lesson_body_md: "B".into(),
        previous_attempt: None,
    };
    let scene = write_scene_with(&client, &req, &CallOptions::new(Route::Write))
        .await
        .unwrap();
    assert_eq!(scene.usage.cached_input_tokens, 4096);
    assert_eq!(last_request(&fake).1["prompt_cache_key"], "scene:scene-v2");

    // A plain call sends no key and no cap unless asked.
    let (text, _) = collect(&client).await;
    assert_eq!(text.unwrap(), "Hi.");
    let (_, body) = last_request(&fake);
    assert!(body.get("prompt_cache_key").is_none());
    assert!(body.get("max_output_tokens").is_none());
}

fn plan_request() -> PlanRequest {
    PlanRequest {
        prompt: "Fourier series, for signal processing".into(),
        level: Level::Intermediate,
        lesson_count: 2,
        with_quiz: true,
        draft: None,
        contents: vec![DocumentContents {
            document: "dsp.pdf".into(),
            sections: vec![
                "2 Fourier series > 2.1 Orthogonality".into(),
                "2 Fourier series > 2.2 Coefficients".into(),
            ],
        }],
        survey: passages(),
    }
}

#[tokio::test]
async fn plan_course_asks_for_strict_json_and_checks_the_answer() {
    let answer = json!({
        "topic_title": "Fourier series",
        "subject_title": "Signals and systems",
        "lessons": [
            {
                "title": "Orthogonality of sines and cosines",
                "summary": "Why the trigonometric functions form an orthogonal basis on [-pi, pi].",
                "minutes": 12,
                "queries": [
                    "integral of cos(n x) cos(m x) over [-pi, pi] for n != m",
                    "orthogonality relations of the trigonometric system",
                ],
                "must_cite": [
                    {"document": "dsp.pdf", "section": "2 Fourier series > 2.1 Orthogonality"},
                    {"document": "dsp.pdf", "section": "9 Wavelets"},
                ],
            },
            {
                "title": "Computing the coefficients",
                "summary": "Deriving a_n and b_n by projection.",
                "minutes": 15,
                "queries": ["coefficient a_n as projection onto cos(n x)", "b_n formula"],
                "must_cite": [{"document": "DSP.pdf", "section": "2 fourier series > 2.2 coefficients"}],
            },
        ],
    });
    // A server that wraps JSON in a fence is tolerated.
    let fenced = format!("```json\n{answer}\n```");
    let (client, fake) = spawn(vec![
        json_reply(answer.clone()),
        whole(text_stream(&[&fenced])),
        json_reply(json!({"topic_title": "x", "subject_title": "y", "lessons": []})),
        whole(text_stream(&["not json"])),
    ])
    .await;
    let opts = CallOptions::new(Route::Write);
    let plan = plan_course(&client, &plan_request(), &opts)
        .await
        .unwrap()
        .value;
    assert_eq!(plan.lessons.len(), 2);
    assert_eq!(
        plan.lessons[0].must_cite.len(),
        1,
        "an unlisted section is dropped"
    );
    assert_eq!(
        plan.lessons[1].must_cite[0].section, "2 Fourier series > 2.2 Coefficients",
        "spelled as in the contents"
    );
    assert_eq!(plan.outline().lessons[1].minutes, 15);
    let (_, body) = last_request(&fake);
    assert_eq!(body["prompt_cache_key"], "plan:plan-v1");
    assert_eq!(body["text"]["format"]["type"], "json_schema");
    assert_eq!(body["text"]["format"]["strict"], true);
    assert_eq!(body["text"]["format"]["name"], "course_plan");
    let ins = body["instructions"].as_str().unwrap();
    assert!(ins.contains("Lessons: exactly 2."));
    let input = body["input"].as_str().unwrap();
    assert!(input.contains("Contents:\n- dsp.pdf\n  - 2 Fourier series > 2.1 Orthogonality"));
    assert!(input.contains("[1] dsp.pdf, p. 12:"));

    assert_eq!(
        plan_course(&client, &plan_request(), &opts)
            .await
            .unwrap()
            .value,
        plan
    );
    assert!(matches!(
        plan_course(&client, &plan_request(), &opts).await,
        Err(AiError::Parse(m)) if m.contains("0 lessons")
    ));
    assert!(matches!(
        plan_course(&client, &plan_request(), &opts).await,
        Err(AiError::Parse(m)) if m.contains("plan answer isn't the JSON")
    ));
}

#[tokio::test]
async fn rerank_returns_every_candidate_once() {
    let (client, fake) = spawn(vec![json_reply(json!({"ranking": [
        {"id": "p2", "grade": 3},
        {"id": "invented", "grade": 3},
        {"id": "p1", "grade": 1},
    ]}))])
    .await;
    let candidates: Vec<RerankCandidate> = ["p1", "p2", "p3"]
        .iter()
        .map(|id| RerankCandidate {
            id: id.to_string(),
            label: "dsp.pdf".into(),
            text: "text".into(),
        })
        .collect();
    let opts = CallOptions::new(Route::Small);
    let ranked = rerank(&client, "why does the window leak", &candidates, &opts)
        .await
        .unwrap()
        .value;
    let pairs: Vec<(&str, u8)> = ranked
        .iter()
        .map(|Ranked { id, grade }| (id.as_str(), *grade))
        .collect();
    assert_eq!(pairs, [("p2", 3), ("p1", 1), ("p3", 0)]);
    assert_eq!(
        last_request(&fake).1["prompt_cache_key"],
        "rerank:rerank-v1"
    );
    // No candidates, no call.
    assert!(
        rerank(&client, "q", &[], &opts)
            .await
            .unwrap()
            .value
            .is_empty()
    );
    assert_eq!(all_requests(&fake).len(), 1);
}

#[tokio::test]
async fn eval_calls_generate_grade_and_judge() {
    let section = &evals::CANARY_SECTIONS[3];
    let (client, fake) = spawn(vec![
        json_reply(json!({"questions": [{
            "question": "A test has sensitivity 0.99 and a false-positive rate of 0.02 for a condition with prevalence 1 in 1,000. What fraction of positives are true positives?",
            "kind": "application",
            "answer": "About 0.047, because false positives from the healthy majority dominate.",
            "spans": [{"section": 1, "quote": "most positive results are false positives"}],
            "self_grade": 3
        }]})),
        json_reply(json!({"grade": 3, "supported": true, "kind": "application", "reason": "Needs Bayes' rule on new numbers."})),
        json_reply(json!({"grade": 0, "items": [{"quote": "P(+|not D)", "passages": [1], "grade": 0, "note": "Wrong denominator."}], "reason": "Wrong formula."})),
        json_reply(json!({"preferred": "first", "first_grade": 3, "second_grade": 1, "reason": "A is faithful."})),
        json_reply(json!({"preferred": "second", "first_grade": 1, "second_grade": 3, "reason": "A is faithful."})),
    ])
    .await;
    let small = CallOptions::new(Route::Small);
    let req = SynthRequest {
        sections: vec![section.synth()],
        count: 3,
        kinds: vec![QuestionKind::Application, QuestionKind::Derivation],
    };
    let questions = evals::synth_questions(&client, &req, &CallOptions::new(Route::Write))
        .await
        .unwrap()
        .value;
    assert_eq!(questions.len(), 1);
    assert!(questions[0].is_usable(), "{:?}", questions[0].problems);
    assert_eq!(questions[0].spans[0].document, "probability.md");

    let grade = evals::grade_question(&client, &(&questions[0]).into(), &small)
        .await
        .unwrap()
        .value;
    assert!(grade.keeps());

    let canary = &evals::BAD_LESSONS[2];
    let verdict =
        evals::judge_candidate(&client, canary.rubric, &canary.input(), canary.body, &small)
            .await
            .unwrap()
            .value;
    assert!(canary.passes(&verdict));
    assert_eq!(verdict.item_mean(), Some(0.0));

    let pair = evals::judge_pair(
        &client,
        Rubric::Faithfulness,
        &canary.input(),
        "A",
        "B",
        &small,
    )
    .await
    .unwrap();
    assert_eq!(pair.value.0.winner, Winner::A);
    assert!(pair.value.0.consistent);
    assert_eq!(pair.usage.model_calls, 2);

    let sent = all_requests(&fake);
    let keys: Vec<&str> = sent
        .iter()
        .map(|b| b["prompt_cache_key"].as_str().unwrap())
        .collect();
    assert_eq!(
        keys,
        [
            "synth:synth-v1",
            "seriousness:seriousness-v1",
            "judge_faithfulness:judge-v1",
            "pair_faithfulness:pair-v1",
            "pair_faithfulness:pair-v1",
        ]
    );
    for body in &sent {
        assert_eq!(body["text"]["format"]["strict"], true);
    }
    // The second pairwise call shows the candidates the other way round.
    let order = |b: &Value| {
        b["input"]
            .as_str()
            .unwrap()
            .contains("=== BEGIN FIRST ===\nA\n")
    };
    assert!(order(&sent[3]));
    assert!(!order(&sent[4]));
}

// ---------------------------------------------------------------------------
// The agent's streaming call (`stream_response`)
// ---------------------------------------------------------------------------

/// Every event of `stream`, in order (an `Err` ends it).
async fn drain(stream: ResponseStream) -> Vec<Result<StreamEvent, String>> {
    stream
        .map(|r| r.map_err(|e| format!("{e:?}")))
        .collect()
        .await
}

fn agent_request() -> ResponseRequest {
    ResponseRequest {
        model: Some("agent-model".into()),
        input: vec![
            json!({"role": "developer", "content": "You are Margin."}),
            json!({"role": "user", "content": "Hi"}),
        ],
        tools: vec![json!({
            "type": "function", "name": "search_library", "description": "Search.",
            "parameters": {"type": "object", "properties": {"query": {"type": "string"}},
                           "required": ["query"], "additionalProperties": false},
            "strict": true
        })],
        tool_choice: Some(ResponsesToolChoice::Auto),
        reasoning: Some(ReasoningConfig {
            effort: ReasoningEffort::Medium,
            summary: Some(ReasoningSummary::Auto),
        }),
        include_encrypted_reasoning: true,
        prompt_cache_key: Some("chat-1".into()),
        parallel_tool_calls: Some(true),
        ..Default::default()
    }
}

fn completed_with_usage(id: &str) -> String {
    event(
        json!({"type": "response.completed", "sequence_number": 99, "response": {
            "id": id, "status": "completed", "output": [],
            "usage": {
                "input_tokens": 1200, "input_tokens_details": {"cached_tokens": 1024},
                "output_tokens": 300, "output_tokens_details": {"reasoning_tokens": 200},
                "total_tokens": 1500
            }
        }}),
    )
}

fn usage_1200() -> Usage {
    Usage {
        input_tokens: 1200,
        cached_input_tokens: 1024,
        output_tokens: 300,
        reasoning_tokens: 200,
        model_calls: 1,
        ..Default::default()
    }
}

fn message_item(status: &str, text: &str) -> Value {
    let content = if status == "in_progress" {
        json!([])
    } else {
        json!([{"type": "output_text", "text": text, "annotations": []}])
    };
    json!({"id": "msg_1", "type": "message", "role": "assistant", "status": status, "content": content})
}

/// A text answer with every event the API sends around it, including those the stream
/// doesn't yield (`created`, `in_progress`, `content_part.*`, `output_text.done`).
fn agent_text_body(pieces: &[&str]) -> String {
    let text = pieces.concat();
    let mut s = created();
    s.push_str(&event(
        json!({"type": "response.in_progress", "response": {"id": "resp_1", "status": "in_progress"}}),
    ));
    s.push_str(&event(
        json!({"type": "response.output_item.added", "output_index": 0, "item": message_item("in_progress", "")}),
    ));
    s.push_str(&event(
        json!({"type": "response.content_part.added", "item_id": "msg_1", "output_index": 0, "content_index": 0, "part": {"type": "output_text", "text": ""}}),
    ));
    for p in pieces {
        s.push_str(&delta(p));
    }
    s.push_str(&event(
        json!({"type": "response.output_text.done", "item_id": "msg_1", "output_index": 0, "content_index": 0, "text": text}),
    ));
    s.push_str(&event(
        json!({"type": "response.content_part.done", "item_id": "msg_1", "output_index": 0, "content_index": 0, "part": {"type": "output_text", "text": text}}),
    ));
    s.push_str(&event(
        json!({"type": "response.output_item.done", "output_index": 0, "item": message_item("completed", &text)}),
    ));
    s.push_str(&completed_with_usage("resp_1"));
    s
}

fn text_delta_event(text: &str) -> StreamEvent {
    StreamEvent::OutputTextDelta {
        output_index: 0,
        item_id: "msg_1".into(),
        content_index: 0,
        delta: text.into(),
    }
}

#[tokio::test]
async fn stream_response_yields_a_text_answer_split_anywhere() {
    let pieces = ["Héllo", ", wörld", " — ∫ x dx", "!"];
    let body = agent_text_body(&pieces);
    let mut expected = vec![Ok(StreamEvent::OutputItemAdded {
        output_index: 0,
        item: message_item("in_progress", ""),
    })];
    expected.extend(pieces.iter().map(|p| Ok(text_delta_event(p))));
    expected.push(Ok(StreamEvent::OutputItemDone {
        output_index: 0,
        item: message_item("completed", &pieces.concat()),
    }));
    expected.push(Ok(StreamEvent::Completed {
        response_id: "resp_1".into(),
        usage: usage_1200(),
        model: String::new(),
        status: "completed".into(),
    }));
    for size in [3, 13, 4096] {
        let (client, fake) = spawn(vec![chunked(&body, size)]).await;
        let req = agent_request();
        let events = drain(client.stream_response(&req).await.unwrap()).await;
        assert_eq!(events, expected, "chunk size {size}");

        // What was sent is exactly `response_body`.
        let (auth, sent) = last_request(&fake);
        assert_eq!(auth.as_deref(), Some("Bearer test-key"));
        assert_eq!(sent, client.response_body(&req));
        assert_eq!(sent["model"], "agent-model");
        assert_eq!(sent["stream"], true);
        assert_eq!(sent["store"], false);
        assert_eq!(sent["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(
            sent["reasoning"],
            json!({"effort": "medium", "summary": "auto"})
        );
        assert_eq!(sent["tool_choice"], "auto");
        assert_eq!(sent["prompt_cache_key"], "chat-1");
        assert_eq!(sent["parallel_tool_calls"], true);
        assert_eq!(sent["tools"][0]["strict"], true);
        assert_eq!(sent["input"][0]["role"], "developer");
    }
}

fn function_call_item(id: &str, call_id: &str, name: &str, args: &str, status: &str) -> Value {
    json!({"id": id, "type": "function_call", "status": status, "call_id": call_id, "name": name, "arguments": args})
}

#[tokio::test]
async fn stream_response_yields_parallel_function_calls() {
    let calls = [
        (
            "fc_1",
            "call_a",
            "search_library",
            ["{\"query\":", "\"Fourier\"}"],
        ),
        ("fc_2", "call_b", "list_topics", ["{", "}"]),
    ];
    let mut body = created();
    let mut expected = Vec::new();
    for (i, (id, call_id, name, parts)) in calls.iter().enumerate() {
        let args = parts.concat();
        let added = function_call_item(id, call_id, name, "", "in_progress");
        let done = function_call_item(id, call_id, name, &args, "completed");
        body.push_str(&event(
            json!({"type": "response.output_item.added", "output_index": i, "item": added}),
        ));
        expected.push(Ok(StreamEvent::OutputItemAdded {
            output_index: i as u32,
            item: added,
        }));
        for p in parts {
            body.push_str(&event(
                json!({"type": "response.function_call_arguments.delta", "item_id": id, "output_index": i, "delta": p}),
            ));
            expected.push(Ok(StreamEvent::FunctionCallArgumentsDelta {
                output_index: i as u32,
                item_id: (*id).into(),
                delta: (*p).into(),
            }));
        }
        body.push_str(&event(
            json!({"type": "response.function_call_arguments.done", "item_id": id, "output_index": i, "arguments": args}),
        ));
        expected.push(Ok(StreamEvent::FunctionCallArgumentsDone {
            output_index: i as u32,
            item_id: (*id).into(),
            arguments: args.clone(),
        }));
        body.push_str(&event(
            json!({"type": "response.output_item.done", "output_index": i, "item": done}),
        ));
        expected.push(Ok(StreamEvent::OutputItemDone {
            output_index: i as u32,
            item: done,
        }));
    }
    body.push_str(&completed_with_usage("resp_tools"));
    expected.push(Ok(StreamEvent::Completed {
        response_id: "resp_tools".into(),
        usage: usage_1200(),
        model: String::new(),
        status: "completed".into(),
    }));
    let (client, _) = spawn(vec![chunked(&body, 7)]).await;
    let events = drain(client.stream_response(&agent_request()).await.unwrap()).await;
    assert_eq!(events, expected);
}

#[tokio::test]
async fn stream_response_yields_reasoning_summaries_and_keeps_encrypted_content() {
    let added = json!({"id": "rs_1", "type": "reasoning", "summary": []});
    let done = json!({
        "id": "rs_1", "type": "reasoning",
        "summary": [{"type": "summary_text", "text": "**Plan** Look it up."}],
        "encrypted_content": "gAAAAABo-ëñçrÿptëd+/=="
    });
    let body = [
        created(),
        event(json!({"type": "response.output_item.added", "output_index": 0, "item": added})),
        event(
            json!({"type": "response.reasoning_summary_part.added", "item_id": "rs_1", "output_index": 0, "summary_index": 0, "part": {"type": "summary_text", "text": ""}}),
        ),
        event(
            json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "output_index": 0, "summary_index": 0, "delta": "**Plan** "}),
        ),
        event(
            json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "output_index": 0, "summary_index": 0, "delta": "Look it up."}),
        ),
        event(
            json!({"type": "response.reasoning_summary_text.done", "item_id": "rs_1", "output_index": 0, "summary_index": 0, "text": "**Plan** Look it up."}),
        ),
        event(
            json!({"type": "response.reasoning_summary_part.done", "item_id": "rs_1", "output_index": 0, "summary_index": 0, "part": {"type": "summary_text", "text": "**Plan** Look it up."}}),
        ),
        event(json!({"type": "response.output_item.done", "output_index": 0, "item": done})),
        completed_with_usage("resp_1"),
    ]
    .concat();
    let (client, fake) = spawn(vec![chunked(&body, 5), whole(agent_text_body(&["ok"]))]).await;
    let events = drain(client.stream_response(&agent_request()).await.unwrap()).await;
    let summary = |d: &str| {
        Ok(StreamEvent::ReasoningSummaryTextDelta {
            output_index: 0,
            item_id: "rs_1".into(),
            summary_index: 0,
            delta: d.into(),
        })
    };
    assert_eq!(
        events,
        vec![
            Ok(StreamEvent::OutputItemAdded {
                output_index: 0,
                item: added
            }),
            summary("**Plan** "),
            summary("Look it up."),
            Ok(StreamEvent::OutputItemDone {
                output_index: 0,
                item: done.clone()
            }),
            Ok(StreamEvent::Completed {
                response_id: "resp_1".into(),
                usage: usage_1200(),
                model: String::new(),
                status: "completed".into(),
            }),
        ]
    );

    // The done item, replayed as input, reaches the API unchanged.
    let mut next = agent_request();
    next.input.push(done.clone());
    drain(client.stream_response(&next).await.unwrap()).await;
    let (_, sent) = last_request(&fake);
    assert_eq!(sent["input"][2], done);
}

#[tokio::test]
async fn stream_response_streams_a_refusal_as_text() {
    let refused = json!({"id": "msg_1", "type": "message", "role": "assistant", "status": "completed",
        "content": [{"type": "refusal", "refusal": "I can't help with that."}]});
    let body = [
        created(),
        event(
            json!({"type": "response.output_item.added", "output_index": 0, "item": message_item("in_progress", "")}),
        ),
        event(
            json!({"type": "response.refusal.delta", "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": "I can't "}),
        ),
        event(
            json!({"type": "response.refusal.delta", "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": "help with that."}),
        ),
        event(
            json!({"type": "response.refusal.done", "item_id": "msg_1", "output_index": 0, "content_index": 0, "refusal": "I can't help with that."}),
        ),
        event(json!({"type": "response.output_item.done", "output_index": 0, "item": refused})),
        completed_with_usage("resp_1"),
    ]
    .concat();
    let (client, _) = spawn(vec![whole(body)]).await;
    let events = drain(client.stream_response(&agent_request()).await.unwrap()).await;
    assert_eq!(events[1], Ok(text_delta_event("I can't ")));
    assert_eq!(events[2], Ok(text_delta_event("help with that.")));
    assert_eq!(
        events[3],
        Ok(StreamEvent::OutputItemDone {
            output_index: 0,
            item: refused
        })
    );
    assert!(matches!(events[4], Ok(StreamEvent::Completed { .. })));
    assert_eq!(events.len(), 5);
}

#[tokio::test]
async fn stream_response_passes_other_items_through() {
    let compaction = json!({"id": "cmp_1", "type": "compaction", "encrypted_content": "opaque"});
    let body = [
        created(),
        event(json!({"type": "response.output_item.added", "output_index": 0, "item": compaction})),
        event(json!({"type": "response.output_item.done", "output_index": 0, "item": compaction})),
        completed_with_usage("resp_1"),
    ]
    .concat();
    let (client, _) = spawn(vec![whole(body)]).await;
    let events = drain(client.stream_response(&agent_request()).await.unwrap()).await;
    assert_eq!(
        events[1],
        Ok(StreamEvent::OutputItemDone {
            output_index: 0,
            item: compaction
        })
    );
}

#[tokio::test]
async fn stream_response_incomplete_keeps_usage_and_ends() {
    let body = [
        agent_text_body(&["partial"]).replace(&completed_with_usage("resp_1"), ""),
        event(json!({"type": "response.incomplete", "response": {
            "id": "resp_1", "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"}, "output": [],
            "usage": {"input_tokens": 1200, "input_tokens_details": {"cached_tokens": 1024},
                      "output_tokens": 300, "output_tokens_details": {"reasoning_tokens": 200}}
        }})),
        // Nothing after a terminal event is read.
        delta("never"),
    ]
    .concat();
    let (client, _) = spawn(vec![chunked(&body, 11)]).await;
    let events = drain(client.stream_response(&agent_request()).await.unwrap()).await;
    assert_eq!(
        events.last(),
        Some(&Ok(StreamEvent::Incomplete {
            reason: "max_output_tokens".into(),
            usage: usage_1200(),
        }))
    );
    assert!(matches!(
        events[events.len() - 2],
        Ok(StreamEvent::OutputItemDone { .. })
    ));
    assert!(!events.contains(&Ok(text_delta_event("never"))));
}

#[tokio::test]
async fn stream_response_failed_and_error_events_are_terminal() {
    let failed = [
        created(),
        event(json!({"type": "response.failed", "response": {
            "id": "resp_1", "status": "failed",
            "error": {"code": "server_error", "message": "The model crashed."}, "output": []
        }})),
        completed_with_usage("resp_1"),
    ]
    .concat();
    let error = [
        created(),
        delta("half"),
        event(
            json!({"type": "error", "code": "rate_limit_exceeded", "message": "Slow down.", "param": null, "sequence_number": 2}),
        ),
    ]
    .concat();
    let nested = event(
        json!({"type": "error", "error": {"type": "server_error", "code": "overloaded", "message": "Busy."}}),
    );
    let bare_failed =
        event(json!({"type": "response.failed", "response": {"id": "r", "status": "failed"}}));
    let (client, _) = spawn(vec![
        whole(failed),
        whole(error),
        whole(nested),
        whole(bare_failed),
    ])
    .await;
    let req = agent_request();

    let events = drain(client.stream_response(&req).await.unwrap()).await;
    assert_eq!(
        events,
        vec![Ok(StreamEvent::Failed {
            code: Some("server_error".into()),
            message: "The model crashed.".into(),
        })]
    );
    let events = drain(client.stream_response(&req).await.unwrap()).await;
    assert_eq!(
        events,
        vec![
            Ok(text_delta_event("half")),
            Ok(StreamEvent::Error {
                code: Some("rate_limit_exceeded".into()),
                message: "Slow down.".into(),
            })
        ]
    );
    let events = drain(client.stream_response(&req).await.unwrap()).await;
    assert_eq!(
        events,
        vec![Ok(StreamEvent::Error {
            code: Some("overloaded".into()),
            message: "Busy.".into()
        })]
    );
    let events = drain(client.stream_response(&req).await.unwrap()).await;
    assert_eq!(
        events,
        vec![Ok(StreamEvent::Failed {
            code: None,
            message: "the response failed".into()
        })]
    );
}

#[tokio::test]
async fn stream_response_closed_or_cut_connection_is_a_network_error() {
    let half = format!("{}{}", created(), delta("half"));
    let (client, _) = spawn(vec![
        whole(half.clone()),
        Reply::SseThenDrop(vec![half.clone().into_bytes()]),
        // Cut mid-event: the partial line is never dispatched.
        Reply::SseThenDrop(vec![
            format!("{half}data: {{\"type\":\"response.out").into_bytes(),
        ]),
    ])
    .await;
    for case in ["clean close", "cut", "cut mid-event"] {
        let events = drain(client.stream_response(&agent_request()).await.unwrap()).await;
        assert_eq!(events.len(), 2, "{case}: {events:?}");
        assert_eq!(events[0], Ok(text_delta_event("half")), "{case}");
        assert!(
            events[1].as_ref().unwrap_err().starts_with("Network("),
            "{case}: {events:?}"
        );
    }
}

#[tokio::test]
async fn stream_response_unreadable_event_is_parse_and_ends() {
    let body = [
        created(),
        "data: {not json\n\n".to_string(),
        delta("after"),
        completed_with_usage("resp_1"),
    ]
    .concat();
    let no_item = event(json!({"type": "response.output_item.added", "output_index": 0}));
    let (client, _) = spawn(vec![whole(body), whole(no_item)]).await;
    for _ in 0..2 {
        let events = drain(client.stream_response(&agent_request()).await.unwrap()).await;
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(
            events[0].as_ref().unwrap_err().starts_with("Parse("),
            "{events:?}"
        );
    }
}

#[tokio::test]
async fn stream_response_ignores_done_markers_and_unknown_events() {
    let body = [
        created(),
        event(json!({"type": "response.output_text.annotation.added", "item_id": "msg_1"})),
        event(json!({"type": "response.some_future_event"})),
        delta("x"),
        completed_with_usage("resp_1"),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    let (client, _) = spawn(vec![whole(body)]).await;
    let events = drain(client.stream_response(&agent_request()).await.unwrap()).await;
    assert_eq!(
        events,
        vec![
            Ok(text_delta_event("x")),
            Ok(StreamEvent::Completed {
                response_id: "resp_1".into(),
                usage: usage_1200(),
                model: String::new(),
                status: "completed".into(),
            })
        ]
    );
}

fn rate_limited(retry_after: &str) -> Reply {
    Reply::WithHeaders(
        StatusCode::TOO_MANY_REQUESTS,
        vec![("retry-after", retry_after.into())],
        json!({"error": {"message": "Rate limit reached.", "type": "requests", "code": "rate_limit_exceeded"}}),
    )
}

#[tokio::test]
async fn stream_response_http_errors_carry_retry_after() {
    let (client, _) = spawn(vec![
        rate_limited("7"),
        Reply::WithHeaders(
            StatusCode::SERVICE_UNAVAILABLE,
            vec![
                ("retry-after", "2".into()),
                ("retry-after-ms", "1500".into()),
            ],
            json!({"error": {"message": "Overloaded."}}),
        ),
        Reply::Json(
            StatusCode::BAD_REQUEST,
            json!({"error": {"message": "Unknown parameter: 'context_management'."}}),
        ),
    ])
    .await;
    let req = agent_request();
    let err = client.stream_response(&req).await.err().unwrap();
    assert!(
        matches!(&err, AiError::Http { status: 429, message, .. } if message == "Rate limit reached.")
    );
    assert_eq!(err.retry_after(), Some(Duration::from_secs(7)));
    assert!(err.is_retryable());
    let err = client.stream_response(&req).await.err().unwrap();
    assert_eq!(err.retry_after(), Some(Duration::from_millis(1500)));
    assert!(err.is_retryable());
    let err = client.stream_response(&req).await.err().unwrap();
    assert!(
        matches!(&err, AiError::Http { status: 400, message, retry_after: None, .. } if message.contains("context_management"))
    );
    assert!(!err.is_retryable());
}

fn quick() -> Backoff {
    Backoff {
        retries: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(100),
    }
}

#[tokio::test]
async fn stream_response_with_retries_retries_429_and_5xx_then_streams() {
    let (client, fake) = spawn(vec![
        rate_limited("0.05"),
        Reply::Text(StatusCode::BAD_GATEWAY, "upstream down".into()),
        Reply::Json(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": {"message": "oops"}}),
        ),
        whole(agent_text_body(&["ok"])),
    ])
    .await;
    let started = std::time::Instant::now();
    let stream = client
        .stream_response_with_retries(&agent_request(), &quick())
        .await
        .unwrap();
    // The 429's Retry-After (50 ms, under `max_delay`) was honoured.
    assert!(started.elapsed() >= Duration::from_millis(50));
    let events = drain(stream).await;
    assert!(matches!(
        events.last(),
        Some(Ok(StreamEvent::Completed { .. }))
    ));
    let requests = fake.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    // Every attempt sends the same request.
    assert!(requests.iter().all(|(_, body)| body == &requests[0].1));
}

#[tokio::test]
async fn stream_response_with_retries_gives_up_after_three_retries() {
    let busy = || Reply::Text(StatusCode::SERVICE_UNAVAILABLE, "busy".into());
    let (client, fake) = spawn(vec![busy(), busy(), busy(), busy(), busy()]).await;
    let err = client
        .stream_response_with_retries(&agent_request(), &quick())
        .await
        .err()
        .unwrap();
    assert!(matches!(err, AiError::Http { status: 503, .. }));
    assert_eq!(fake.requests.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn stream_response_with_retries_doesnt_retry_client_errors() {
    let (client, fake) = spawn(vec![Reply::Json(
        StatusCode::UNAUTHORIZED,
        json!({"error": {"message": "Incorrect API key provided."}}),
    )])
    .await;
    let err = client
        .stream_response_with_retries(&agent_request(), &quick())
        .await
        .err()
        .unwrap();
    assert!(matches!(err, AiError::Http { status: 401, .. }));
    assert_eq!(fake.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn stream_response_with_retries_doesnt_retry_inside_the_stream() {
    let (client, fake) = spawn(vec![Reply::SseThenDrop(vec![
        format!("{}{}", created(), delta("half")).into_bytes(),
    ])])
    .await;
    let events = drain(
        client
            .stream_response_with_retries(&agent_request(), &quick())
            .await
            .unwrap(),
    )
    .await;
    assert!(
        events
            .last()
            .unwrap()
            .as_ref()
            .unwrap_err()
            .starts_with("Network(")
    );
    assert_eq!(fake.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn stream_response_unreachable_server_is_a_network_error() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let client = ResponsesClient::new(format!("http://{addr}/v1"), "k", "m");
    let err = client
        .stream_response(&agent_request())
        .await
        .err()
        .unwrap();
    assert!(matches!(err, AiError::Network(_)));
    assert!(err.is_retryable());
}
