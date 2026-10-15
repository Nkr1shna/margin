//! margin-ai's Claude client against a local fake Messages API (axum) that streams the
//! documented SSE shapes and records every request. No real network.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::StreamExt;
use margin_ai::prompts::{LESSON_CONSTANT, RERANK_CONSTANT, REWRITE_CONSTANT};
use margin_ai::{
    AiError, CLAUDE_BETAS, CallOptions, ClaudeClient, Delta, Image, InputPart, ItemView,
    ModelEvent, ModelRequest, Next, Provider, RerankCandidate, RewriteInput, RouteModels,
    StartKind, TextModel, ToolChoice, ToolDef, WebTools, rerank, rewrite_queries,
};
use margin_shared::pipeline::{RetrievalPurpose, Route};
use margin_shared::routes::REWRITE_MAX_OUTPUT_TOKENS;
use margin_shared::{ReasoningEffort, WebCitation, WebResult};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// The fake server
// ---------------------------------------------------------------------------

enum Reply {
    /// An SSE body sent as these byte chunks, a millisecond apart.
    Sse(Vec<Vec<u8>>),
    Json(StatusCode, Value),
    /// A status with these headers and a JSON body (e.g. 429 with `retry-after`).
    WithHeaders(StatusCode, Vec<(&'static str, String)>, Value),
}

#[derive(Clone, Debug)]
struct Recorded {
    headers: HeaderMap,
    body: Value,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

#[derive(Default)]
struct Fake {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<Recorded>>,
    /// `GET /v1/models` pages, in order, with the query each was asked with.
    model_pages: Mutex<VecDeque<Value>>,
    model_queries: Mutex<Vec<String>>,
}

async fn messages(State(fake): State<Arc<Fake>>, headers: HeaderMap, body: Bytes) -> Response {
    let body: Value = serde_json::from_slice(&body).expect("the client sends JSON");
    fake.requests
        .lock()
        .unwrap()
        .push(Recorded { headers, body });
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
        Reply::WithHeaders(status, headers, v) => {
            let mut resp = (status, axum::Json(v)).into_response();
            for (name, value) in headers {
                resp.headers_mut().insert(name, value.parse().unwrap());
            }
            resp
        }
    }
}

async fn models(
    State(fake): State<Arc<Fake>>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    assert_eq!(
        headers.get("x-api-key").unwrap().to_str().unwrap(),
        "sk-ant-test"
    );
    assert_eq!(
        headers.get("anthropic-version").unwrap().to_str().unwrap(),
        "2023-06-01"
    );
    fake.model_queries
        .lock()
        .unwrap()
        .push(query.unwrap_or_default());
    let page = fake.model_pages.lock().unwrap().pop_front().unwrap();
    axum::Json(page).into_response()
}

fn models_for_test() -> RouteModels {
    RouteModels {
        chat: "claude-opus-5-5".into(),
        write: "claude-sonnet-5-5".into(),
        small: "claude-haiku-5-5".into(),
    }
}

async fn spawn(replies: Vec<Reply>) -> (ClaudeClient, Arc<Fake>) {
    let fake = Arc::new(Fake {
        replies: Mutex::new(replies.into()),
        ..Default::default()
    });
    let app = Router::new()
        .route("/v1/messages", post(messages))
        .route("/v1/models", get(models))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = ClaudeClient::new(format!("http://{addr}/"), "sk-ant-test", models_for_test());
    (client, fake)
}

fn requests(fake: &Fake) -> Vec<Recorded> {
    fake.requests.lock().unwrap().clone()
}

fn last_request(fake: &Fake) -> Recorded {
    requests(fake).pop().expect("a request was made")
}

// ---------------------------------------------------------------------------
// SSE helpers (the documented event shapes)
// ---------------------------------------------------------------------------

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|v| format!("event: {}\ndata: {}\n\n", v["type"].as_str().unwrap(), v))
        .collect()
}

fn message_start(model: &str) -> Value {
    json!({"type": "message_start", "message": {
        "id": "msg_1", "type": "message", "role": "assistant", "model": model,
        "content": [], "stop_reason": null,
        "usage": {"input_tokens": 120, "cache_read_input_tokens": 1000,
                  "cache_creation_input_tokens": 30, "output_tokens": 1}
    }})
}

fn start(index: u32, block: Value) -> Value {
    json!({"type": "content_block_start", "index": index, "content_block": block})
}

fn delta(index: u32, delta: Value) -> Value {
    json!({"type": "content_block_delta", "index": index, "delta": delta})
}

fn text_delta(index: u32, text: &str) -> Value {
    delta(index, json!({"type": "text_delta", "text": text}))
}

fn stop(index: u32) -> Value {
    json!({"type": "content_block_stop", "index": index})
}

fn message_delta(stop_reason: &str, usage: Value) -> Value {
    json!({"type": "message_delta", "delta": {"stop_reason": stop_reason, "stop_sequence": null}, "usage": usage})
}

fn message_stop() -> Value {
    json!({"type": "message_stop"})
}

fn ping() -> Value {
    json!({"type": "ping"})
}

/// A whole response: `message_start`, these blocks' events, `message_delta`,
/// `message_stop`.
fn response(blocks: Vec<Value>, stop_reason: &str) -> String {
    let mut events = vec![message_start("claude-opus-5-5-20261001"), ping()];
    events.extend(blocks);
    events.push(message_delta(stop_reason, json!({"output_tokens": 42})));
    events.push(message_stop());
    sse(&events)
}

fn whole(body: String) -> Reply {
    Reply::Sse(vec![body.into_bytes()])
}

fn chunked(body: &str, size: usize) -> Reply {
    Reply::Sse(body.as_bytes().chunks(size).map(<[u8]>::to_vec).collect())
}

fn tool(name: &str) -> ToolDef {
    ToolDef {
        name: name.into(),
        description: format!("The {name} tool."),
        parameters: json!({
            "type": "object",
            "properties": {"q": {"type": "string"}},
            "required": ["q"],
            "additionalProperties": false,
        }),
    }
}

fn step(history: Vec<Value>) -> ModelRequest {
    ModelRequest {
        opts: CallOptions::new(Route::Chat)
            .with_cache_key("chat:1")
            .with_prompt_version("agent-v2"),
        system: "You are Margin's agent.".into(),
        history,
        tools: vec![tool("search"), tool("read_passage")],
        tool_choice: ToolChoice::Auto,
        web: WebTools {
            search: true,
            fetch: true,
        },
        effort: ReasoningEffort::High,
        compaction: Some(150_000),
        parallel_tool_calls: true,
    }
}

fn user_says(text: &str) -> Vec<Value> {
    vec![json!({"role": "user", "content": [{"type": "text", "text": text}]})]
}

/// Runs one step to its end: every event, or the error that ended it.
async fn run(client: &ClaudeClient, req: ModelRequest) -> (Vec<ModelEvent>, Option<AiError>) {
    let mut stream = match client.stream(req).await {
        Ok(s) => s,
        Err(e) => return (vec![], Some(e)),
    };
    let mut events = Vec::new();
    while let Some(e) = stream.next().await {
        match e {
            Ok(e) => events.push(e),
            Err(e) => return (events, Some(e)),
        }
    }
    (events, None)
}

fn done_items(events: &[ModelEvent]) -> Vec<(u32, Vec<Value>, ItemView)> {
    events
        .iter()
        .filter_map(|e| match e {
            ModelEvent::Done { index, item } => {
                Some((*index, item.stored.clone(), item.view.clone()))
            }
            _ => None,
        })
        .collect()
}

fn terminal(events: &[ModelEvent]) -> &ModelEvent {
    events.last().expect("a terminal event")
}

// ---------------------------------------------------------------------------
// Streaming: text, thinking, tools
// ---------------------------------------------------------------------------

#[tokio::test]
async fn text_streams_in_any_chunking() {
    let body = response(
        vec![
            start(0, json!({"type": "text", "text": ""})),
            text_delta(0, "Héllo, "),
            text_delta(0, "wörld — ∫ x dx"),
            stop(0),
        ],
        "end_turn",
    );
    for size in [1, 5, 64, body.len()] {
        let (client, _) = spawn(vec![chunked(&body, size)]).await;
        let (events, err) = run(&client, step(user_says("hi"))).await;
        assert!(err.is_none(), "{err:?}");
        assert_eq!(
            events[0],
            ModelEvent::Started {
                index: 0,
                kind: StartKind::Text
            }
        );
        let deltas: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ModelEvent::Delta {
                    index: 0,
                    delta: Delta::Text(t),
                } => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, ["Héllo, ", "wörld — ∫ x dx"], "chunk size {size}");
        let items = done_items(&events);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].1,
            vec![
                json!({"role": "assistant", "block": {"type": "text", "text": "Héllo, wörld — ∫ x dx"}})
            ]
        );
        match terminal(&events) {
            ModelEvent::Completed {
                usage,
                model,
                stop_reason,
                next,
            } => {
                assert_eq!(model, "claude-opus-5-5-20261001");
                assert_eq!(stop_reason, "end_turn");
                assert_eq!(*next, Next::Finished);
                // 120 fresh + 1000 cache reads + 30 cache writes.
                assert_eq!(usage.input_tokens, 1150);
                assert_eq!(usage.cached_input_tokens, 1000);
                assert_eq!(usage.cache_write_input_tokens, 30);
                assert_eq!(usage.output_tokens, 42);
                assert_eq!(usage.model_calls, 1);
            }
            other => panic!("{other:?}"),
        }
    }
}

#[tokio::test]
async fn thinking_and_its_signature_are_stored_verbatim() {
    let body = response(
        vec![
            start(
                0,
                json!({"type": "thinking", "thinking": "", "signature": ""}),
            ),
            delta(0, json!({"type": "thinking_delta", "thinking": "Let me "})),
            delta(0, json!({"type": "thinking_delta", "thinking": "think."})),
            delta(
                0,
                json!({"type": "signature_delta", "signature": "EqQBCgIYAhIM1gbcDa=="}),
            ),
            stop(0),
            start(
                1,
                json!({"type": "redacted_thinking", "data": "EmwKAhgBEgy3va3pzix/"}),
            ),
            stop(1),
            start(2, json!({"type": "text", "text": ""})),
            text_delta(2, "Answer."),
            stop(2),
        ],
        "end_turn",
    );
    let (client, _) = spawn(vec![whole(body)]).await;
    let (events, err) = run(&client, step(user_says("hi"))).await;
    assert!(err.is_none());
    assert!(events.contains(&ModelEvent::Delta {
        index: 0,
        delta: Delta::Reasoning("Let me ".into())
    }));
    let items = done_items(&events);
    assert_eq!(items[0].0, 0);
    assert_eq!(
        items[0].1,
        vec![json!({"role": "assistant", "block": {
            "type": "thinking", "thinking": "Let me think.", "signature": "EqQBCgIYAhIM1gbcDa=="
        }})]
    );
    assert_eq!(
        items[0].2,
        ItemView::Reasoning {
            summary: "Let me think.".into()
        }
    );
    assert_eq!(
        items[1].1,
        vec![
            json!({"role": "assistant", "block": {"type": "redacted_thinking", "data": "EmwKAhgBEgy3va3pzix/"}})
        ]
    );
    assert_eq!((items[1].0, items[2].0), (1, 2));
}

#[tokio::test]
async fn a_tool_call_streams_its_arguments() {
    let body = response(
        vec![
            start(0, json!({"type": "text", "text": ""})),
            text_delta(0, "Looking."),
            stop(0),
            start(
                1,
                json!({"type": "tool_use", "id": "toolu_1", "name": "search", "input": {}}),
            ),
            delta(1, json!({"type": "input_json_delta", "partial_json": ""})),
            delta(
                1,
                json!({"type": "input_json_delta", "partial_json": "{\"q\": \"pho"}),
            ),
            delta(
                1,
                json!({"type": "input_json_delta", "partial_json": "tosynthesis\"}"}),
            ),
            stop(1),
        ],
        "tool_use",
    );
    let (client, _) = spawn(vec![whole(body)]).await;
    let (events, _) = run(&client, step(user_says("hi"))).await;
    assert!(events.contains(&ModelEvent::Started {
        index: 1,
        kind: StartKind::ToolCall {
            call_id: "toolu_1".into(),
            name: "search".into()
        }
    }));
    let args: String = events
        .iter()
        .filter_map(|e| match e {
            ModelEvent::Delta {
                index: 1,
                delta: Delta::Arguments(a),
            } => Some(a.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(args, "{\"q\": \"photosynthesis\"}");
    let items = done_items(&events);
    assert_eq!(
        items[1].2,
        ItemView::ToolCall {
            call_id: "toolu_1".into(),
            name: "search".into(),
            arguments: "{\"q\": \"photosynthesis\"}".into()
        }
    );
    assert_eq!(
        items[1].1[0]["block"],
        json!({"type": "tool_use", "id": "toolu_1", "name": "search", "input": {"q": "photosynthesis"}})
    );
    match terminal(&events) {
        ModelEvent::Completed {
            next, stop_reason, ..
        } => {
            assert_eq!(*next, Next::Finished);
            assert_eq!(stop_reason, "tool_use");
        }
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Web search and fetch
// ---------------------------------------------------------------------------

fn search_call(index: u32, id: &str, query: &str) -> Vec<Value> {
    vec![
        start(
            index,
            json!({"type": "server_tool_use", "id": id, "name": "web_search", "input": {}}),
        ),
        delta(
            index,
            json!({"type": "input_json_delta", "partial_json": json!({"query": query}).to_string()}),
        ),
        stop(index),
    ]
}

#[tokio::test]
async fn a_web_search_and_its_results() {
    let results = json!([
        {"type": "web_search_result", "url": "https://example.com/a", "title": "A",
         "encrypted_content": "EqgfCioIARgBIiQ3", "page_age": "April 30, 2025"},
        {"type": "web_search_result", "url": "https://example.com/b", "title": "B",
         "encrypted_content": "EqgfCioIARgBIiQ4"}
    ]);
    let result_block =
        json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": results});
    let mut blocks = search_call(0, "srvtoolu_1", "rust 2026 edition");
    blocks.extend([start(1, result_block.clone()), stop(1)]);
    blocks.extend(search_call(2, "srvtoolu_2", "again"));
    blocks.extend([
        start(
            3,
            json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_2",
                   "content": {"type": "web_search_tool_result_error", "error_code": "max_uses_exceeded"}}),
        ),
        stop(3),
    ]);
    let (client, _) = spawn(vec![whole(response(blocks, "end_turn"))]).await;
    let (events, _) = run(&client, step(user_says("news?"))).await;
    assert_eq!(
        events[0],
        ModelEvent::Started {
            index: 0,
            kind: StartKind::WebSearch {
                id: "srvtoolu_1".into()
            }
        }
    );
    let items = done_items(&events);
    assert_eq!(
        items[0].2,
        ItemView::WebSearch {
            id: "srvtoolu_1".into(),
            query: "rust 2026 edition".into()
        }
    );
    assert_eq!(
        items[1].2,
        ItemView::WebSearchResult {
            id: "srvtoolu_1".into(),
            results: vec![
                WebResult {
                    url: "https://example.com/a".into(),
                    title: Some("A".into()),
                    page_age: Some("April 30, 2025".into())
                },
                WebResult {
                    url: "https://example.com/b".into(),
                    title: Some("B".into()),
                    page_age: None
                },
            ],
            error: None
        }
    );
    // `encrypted_content` is kept byte for byte for replay.
    assert_eq!(
        items[1].1,
        vec![json!({"role": "assistant", "block": result_block})]
    );
    assert_eq!(
        items[3].2,
        ItemView::WebSearchResult {
            id: "srvtoolu_2".into(),
            results: vec![],
            error: Some("max_uses_exceeded".into())
        }
    );
}

#[tokio::test]
async fn citations_span_their_blocks_in_the_merged_text() {
    let cite = json!({"type": "web_search_result_location", "url": "https://example.com/a",
        "title": "A", "encrypted_index": "Eo8BCioIAhgB", "cited_text": "Rust 2026 ships…"});
    let blocks = vec![
        start(0, json!({"type": "text", "text": ""})),
        text_delta(0, "Per the notes, "),
        stop(0),
        start(1, json!({"type": "text", "text": "", "citations": []})),
        delta(1, json!({"type": "citations_delta", "citation": cite})),
        text_delta(1, "the edition shipped"),
        stop(1),
        start(2, json!({"type": "text", "text": ""})),
        text_delta(2, "."),
        stop(2),
    ];
    let (client, _) = spawn(vec![whole(response(blocks, "end_turn"))]).await;
    let (events, _) = run(&client, step(user_says("q"))).await;
    let starts = events
        .iter()
        .filter(|e| matches!(e, ModelEvent::Started { .. }))
        .count();
    assert_eq!(starts, 1, "three text blocks are one item");
    let items = done_items(&events);
    assert_eq!(items.len(), 1);
    let (_, stored, view) = &items[0];
    assert_eq!(stored.len(), 3, "each block stored as it came");
    assert_eq!(stored[1]["block"]["citations"], json!([cite]));
    assert!(stored[0]["block"].get("citations").is_none());
    let start_at = "Per the notes, ".len() as u32;
    assert_eq!(
        *view,
        ItemView::Text {
            text: "Per the notes, the edition shipped.".into(),
            citations: vec![WebCitation {
                url: "https://example.com/a".into(),
                title: Some("A".into()),
                cited_text: Some("Rust 2026 ships…".into()),
                start: Some(start_at),
                end: Some(start_at + "the edition shipped".len() as u32),
            }]
        }
    );
}

#[tokio::test]
async fn a_web_fetch_and_a_citation_of_the_page() {
    let fetched = json!({"type": "web_fetch_tool_result", "tool_use_id": "srvtoolu_f",
        "content": {"type": "web_fetch_result", "url": "https://example.com/page",
            "retrieved_at": "2026-10-14T10:00:00Z",
            "content": {"type": "document", "title": "The Page",
                "source": {"type": "text", "media_type": "text/plain", "data": "Body text."},
                "citations": {"enabled": true}}}});
    let blocks = vec![
        start(
            0,
            json!({"type": "server_tool_use", "id": "srvtoolu_f", "name": "web_fetch", "input": {}}),
        ),
        delta(
            0,
            json!({"type": "input_json_delta", "partial_json": "{\"url\":\"https://example.com/page\"}"}),
        ),
        stop(0),
        start(1, fetched),
        stop(1),
        start(2, json!({"type": "text", "text": "", "citations": []})),
        delta(
            2,
            json!({"type": "citations_delta", "citation": {"type": "char_location",
            "cited_text": "Body text.", "document_index": 0, "document_title": "The Page",
            "start_char_index": 0, "end_char_index": 10}}),
        ),
        text_delta(2, "It says so."),
        stop(2),
    ];
    let (client, _) = spawn(vec![whole(response(blocks, "end_turn"))]).await;
    let (events, _) = run(&client, step(user_says("read https://example.com/page"))).await;
    assert_eq!(
        events[0],
        ModelEvent::Started {
            index: 0,
            kind: StartKind::WebFetch {
                id: "srvtoolu_f".into()
            }
        }
    );
    let items = done_items(&events);
    assert_eq!(
        items[0].2,
        ItemView::WebFetch {
            id: "srvtoolu_f".into(),
            url: "https://example.com/page".into()
        }
    );
    assert_eq!(
        items[1].2,
        ItemView::WebFetchResult {
            id: "srvtoolu_f".into(),
            url: "https://example.com/page".into(),
            title: Some("The Page".into()),
            error: None
        }
    );
    let ItemView::Text { citations, .. } = &items[2].2 else {
        panic!()
    };
    assert_eq!(
        citations,
        &vec![WebCitation {
            url: "https://example.com/page".into(),
            title: Some("The Page".into()),
            cited_text: Some("Body text.".into()),
            start: Some(0),
            end: Some(11),
        }]
    );
}

#[tokio::test]
async fn a_failed_fetch_names_its_error_and_the_url_asked_for() {
    let blocks = vec![
        start(
            0,
            json!({"type": "server_tool_use", "id": "srvtoolu_f", "name": "web_fetch",
                   "input": {"url": "https://example.com/gone"}}),
        ),
        stop(0),
        start(
            1,
            json!({"type": "web_fetch_tool_result", "tool_use_id": "srvtoolu_f",
                   "content": {"type": "web_fetch_tool_result_error", "error_code": "url_not_accessible"}}),
        ),
        stop(1),
    ];
    let (client, _) = spawn(vec![whole(response(blocks, "end_turn"))]).await;
    let (events, _) = run(&client, step(user_says("q"))).await;
    let items = done_items(&events);
    assert_eq!(
        items[1].2,
        ItemView::WebFetchResult {
            id: "srvtoolu_f".into(),
            url: "https://example.com/gone".into(),
            title: None,
            error: Some("url_not_accessible".into())
        }
    );
}

#[tokio::test]
async fn pause_turn_and_a_pending_server_tool() {
    // `pause_turn`: call again with the same history.
    let (client, _) = spawn(vec![whole(response(
        search_call(0, "srvtoolu_1", "long research"),
        "pause_turn",
    ))])
    .await;
    let (events, _) = run(&client, step(user_says("q"))).await;
    match terminal(&events) {
        ModelEvent::Completed {
            next, stop_reason, ..
        } => {
            assert_eq!(*next, Next::Paused);
            assert_eq!(stop_reason, "pause_turn");
        }
        other => panic!("{other:?}"),
    }

    // A client tool call while a server tool still waits for its result.
    let mut blocks = search_call(0, "srvtoolu_1", "q");
    blocks.extend([
        start(
            1,
            json!({"type": "tool_use", "id": "toolu_1", "name": "search", "input": {"q": "x"}}),
        ),
        stop(1),
    ]);
    let (client, _) = spawn(vec![whole(response(blocks, "tool_use"))]).await;
    let (events, _) = run(&client, step(user_says("q"))).await;
    assert!(matches!(
        terminal(&events),
        ModelEvent::Completed {
            next: Next::ServerToolPending,
            ..
        }
    ));
    // A tool call given whole in `content_block_start` keeps its input.
    assert_eq!(
        done_items(&events)[1].2,
        ItemView::ToolCall {
            call_id: "toolu_1".into(),
            name: "search".into(),
            arguments: "{\"q\":\"x\"}".into()
        }
    );
}

#[tokio::test]
async fn a_compaction_block_is_kept() {
    let blocks = vec![
        start(0, json!({"type": "compaction", "content": ""})),
        delta(
            0,
            json!({"type": "compaction_delta", "content": "Summary of the chat so far."}),
        ),
        stop(0),
        start(1, json!({"type": "text", "text": ""})),
        text_delta(1, "Go on."),
        stop(1),
    ];
    let (client, _) = spawn(vec![whole(response(blocks, "end_turn"))]).await;
    let (events, _) = run(&client, step(user_says("q"))).await;
    let items = done_items(&events);
    assert_eq!(items[0].2, ItemView::Compaction);
    assert_eq!(
        items[0].1,
        vec![
            json!({"role": "assistant", "block": {"type": "compaction", "content": "Summary of the chat so far."}})
        ]
    );
}

#[tokio::test]
async fn usage_counts_server_tools_cache_writes_and_iterations() {
    let mut events = vec![message_start("claude-opus-5-5")];
    events.push(message_delta(
        "end_turn",
        json!({
            "input_tokens": 50, "cache_read_input_tokens": 2000,
            "cache_creation_input_tokens": 700, "output_tokens": 300,
            "output_tokens_details": {"thinking_tokens": 120},
            "server_tool_use": {"web_search_requests": 3, "web_fetch_requests": 1},
            "iterations": [
                {"type": "compaction", "input_tokens": 180000, "output_tokens": 2500},
                {"type": "message", "input_tokens": 50, "output_tokens": 300}
            ]
        }),
    ));
    events.push(message_stop());
    let (client, _) = spawn(vec![whole(sse(&events))]).await;
    let (events, _) = run(&client, step(user_says("q"))).await;
    let ModelEvent::Completed { usage, .. } = terminal(&events) else {
        panic!()
    };
    // Every input token: the iterations' fresh input plus the cache reads and writes
    // (Claude's `input_tokens` leaves those out).
    assert_eq!(usage.input_tokens, 180_050 + 2000 + 700);
    assert_eq!(usage.output_tokens, 2_800);
    assert_eq!(usage.cached_input_tokens, 2000);
    assert_eq!(usage.cache_write_input_tokens, 700);
    assert_eq!(usage.reasoning_tokens, 120);
    assert_eq!(usage.web_search_requests, 3);
    assert_eq!(usage.web_fetch_requests, 1);
}

#[tokio::test]
async fn usage_sums_cache_counts_of_iterations_when_given() {
    let mut events = vec![message_start("claude-opus-5-5")];
    events.push(message_delta(
        "end_turn",
        json!({
            "input_tokens": 40, "cache_read_input_tokens": 900,
            "cache_creation_input_tokens": 60, "output_tokens": 20,
            "iterations": [
                {"type": "compaction", "input_tokens": 1000, "cache_read_input_tokens": 5000,
                 "cache_creation_input_tokens": 0, "output_tokens": 200},
                {"type": "message", "input_tokens": 40, "cache_read_input_tokens": 900,
                 "cache_creation_input_tokens": 60, "output_tokens": 20}
            ]
        }),
    ));
    events.push(message_stop());
    let (client, _) = spawn(vec![whole(sse(&events))]).await;
    let (events, _) = run(&client, step(user_says("q"))).await;
    let ModelEvent::Completed { usage, .. } = terminal(&events) else {
        panic!()
    };
    assert_eq!(usage.cached_input_tokens, 5900);
    assert_eq!(usage.cache_write_input_tokens, 60);
    assert_eq!(usage.input_tokens, 1040 + 5900 + 60);
    assert_eq!(usage.output_tokens, 220);
}

#[tokio::test]
async fn cut_off_refused_and_failed_responses() {
    let text = || {
        vec![
            start(0, json!({"type": "text", "text": ""})),
            text_delta(0, "Partial"),
            stop(0),
        ]
    };
    for reason in ["max_tokens", "model_context_window_exceeded"] {
        let (client, _) = spawn(vec![whole(response(text(), reason))]).await;
        let (events, _) = run(&client, step(user_says("q"))).await;
        assert!(
            matches!(terminal(&events), ModelEvent::Incomplete { reason: r, .. } if r == reason)
        );
    }

    // A refusal: the text as it came, then `Incomplete { reason: "refusal:cyber" }`
    // (the category from `stop_details`).
    let mut events = vec![message_start("claude-opus-5-5")];
    events.extend(text());
    events.push(json!({"type": "message_delta",
        "delta": {"stop_reason": "refusal", "stop_details": {"type": "refusal", "category": "cyber"}},
        "usage": {"output_tokens": 3}}));
    events.push(message_stop());
    let (client, _) = spawn(vec![whole(sse(&events))]).await;
    let (events, _) = run(&client, step(user_says("q"))).await;
    assert!(matches!(
        &done_items(&events)[0].2,
        ItemView::Text { text, .. } if text == "Partial"
    ));
    assert!(matches!(
        terminal(&events),
        ModelEvent::Incomplete { reason, usage } if reason == "refusal:cyber" && usage.output_tokens == 3
    ));

    // Without `stop_details` the reason is plain `refusal`.
    let mut events = vec![message_start("claude-opus-5-5")];
    events.extend(text());
    events.push(json!({"type": "message_delta",
        "delta": {"stop_reason": "refusal"}, "usage": {"output_tokens": 3}}));
    events.push(message_stop());
    let (client, _) = spawn(vec![whole(sse(&events))]).await;
    let (events, _) = run(&client, step(user_says("q"))).await;
    assert!(matches!(
        terminal(&events),
        ModelEvent::Incomplete { reason, .. } if reason == "refusal"
    ));

    // `event: error` mid-stream.
    let mut events = vec![message_start("claude-opus-5-5")];
    events.extend(text());
    events.push(
        json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
    );
    let (client, _) = spawn(vec![whole(sse(&events))]).await;
    let (events, err) = run(&client, step(user_says("q"))).await;
    assert!(err.is_none());
    assert_eq!(
        terminal(&events),
        &ModelEvent::Failed {
            code: Some("overloaded_error".into()),
            message: "Overloaded".into()
        }
    );

    // A body that ends before `message_stop`.
    let body = sse(&[
        message_start("m"),
        start(0, json!({"type": "text", "text": ""})),
    ]);
    let (client, _) = spawn(vec![whole(body)]).await;
    let (_, err) = run(&client, step(user_says("q"))).await;
    assert!(matches!(err, Some(AiError::Network(_))), "{err:?}");
}

// ---------------------------------------------------------------------------
// HTTP errors
// ---------------------------------------------------------------------------

fn api_error(kind: &str, message: &str) -> Value {
    json!({"type": "error", "error": {"type": kind, "message": message}, "request_id": "req_1"})
}

#[tokio::test]
async fn http_errors_carry_their_code_and_pause() {
    let (client, _) = spawn(vec![Reply::WithHeaders(
        StatusCode::TOO_MANY_REQUESTS,
        vec![("retry-after", "7".into())],
        api_error(
            "rate_limit_error",
            "Number of request tokens has exceeded your limit.",
        ),
    )])
    .await;
    let (_, err) = run(&client, step(user_says("q"))).await;
    let err = err.unwrap();
    assert_eq!(err.retry_after(), Some(Duration::from_secs(7)));
    assert_eq!(err.code(), Some("rate_limit_error"));
    assert!(err.is_retryable());

    let (client, _) = spawn(vec![Reply::Json(
        StatusCode::TOO_MANY_REQUESTS,
        json!({"type": "error", "error": {"type": "rate_limit_error",
            "message": "You have reached your spend limit.",
            "details": {"error_code": "enforced_spend_limit_reached"}}}),
    )])
    .await;
    let (_, err) = run(&client, step(user_says("q"))).await;
    let err = err.unwrap();
    assert_eq!(err.code(), Some("enforced_spend_limit_reached"));
    assert!(!err.is_retryable());

    let (client, _) = spawn(vec![Reply::Json(
        StatusCode::UNAUTHORIZED,
        api_error("authentication_error", "invalid x-api-key"),
    )])
    .await;
    let (_, err) = run(&client, step(user_says("q"))).await;
    let err = err.unwrap();
    assert!(matches!(err, AiError::Http { status: 401, .. }));
    assert_eq!(err.code(), Some("authentication_error"));
    assert!(!err.is_retryable());

    let (client, _) = spawn(vec![Reply::Json(
        StatusCode::from_u16(529).unwrap(),
        api_error("overloaded_error", "Overloaded"),
    )])
    .await;
    let (_, err) = run(&client, step(user_says("q"))).await;
    let err = err.unwrap();
    assert_eq!(err.code(), Some("overloaded_error"));
    assert!(err.is_retryable());
}

#[tokio::test]
async fn web_tools_fall_back_to_direct_callers_once_per_model() {
    let ok = || whole(response(vec![], "end_turn"));
    let (client, fake) = spawn(vec![
        Reply::Json(
            StatusCode::BAD_REQUEST,
            api_error(
                "invalid_request_error",
                "tools.4: this model does not support programmatic tool calling; set allowed_callers to [\"direct\"]",
            ),
        ),
        ok(),
        ok(),
    ])
    .await;
    let (_, err) = run(&client, step(user_says("q"))).await;
    assert!(err.is_none(), "{err:?}");
    let reqs = requests(&fake);
    assert_eq!(reqs.len(), 2);
    for tool in &reqs[0].body["tools"].as_array().unwrap()[2..] {
        assert!(tool.get("allowed_callers").is_none());
    }
    for tool in &reqs[1].body["tools"].as_array().unwrap()[2..] {
        assert_eq!(tool["allowed_callers"], json!(["direct"]));
    }
    // Remembered: the next step sends it at once.
    let (_, err) = run(&client, step(user_says("q"))).await;
    assert!(err.is_none());
    let reqs = requests(&fake);
    assert_eq!(reqs.len(), 3);
    assert_eq!(
        reqs[2].body["tools"][3]["allowed_callers"],
        json!(["direct"])
    );
    // Function tools never get it.
    assert!(reqs[2].body["tools"][0].get("allowed_callers").is_none());
}

#[tokio::test]
async fn other_bad_requests_are_not_retried() {
    let (client, fake) = spawn(vec![Reply::Json(
        StatusCode::BAD_REQUEST,
        api_error("invalid_request_error", "messages: roles must alternate"),
    )])
    .await;
    let (_, err) = run(&client, step(user_says("q"))).await;
    assert!(matches!(err, Some(AiError::Http { status: 400, .. })));
    assert_eq!(requests(&fake).len(), 1);
}

// ---------------------------------------------------------------------------
// The request
// ---------------------------------------------------------------------------

fn breakpoints(v: &Value) -> usize {
    match v {
        Value::Object(map) => {
            usize::from(map.contains_key("cache_control"))
                + map.values().map(breakpoints).sum::<usize>()
        }
        Value::Array(items) => items.iter().map(breakpoints).sum(),
        _ => 0,
    }
}

#[tokio::test]
async fn an_agent_step_request() {
    let (client, fake) = spawn(vec![whole(response(vec![], "end_turn"))]).await;
    let history = vec![
        json!({"role": "user", "content": [{"type": "text", "text": "Context."}, {"type": "text", "text": "Hi"}]}),
        json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "t", "signature": "sig"},
            {"type": "tool_use", "id": "toolu_1", "name": "search", "input": {"q": "x"}}
        ]}),
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": "found", "is_error": false}
        ]}),
    ];
    run(&client, step(history.clone())).await;
    let req = last_request(&fake);

    assert_eq!(req.header("x-api-key"), Some("sk-ant-test"));
    assert_eq!(req.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(req.header("anthropic-beta"), Some(CLAUDE_BETAS));
    assert_eq!(
        req.header("anthropic-beta"),
        Some("compact-2026-01-12,thinking-binding-controls-2026-08-01")
    );
    assert_eq!(req.header("accept"), Some("text/event-stream"));
    assert!(req.header("authorization").is_none());

    let body = &req.body;
    assert_eq!(body["model"], "claude-opus-5-5");
    assert_eq!(body["max_tokens"], 32_000);
    assert_eq!(body["stream"], true);
    assert_eq!(
        body["system"],
        json!([{"type": "text", "text": "You are Margin's agent.", "cache_control": {"type": "ephemeral"}}])
    );
    assert_eq!(
        body["thinking"],
        json!({"type": "adaptive", "display": "summarized",
               "block_binding": {"prefix_mismatch_behavior": "drop_block"}})
    );
    assert_eq!(body["output_config"], json!({"effort": "high"}));
    assert_eq!(
        body["context_management"],
        json!({"edits": [{"type": "compact_20260112",
                          "trigger": {"type": "input_tokens", "value": 150000}}]})
    );
    assert_eq!(body["tool_choice"], json!({"type": "auto"}));
    for absent in [
        "temperature",
        "top_p",
        "top_k",
        "metadata",
        "prompt_cache_key",
    ] {
        assert!(body.get(absent).is_none(), "{absent}");
    }
    assert!(body["thinking"].get("budget_tokens").is_none());

    // Tools: strict function tools, the last cached, then the web tools.
    assert_eq!(
        body["tools"],
        json!([
            {"name": "search", "description": "The search tool.", "strict": true,
             "input_schema": tool("search").parameters},
            {"name": "read_passage", "description": "The read_passage tool.", "strict": true,
             "input_schema": tool("read_passage").parameters,
             "cache_control": {"type": "ephemeral"}},
            {"type": "web_search_20260318", "name": "web_search", "max_uses": 8},
            {"type": "web_fetch_20260318", "name": "web_fetch", "max_uses": 8,
             "citations": {"enabled": true}, "max_content_tokens": 50000}
        ])
    );

    // Messages as given, the marker on the last block of the last message only.
    let mut expected = history;
    expected[2]["content"][0]["cache_control"] = json!({"type": "ephemeral"});
    assert_eq!(body["messages"], Value::Array(expected));
    assert_eq!(breakpoints(body), 3, "system, last tool, last message");
}

#[tokio::test]
async fn gating_and_the_last_step() {
    let ok = || whole(response(vec![], "end_turn"));
    let (client, fake) = spawn(vec![ok(), ok(), ok()]).await;
    let full_tools = |r: &Recorded| r.body["tools"].clone();

    let mut req = step(user_says("q"));
    run(&client, req.clone()).await;
    let auto = last_request(&fake);

    // `Allowed` is `auto` on Claude, and the tool list doesn't change (D5).
    req.tool_choice = ToolChoice::Allowed(vec!["search".into()]);
    req.parallel_tool_calls = false;
    run(&client, req.clone()).await;
    let allowed = last_request(&fake);
    assert_eq!(full_tools(&allowed), full_tools(&auto));
    assert_eq!(
        allowed.body["tool_choice"],
        json!({"type": "auto", "disable_parallel_tool_use": true})
    );

    // The last step answers in text: `none`, the same tools.
    req.tool_choice = ToolChoice::None;
    run(&client, req).await;
    let none = last_request(&fake);
    assert_eq!(none.body["tool_choice"], json!({"type": "none"}));
    assert_eq!(full_tools(&none), full_tools(&auto));
}

#[tokio::test]
async fn stored_entries_render_as_messages() {
    let (client, fake) = spawn(vec![whole(response(vec![], "end_turn"))]).await;
    let mut req = step(vec![
        json!({"role": "user", "block": {"type": "text", "text": "Context."}}),
        json!({"role": "user", "block": {"type": "text", "text": "Hi"}}),
        json!({"role": "assistant", "block": {"type": "text", "text": "Hello"}}),
        json!({"role": "assistant", "block": {"type": "thinking", "thinking": "", "signature": "s"}}),
    ]);
    req.web = WebTools::default();
    req.tools = vec![];
    req.compaction = None;
    run(&client, req).await;
    let body = last_request(&fake).body;
    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "Context."}, {"type": "text", "text": "Hi"}]},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Hello", "cache_control": {"type": "ephemeral"}},
                {"type": "thinking", "thinking": "", "signature": "s"}
            ]}
        ])
    );
    // No tools at all: neither `tools` nor `tool_choice`.
    assert!(body.get("tools").is_none() && body.get("tool_choice").is_none());
    assert!(body.get("context_management").is_none());
}

// ---------------------------------------------------------------------------
// Plain text calls
// ---------------------------------------------------------------------------

fn text_reply(text: &str) -> Reply {
    whole(response(
        vec![
            start(0, json!({"type": "text", "text": ""})),
            text_delta(0, text),
            stop(0),
        ],
        "end_turn",
    ))
}

#[tokio::test]
async fn text_calls_split_system_at_the_constant_and_pick_the_route_model() {
    let (client, fake) = spawn(vec![text_reply("Lesson one."), text_reply("Lesson two.")]).await;
    let mut seen = Vec::new();
    for (variable, expect) in [
        ("\n\nLevel: beginner. Passages: A.", "Lesson one."),
        ("\n\nLevel: expert. Passages: B, C.", "Lesson two."),
    ] {
        let instructions = format!("{LESSON_CONSTANT}{variable}");
        let opts = CallOptions::new(Route::Write)
            .with_cache_key("lesson:lesson-v2")
            .with_cache_prefix_len(LESSON_CONSTANT.len())
            .with_prompt_version("lesson-v2");
        let mut deltas = String::new();
        let out = client
            .stream_parts(
                &instructions,
                &[InputPart::Text("Write it.".into())],
                &opts,
                Box::new(|d| deltas.push_str(d)),
            )
            .await
            .unwrap();
        assert_eq!(out.text, expect);
        assert_eq!(deltas, expect);
        assert_eq!(out.model, "claude-opus-5-5-20261001");
        assert_eq!(out.stop_reason, "end_turn");
        assert_eq!(out.usage.cache_write_input_tokens, 30);
        let req = last_request(&fake);
        let body = &req.body;
        assert_eq!(body["model"], "claude-sonnet-5-5", "the write slot");
        assert_eq!(body["max_tokens"], 64_000);
        assert_eq!(body["system"][0]["text"], LESSON_CONSTANT);
        assert_eq!(
            body["system"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert_eq!(body["system"][1], json!({"type": "text", "text": variable}));
        assert_eq!(breakpoints(body), 1);
        assert!(body.get("thinking").is_none(), "omitted = adaptive");
        assert!(body.get("output_config").is_none());
        assert!(body.get("tools").is_none());
        assert!(req.header("anthropic-beta").is_none());
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": [{"type": "text", "text": "Write it."}]}])
        );
        seen.push(serde_json::to_vec(&body["system"][0]).unwrap());
    }
    assert_eq!(seen[0], seen[1], "system[0] is byte-identical per version");
}

/// The R2 prompts on the Claude client (the prefix identity the prefix gate checks in
/// `margin_evals::prefix`): every rewrite and rerank call sends its version's constant
/// block alone as `system[0]` with the one cache marker (neither prompt has a variable
/// part), byte-identical across calls, on the small slot; the query, names and
/// candidates go in the message, the schema after them.
#[tokio::test]
async fn rewrite_and_rerank_calls_send_their_constant_block_as_the_cached_system() {
    let (client, fake) = spawn(vec![
        text_reply(
            r#"{"queries": ["gibbs phenomenon ringing", "gibbs", "overshoot near a jump", "x"]}"#,
        ),
        text_reply(r#"{"queries": ["fourier coefficient formula"]}"#),
        text_reply(r#"{"ranking": [{"id": "p2", "grade": 3}]}"#),
        text_reply(r#"{"ranking": [{"id": "p1", "grade": 2}]}"#),
    ])
    .await;
    let opts = CallOptions::new(Route::Small);
    let mut input = RewriteInput {
        query: "gibbs".into(),
        purpose: RetrievalPurpose::Agent,
        scope_names: vec!["notes/dsp.pdf".into()],
        max_added: 2,
    };
    let out = rewrite_queries(&client, &input, &opts).await.unwrap().value;
    assert_eq!(out, ["gibbs phenomenon ringing", "overshoot near a jump"]);
    input.query = "a_n".into();
    input.purpose = RetrievalPurpose::Lesson;
    input.scope_names.push("lecture 4.mp4".into());
    rewrite_queries(&client, &input, &opts).await.unwrap();
    let candidates: Vec<RerankCandidate> = (1..=2)
        .map(|i| RerankCandidate::new(format!("p{i}"), "dsp.pdf", Some(i), "", "text"))
        .collect();
    let ranked = rerank(&client, "window leakage", &candidates, &opts)
        .await
        .unwrap()
        .value;
    assert_eq!(ranked[0].id, "p2");
    rerank(
        &client,
        "coefficients\nAlso asked: what is a_n",
        &candidates,
        &opts,
    )
    .await
    .unwrap();

    let sent = requests(&fake);
    assert_eq!(sent.len(), 4);
    for (i, req) in sent.iter().enumerate() {
        let body = &req.body;
        let constant = if i < 2 {
            REWRITE_CONSTANT
        } else {
            RERANK_CONSTANT
        };
        assert_eq!(
            body["system"],
            json!([{"type": "text", "text": constant, "cache_control": {"type": "ephemeral"}}]),
            "request {i}"
        );
        assert_eq!(breakpoints(body), 1, "request {i}");
        assert_eq!(
            body["model"], "claude-haiku-5-5",
            "request {i}: the small slot"
        );
        let content = body["messages"][0]["content"].as_array().unwrap();
        let last = content.last().unwrap()["text"].as_str().unwrap();
        assert!(last.contains("JSON schema"), "request {i}: {last}");
        assert!(!body["messages"].to_string().contains(&constant[..80]));
    }
    assert_eq!(sent[0].body["max_tokens"], REWRITE_MAX_OUTPUT_TOKENS);
    assert_ne!(sent[0].body["messages"], sent[1].body["messages"]);
    assert_ne!(sent[2].body["messages"], sent[3].body["messages"]);
    let system = |i: usize| serde_json::to_vec(&sent[i].body["system"][0]).unwrap();
    assert_eq!(
        system(0),
        system(1),
        "rewrite-v1's prefix is byte-identical"
    );
    assert_eq!(system(2), system(3), "rerank-v1's prefix is byte-identical");
    let first = sent[3].body["messages"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(
        first.starts_with("Query: coefficients\nAlso asked: what is a_n\n\nCandidates:\n"),
        "{first}"
    );
    assert!(first.contains("[p1] dsp.pdf, p. 1:\n"));
}

#[tokio::test]
async fn small_text_calls_with_an_image_and_no_split() {
    let (client, fake) = spawn(vec![text_reply("A title")]).await;
    let image = Image {
        media_type: "image/png".into(),
        data: vec![0x89, b'P', b'N', b'G'],
    };
    let out = client
        .stream_parts(
            "Name this chat.",
            &[
                InputPart::Text("Look: ".into()),
                InputPart::Image(image),
                InputPart::Text("What is it?".into()),
            ],
            &CallOptions::new(Route::Small).with_max_output_tokens(40),
            Box::new(|_| {}),
        )
        .await
        .unwrap();
    assert_eq!(out.text, "A title");
    let body = last_request(&fake).body;
    assert_eq!(body["model"], "claude-haiku-5-5");
    assert_eq!(body["max_tokens"], 40);
    assert_eq!(
        body["system"],
        json!([{"type": "text", "text": "Name this chat.", "cache_control": {"type": "ephemeral"}}])
    );
    assert_eq!(
        body["messages"][0]["content"],
        json!([
            {"type": "text", "text": "Look: "},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw=="}},
            {"type": "text", "text": "What is it?"}
        ])
    );
}

#[tokio::test]
async fn text_call_failures() {
    let cut = whole(response(
        vec![
            start(0, json!({"type": "text", "text": ""})),
            text_delta(0, "Half"),
            stop(0),
        ],
        "max_tokens",
    ));
    let overloaded = whole(sse(&[
        message_start("m"),
        json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
    ]));
    let (client, _) = spawn(vec![cut, overloaded]).await;
    let opts = CallOptions::new(Route::Write);
    let err = client
        .stream_parts("I", &[InputPart::Text("x".into())], &opts, Box::new(|_| {}))
        .await
        .unwrap_err();
    assert!(matches!(err, AiError::Incomplete(m) if m.contains("output token limit")));
    let err = client
        .stream_parts("I", &[InputPart::Text("x".into())], &opts, Box::new(|_| {}))
        .await
        .unwrap_err();
    assert!(err.is_retryable(), "{err:?}");
    assert_eq!(err.code(), Some("overloaded_error"));
}

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_models_reads_windows_across_pages() {
    let (client, fake) = spawn(vec![]).await;
    fake.model_pages.lock().unwrap().extend([
        json!({"data": [
            {"type": "model", "id": "claude-opus-5-5", "display_name": "Claude Opus 5.5",
             "max_input_tokens": 1000000, "max_tokens": 128000},
            {"type": "model", "id": "claude-sonnet-5-5", "display_name": "Claude Sonnet 5.5",
             "max_input_tokens": 1000000, "max_tokens": 128000}
        ], "has_more": true, "first_id": "claude-opus-5-5", "last_id": "claude-sonnet-5-5"}),
        json!({"data": [
            {"type": "model", "id": "claude-haiku-5-5", "display_name": "Claude Haiku 5.5",
             "max_input_tokens": 1000000, "max_tokens": 64000},
            {"type": "model", "id": "claude-old"}
        ], "has_more": false, "first_id": "claude-haiku-5-5", "last_id": "claude-old"}),
    ]);
    let models = client.list_models().await.unwrap();
    assert_eq!(models.len(), 4);
    assert_eq!(models[0].id, "claude-opus-5-5");
    assert_eq!(models[0].display_name.as_deref(), Some("Claude Opus 5.5"));
    assert_eq!(models[0].context_window, Some(1_000_000));
    assert_eq!(models[0].max_output_tokens, Some(128_000));
    assert_eq!(models[2].max_output_tokens, Some(64_000));
    assert_eq!(
        (models[3].context_window, models[3].max_output_tokens),
        (None, None)
    );
    let queries = fake.model_queries.lock().unwrap().clone();
    assert_eq!(
        queries,
        ["limit=1000", "limit=1000&after_id=claude-sonnet-5-5"]
    );
}

#[tokio::test]
async fn list_models_errors() {
    // Nothing listens on the discard port: a network error, not a panic.
    let client = ClaudeClient::new("http://127.0.0.1:9", "k", RouteModels::claude_default());
    assert!(matches!(
        client.list_models().await,
        Err(AiError::Network(_))
    ));
}

// ---------------------------------------------------------------------------
// `Retrying` on Claude: 529 overloaded, `retry-after` in seconds, the spend limit
// ---------------------------------------------------------------------------

/// What a `Retrying` model's `on_wait` was told, in order.
type Waits = Arc<Mutex<Vec<Option<margin_shared::Waiting>>>>;

fn retrying(client: ClaudeClient) -> (margin_ai::Retrying<ClaudeClient>, Waits) {
    let seen: Waits = Arc::new(Mutex::new(Vec::new()));
    let keep = seen.clone();
    let backoff = margin_ai::Backoff {
        retries: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(50),
        jitter: Duration::ZERO,
    };
    let model = margin_ai::Retrying::new(
        client,
        backoff,
        Box::new(move |w| keep.lock().unwrap().push(w)),
    );
    (model, seen)
}

async fn text(model: &dyn TextModel) -> (Result<String, AiError>, String) {
    let mut shown = String::new();
    let out = model
        .stream_parts(
            "Write it.",
            &[InputPart::Text("x".into())],
            &CallOptions::new(Route::Write),
            Box::new(|d| shown.push_str(d)),
        )
        .await
        .map(|o| o.text);
    (out, shown)
}

#[tokio::test]
async fn retrying_waits_out_overloaded_and_rate_limits_on_claude() {
    let overloaded_event = whole(sse(&[
        message_start("claude-sonnet-5-5"),
        json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
    ]));
    let (client, fake) = spawn(vec![
        Reply::Json(
            StatusCode::from_u16(529).unwrap(),
            api_error("overloaded_error", "Overloaded"),
        ),
        Reply::WithHeaders(
            StatusCode::TOO_MANY_REQUESTS,
            vec![("retry-after", "1".into())],
            api_error("rate_limit_error", "Slow down."),
        ),
        overloaded_event,
        text_reply("Lesson."),
    ])
    .await;
    let (model, seen) = retrying(client);
    let started = std::time::Instant::now();
    let (out, shown) = text(&model).await;
    assert_eq!(out.unwrap(), "Lesson.");
    assert_eq!(shown, "Lesson.");
    // The 429's `retry-after: 1` (seconds on Claude) was waited in full, past
    // `max_delay`.
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert_eq!(requests(&fake).len(), 4);
    let seen = seen.lock().unwrap();
    let reasons: Vec<_> = seen.iter().flatten().map(|w| w.reason.as_str()).collect();
    assert_eq!(
        reasons,
        [
            "The model API had a problem.",
            "The model API asked Margin to slow down.",
            "The model API had a problem.",
        ]
    );
    assert_eq!(seen.iter().filter(|w| w.is_none()).count(), 3);
}

#[tokio::test]
async fn retrying_stops_at_the_spend_limit_and_a_refused_key_on_claude() {
    for reply in [
        Reply::Json(
            StatusCode::TOO_MANY_REQUESTS,
            json!({"type": "error", "error": {"type": "rate_limit_error",
                "message": "You have reached your spend limit.",
                "details": {"error_code": "enforced_spend_limit_reached"}}}),
        ),
        Reply::Json(
            StatusCode::UNAUTHORIZED,
            api_error("authentication_error", "invalid x-api-key"),
        ),
    ] {
        let (client, fake) = spawn(vec![reply]).await;
        let (model, seen) = retrying(client);
        let (out, _) = text(&model).await;
        let err = out.unwrap_err();
        assert!(!err.is_retryable(), "{err:?}");
        assert_eq!(requests(&fake).len(), 1);
        assert!(seen.lock().unwrap().is_empty());
    }

    // Cut off after text was shown: returned as it is.
    let cut = whole(response(
        vec![
            start(0, json!({"type": "text", "text": ""})),
            text_delta(0, "Half"),
            stop(0),
        ],
        "max_tokens",
    ));
    let (client, fake) = spawn(vec![cut]).await;
    let (model, seen) = retrying(client);
    let (out, shown) = text(&model).await;
    assert!(matches!(out, Err(AiError::Incomplete(_))));
    assert_eq!(shown, "Half");
    assert_eq!(requests(&fake).len(), 1);
    assert!(seen.lock().unwrap().is_empty());
}
