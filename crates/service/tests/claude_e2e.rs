//! The providers phase end to end on Claude (`docs/agent-providers-brief.md` §5,
//! "Integrator e2e"): the twins of the phase-1 chat test and of R1's course on the
//! corpus, through the real `ClaudeClient` against a fake Messages API that streams the
//! documented SSE shapes and lists its models. A chat on Claude runs a Margin tool,
//! replays its thinking and tool blocks byte for byte, searches the web (rows, card,
//! citations), stops with a server tool pending, survives a restart with a call open
//! and continues; a course is planned and written on Claude with the passages packed to
//! the window the fake advertises less the answer's reserve. Every model call is traced
//! with `provider: "claude"`, its model and a price. No real provider is called.

mod common;

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use common::*;
use futures_util::StreamExt as _;
use margin_agent::RetryPolicy;
use margin_service::ask::{CoursePlan, PlannedLesson};
use margin_service::db::PassageRow;
use margin_service::providers::Endpoints;
use margin_service::secrets::{GITHUB_CLIENT_SECRET, MemoryStore};
use margin_service::turns::{CALL_INTERRUPTED, STOPPED, STOPPED_BY_RESTART};
use margin_shared::context::{TraceKind, TraceRow};
use margin_shared::pipeline::Route;
use margin_shared::{
    Card, ChatDetail, ChatItem, ChatItemKind, ChatState, ItemStatus, Lesson, LessonStatus, Library,
    Proposal, Source, SourceStatus, Topic,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;

// ---------------------------------------------------------------------------
// Fake Messages API
// ---------------------------------------------------------------------------

enum Reply {
    /// The whole SSE body, then the connection ends.
    Stream(String),
    /// These SSE events, then the connection stays open without another byte.
    Hang(String),
}

struct FakeClaude {
    replies: Mutex<VecDeque<Reply>>,
    /// Every `POST /v1/messages`: its `x-api-key` and JSON body.
    requests: Mutex<Vec<(Option<String>, Value)>>,
    /// What `GET /v1/models` lists: `(id, max_input_tokens, max_tokens)`.
    models: Mutex<Vec<(String, u32, u32)>>,
}

impl FakeClaude {
    fn push(&self, reply: Reply) {
        self.replies.lock().unwrap().push_back(reply);
    }

    fn bodies(&self) -> Vec<Value> {
        let requests = self.requests.lock().unwrap();
        requests.iter().map(|(_, b)| b.clone()).collect()
    }
}

async fn messages(
    State(fake): State<Arc<FakeClaude>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    fake.requests.lock().unwrap().push((key, body));
    let Some(reply) = fake.replies.lock().unwrap().pop_front() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"type": "error", "error": {"type": "api_error", "message": "no reply scripted"}})),
        )
            .into_response();
    };
    let body = match reply {
        Reply::Stream(sse) => Body::from(sse),
        Reply::Hang(sse) => Body::from_stream(
            futures_util::stream::once(async move { Ok::<_, Infallible>(Bytes::from(sse)) })
                .chain(futures_util::stream::pending()),
        ),
    };
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

async fn models(State(fake): State<Arc<FakeClaude>>) -> Json<Value> {
    let data: Vec<Value> = fake
        .models
        .lock()
        .unwrap()
        .iter()
        .map(|(id, input, output)| {
            json!({"type": "model", "id": id, "display_name": id,
                "max_input_tokens": input, "max_tokens": output})
        })
        .collect();
    Json(json!({"data": data, "has_more": false}))
}

/// The fake's base URL and its state, listing Claude's three default models.
async fn start_fake_claude() -> (String, Arc<FakeClaude>) {
    let fake = Arc::new(FakeClaude {
        replies: Mutex::new(VecDeque::new()),
        requests: Mutex::new(Vec::new()),
        models: Mutex::new(vec![
            ("claude-opus-5-5".into(), 500_000, 64_000),
            ("claude-sonnet-5-5".into(), 400_000, 64_000),
            ("claude-haiku-5-5".into(), 200_000, 64_000),
        ]),
    });
    let app = Router::new()
        .route("/v1/messages", post(messages))
        .route("/v1/models", get(models))
        .with_state(fake.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, fake)
}

// ---------------------------------------------------------------------------
// SSE bodies, in the documented shapes
// ---------------------------------------------------------------------------

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|v| format!("event: {}\ndata: {}\n\n", v["type"].as_str().unwrap(), v))
        .collect()
}

/// `message_start` with the model and the cache numbers of a warm prefix.
fn message_start(model: &str) -> Value {
    json!({"type": "message_start", "message": {
        "id": "msg_x", "type": "message", "role": "assistant", "model": model,
        "content": [], "stop_reason": null,
        "usage": {"input_tokens": 200, "cache_read_input_tokens": 800,
                  "cache_creation_input_tokens": 100, "output_tokens": 1}
    }})
}

fn start(index: u32, block: Value) -> Value {
    json!({"type": "content_block_start", "index": index, "content_block": block})
}

fn delta(index: u32, delta: Value) -> Value {
    json!({"type": "content_block_delta", "index": index, "delta": delta})
}

fn stop(index: u32) -> Value {
    json!({"type": "content_block_stop", "index": index})
}

fn text_block(index: u32, pieces: &[&str]) -> Vec<Value> {
    let mut v = vec![start(index, json!({"type": "text", "text": ""}))];
    v.extend(
        pieces
            .iter()
            .map(|p| delta(index, json!({"type": "text_delta", "text": p}))),
    );
    v.push(stop(index));
    v
}

fn thinking_block(index: u32, pieces: &[&str], signature: &str) -> Vec<Value> {
    let mut v = vec![start(
        index,
        json!({"type": "thinking", "thinking": "", "signature": ""}),
    )];
    v.extend(
        pieces
            .iter()
            .map(|p| delta(index, json!({"type": "thinking_delta", "thinking": p}))),
    );
    v.push(delta(
        index,
        json!({"type": "signature_delta", "signature": signature}),
    ));
    v.push(stop(index));
    v
}

fn tool_use_block(index: u32, id: &str, name: &str, args: &str) -> Vec<Value> {
    vec![
        start(
            index,
            json!({"type": "tool_use", "id": id, "name": name, "input": {}}),
        ),
        delta(
            index,
            json!({"type": "input_json_delta", "partial_json": args}),
        ),
        stop(index),
    ]
}

fn web_search_block(index: u32, id: &str, query: &str) -> Vec<Value> {
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

/// A whole response: these blocks, then the stop reason and the usage.
fn response(model: &str, blocks: Vec<Value>, stop_reason: &str, usage: Value) -> String {
    let mut events = vec![message_start(model)];
    events.extend(blocks);
    events.push(json!({"type": "message_delta",
        "delta": {"stop_reason": stop_reason, "stop_sequence": null}, "usage": usage}));
    events.push(json!({"type": "message_stop"}));
    sse(&events)
}

/// The start of a response and these blocks, with no end.
fn partial(model: &str, blocks: Vec<Value>) -> String {
    let mut events = vec![message_start(model)];
    events.extend(blocks);
    sse(&events)
}

const OPUS: &str = "claude-opus-5-5";

fn out_usage() -> Value {
    json!({"output_tokens": 40})
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A service with a Claude key whose chats and jobs call the fake Messages API through
/// the real client. Claude is the default provider (new chats, the writing route).
async fn start_service(
    store: Option<margin_service::db::Store>,
    claude: &str,
) -> (Harness, String) {
    let h = start_with(Setup {
        store,
        secrets: MemoryStore::new().with(GITHUB_CLIENT_SECRET, CLIENT_SECRET),
        ..Setup::default()
    })
    .await;
    h.generator.agent_over_http.store(true, Ordering::SeqCst);
    h.generator.jobs_over_http.store(true, Ordering::SeqCst);
    *h.state.turns.retry.lock().unwrap() = RetryPolicy::immediate();
    h.state.providers.set_endpoints(Endpoints {
        anthropic: claude.to_string(),
        ..Endpoints::default()
    });
    let t = h.sign_in("Mac").await;
    let r = h
        .patch(&t, "/api/settings")
        .json(&json!({"anthropic_api_key": CLAUDE_KEY, "default_provider": "claude"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    (h, t)
}

async fn detail(h: &Harness, token: &str, chat_id: &str) -> ChatDetail {
    let r = h
        .get(token, &format!("/api/chats/{chat_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

async fn wait_chat(
    h: &Harness,
    token: &str,
    chat_id: &str,
    done: impl Fn(&ChatDetail) -> bool,
) -> ChatDetail {
    eventually(|| async {
        let d = detail(h, token, chat_id).await;
        done(&d).then_some(d)
    })
    .await
}

fn line(item: &ChatItem) -> String {
    let status = match item.status {
        ItemStatus::InProgress => "…",
        ItemStatus::Done => "",
        ItemStatus::Cancelled => " (cancelled)",
    };
    let what = match &item.kind {
        ChatItemKind::UserMessage { text, .. } => format!("user: {text}"),
        ChatItemKind::AssistantText { text, .. } => format!("assistant: {text}"),
        ChatItemKind::Reasoning { summary } => format!("reasoning: {summary}"),
        ChatItemKind::ToolCall { name, label, .. } => format!("call {name}: {label}"),
        ChatItemKind::ToolResult { ok, summary, .. } => {
            format!("result {}: {summary}", if *ok { "ok" } else { "failed" })
        }
        ChatItemKind::Card { card, .. } => match card {
            Card::WebResults(_) => "card web results".into(),
            _ => "card".into(),
        },
        ChatItemKind::Action { text } => format!("action: {text}"),
        ChatItemKind::Error { message, .. } => format!("error: {message}"),
    };
    format!("{what}{status}")
}

fn lines(d: &ChatDetail) -> Vec<String> {
    d.items.iter().map(line).collect()
}

/// A request's messages without the moving `cache_control` breakpoint (the client
/// marks the last block of each request; the bytes before it are what must repeat).
fn messages_of(body: &Value) -> Vec<Value> {
    let mut v = body["messages"].clone();
    for m in v.as_array_mut().unwrap() {
        if let Some(blocks) = m["content"].as_array_mut() {
            for b in blocks {
                if let Some(o) = b.as_object_mut() {
                    o.remove("cache_control");
                }
            }
        }
    }
    v.as_array().unwrap().clone()
}

/// Every block of `messages`, in order, with its message's role.
fn blocks(messages: &[Value]) -> Vec<(String, Value)> {
    messages
        .iter()
        .flat_map(|m| {
            let role = m["role"].as_str().unwrap().to_string();
            m["content"]
                .as_array()
                .unwrap()
                .iter()
                .map(move |b| (role.clone(), b.clone()))
                .collect::<Vec<_>>()
        })
        .collect()
}

async fn traces_of(h: &Harness, t: &str, trace_id: &str) -> Vec<TraceRow> {
    let r = h
        .get(t, "/api/traces")
        .query(&[("trace_id", trace_id)])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.text()
        .await
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// The turn's model_call rows, once `n` of them are written.
async fn model_calls(h: &Harness, t: &str, turn_id: &str, n: usize) -> Vec<TraceRow> {
    eventually(|| async {
        let rows: Vec<TraceRow> = traces_of(h, t, turn_id)
            .await
            .into_iter()
            .filter(|r| r.kind == TraceKind::ModelCall && r.route == Some(Route::Chat))
            .collect();
        (rows.len() >= n).then_some(rows)
    })
    .await
}

// ---------------------------------------------------------------------------
// 1. The phase-1 chat, on Claude
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_claude_chat_runs_through_the_real_client_tools_web_replay_stop_and_restart() {
    let (claude, fake) = start_fake_claude().await;
    let (h, t) = start_service(None, &claude).await;
    let r = h
        .post(&t, "/api/topics")
        .json(&json!({"parent_id": null, "title": "Photosynthesis"}))
        .send()
        .await
        .unwrap();
    let topic: Topic = r.json().await.unwrap();

    // --- Turn 1: thinking, a library search, then the answer. ---
    let signature = "EqQBCgIYAhIM1gbcDa+/==";
    fake.push(Reply::Stream(response(
        OPUS,
        [
            thinking_block(0, &["Search the ", "library."], signature),
            tool_use_block(
                1,
                "toolu_1",
                "search_library",
                "{\"query\":\"photosynthesis\"}",
            ),
        ]
        .concat(),
        "tool_use",
        out_usage(),
    )));
    fake.push(Reply::Stream(response(
        OPUS,
        text_block(0, &["You have a topic ", "on photosynthesis."]),
        "end_turn",
        out_usage(),
    )));
    let r = h
        .post(&t, "/api/chats")
        .json(&json!({"message": {"text": "What do I have on photosynthesis?"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let created: ChatDetail = r.json().await.unwrap();
    assert_eq!(created.chat.provider, margin_shared::Provider::Claude);
    let chat_id = created.chat.id.clone();
    let d = wait_chat(&h, &t, &chat_id, |d| {
        d.chat.state == ChatState::Idle && d.items.len() == 5
    })
    .await;
    let label = "Searching the library for “photosynthesis”";
    let l = lines(&d);
    assert_eq!(l[0], "user: What do I have on photosynthesis?");
    assert_eq!(l[1], "reasoning: Search the library.");
    assert_eq!(l[2], format!("call search_library: {label}"));
    assert!(l[3].starts_with("result ok: "), "{l:#?}");
    assert_eq!(l[4], "assistant: You have a topic on photosynthesis.");
    assert_eq!(d.chat.usage.model_calls, 2);

    // What reached the API: the key, the route's model, the cached system prompt, the
    // tools as input schemas, and the second call replaying the first one's blocks
    // verbatim (the thinking signature included) with the tool's result.
    {
        let requests = fake.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|(key, _)| key.as_deref() == Some(CLAUDE_KEY))
        );
    }
    let bodies = fake.bodies();
    let first = &bodies[0];
    assert_eq!(first["model"], OPUS);
    assert_eq!(first["stream"], true);
    assert_eq!(first["system"][0]["cache_control"]["type"], "ephemeral");
    let tools = first["tools"].as_array().unwrap();
    let search = tools
        .iter()
        .find(|t| t["name"] == "search_library")
        .expect("search_library is offered");
    assert!(search["input_schema"].is_object());
    assert!(
        tools.iter().any(|t| t["type"]
            .as_str()
            .is_some_and(|ty| ty.starts_with("web_search_"))),
        "the hosted web search is offered"
    );
    assert_eq!(
        bodies[1]["tools"], first["tools"],
        "the same tools every call"
    );
    let first_messages = messages_of(first);
    let second_messages = messages_of(&bodies[1]);
    assert_eq!(
        &second_messages[..first_messages.len()],
        &first_messages[..]
    );
    let added = blocks(&second_messages[first_messages.len()..]);
    assert_eq!(added[0].0, "assistant");
    assert_eq!(
        added[0].1,
        json!({"type": "thinking", "thinking": "Search the library.", "signature": signature})
    );
    assert_eq!(
        added[1].1,
        json!({"type": "tool_use", "id": "toolu_1", "name": "search_library", "input": {"query": "photosynthesis"}})
    );
    assert_eq!(added[2].0, "user");
    assert_eq!(added[2].1["type"], "tool_result");
    assert_eq!(added[2].1["tool_use_id"], "toolu_1");
    let result_text = added[2].1["content"].to_string();
    assert!(result_text.contains(&topic.id), "{result_text}");

    // Both steps are traced on Claude: the model the fake answered with, the cache
    // write, a price from prices.toml, the agent's prompt version.
    let turn_id = d.items[0].turn_id.clone();
    let calls = model_calls(&h, &t, &turn_id, 2).await;
    for c in &calls {
        assert_eq!(c.provider.as_deref(), Some("claude"), "{c:#?}");
        assert_eq!(c.model.as_deref(), Some(OPUS));
        assert_eq!(
            c.prompt_version.as_deref(),
            Some(margin_agent::AGENT_PROMPT_VERSION)
        );
        let usage = c.usage.unwrap();
        assert_eq!(usage.cache_write, 100);
        assert_eq!(usage.cached, 800);
        // Every input token: 200 fresh + 800 read + 100 written.
        assert_eq!(usage.input, 1100);
        assert!(c.cost_usd.is_some_and(|p| p > 0.0), "{c:#?}");
    }

    // --- Turn 2: a web search with its card and a cited answer; the whole chat is
    // replayed byte for byte. ---
    let results = json!([
        {"type": "web_search_result", "url": "https://example.com/chlorophyll",
         "title": "Chlorophyll", "encrypted_content": "EqgfCioIARgBIiQ3", "page_age": "May 2026"},
        {"type": "web_search_result", "url": "https://example.com/calvin",
         "title": "Calvin cycle", "encrypted_content": "EqgfCioIARgBIiQ4"}
    ]);
    let cite = json!({"type": "web_search_result_location", "url": "https://example.com/chlorophyll",
        "title": "Chlorophyll", "encrypted_index": "Eo8BCioIAhgB", "cited_text": "Chlorophyll absorbs red and blue."});
    let mut web = web_search_block(0, "srvtoolu_1", "chlorophyll absorption");
    web.extend([
        start(
            1,
            json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": results}),
        ),
        stop(1),
        start(2, json!({"type": "text", "text": "", "citations": []})),
        delta(2, json!({"type": "citations_delta", "citation": cite})),
        delta(
            2,
            json!({"type": "text_delta", "text": "It absorbs red and blue light."}),
        ),
        stop(2),
    ]);
    fake.push(Reply::Stream(response(
        OPUS,
        web,
        "end_turn",
        json!({"output_tokens": 40, "server_tool_use": {"web_search_requests": 1}}),
    )));
    let r = h
        .post(&t, &format!("/api/chats/{chat_id}/messages"))
        .json(&json!({"text": "What does chlorophyll absorb?"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let d = wait_chat(&h, &t, &chat_id, |d| {
        d.chat.state == ChatState::Idle
            && d.items
                .last()
                .is_some_and(|i| matches!(i.kind, ChatItemKind::AssistantText { .. }))
            && d.items.len() > 6
    })
    .await;
    let l = lines(&d);
    assert_eq!(l[5], "user: What does chlorophyll absorb?");
    assert!(
        l[6].starts_with("call web_search: ") && l[6].contains("chlorophyll absorption"),
        "{l:#?}"
    );
    assert!(l.iter().any(|x| x == "card web results"), "{l:#?}");
    let card = d
        .items
        .iter()
        .find_map(|i| match &i.kind {
            ChatItemKind::Card {
                card: Card::WebResults(c),
                ..
            } => Some(c.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(card.results.len(), 2);
    assert_eq!(card.results[0].url, "https://example.com/chlorophyll");
    let answer = d.items.last().unwrap();
    let ChatItemKind::AssistantText { text, citations } = &answer.kind else {
        panic!("{answer:?}")
    };
    assert_eq!(text, "It absorbs red and blue light.");
    assert_eq!(citations.len(), 1);
    assert_eq!(citations[0].url, "https://example.com/chlorophyll");
    let bodies = fake.bodies();
    let third_messages = messages_of(&bodies[2]);
    assert_eq!(
        &third_messages[..second_messages.len()],
        &second_messages[..]
    );
    let added = blocks(&third_messages[second_messages.len()..]);
    assert_eq!(
        added[0],
        (
            "assistant".to_string(),
            json!({"type": "text", "text": "You have a topic on photosynthesis."})
        )
    );
    assert_eq!(added.last().unwrap().0, "user");
    assert_eq!(
        added.last().unwrap().1["text"],
        "What does chlorophyll absorb?"
    );
    let turn_id = d.items[5].turn_id.clone();
    let web_call = model_calls(&h, &t, &turn_id, 1).await;
    assert_eq!(web_call[0].usage.unwrap().web_searches, 1);
    assert_eq!(web_call[0].provider.as_deref(), Some("claude"));
    assert!(web_call[0].cost_usd.is_some());

    // --- Turn 3: Stop while a web search is pending on the server. ---
    fake.push(Reply::Hang(partial(
        OPUS,
        web_search_block(0, "srvtoolu_2", "light reactions"),
    )));
    let r = h
        .post(&t, &format!("/api/chats/{chat_id}/messages"))
        .json(&json!({"text": "And the light reactions?"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let before = d.items.len();
    wait_chat(&h, &t, &chat_id, |d| {
        d.items[before..]
            .iter()
            .any(|i| matches!(&i.kind, ChatItemKind::ToolCall { name, .. } if name == "web_search"))
    })
    .await;
    let r = h
        .post(&t, &format!("/api/chats/{chat_id}/stop"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let d = wait_chat(&h, &t, &chat_id, |d| d.chat.state == ChatState::Idle).await;
    let l = lines(&d);
    assert_eq!(l[before], "user: And the light reactions?");
    let stopped_search = d.items[before..]
        .iter()
        .find(|i| matches!(&i.kind, ChatItemKind::ToolCall { name, .. } if name == "web_search"))
        .unwrap();
    assert_eq!(
        stopped_search.status,
        ItemStatus::Cancelled,
        "the open web row is cancelled: {l:#?}"
    );
    assert_eq!(l.last().unwrap(), &format!("action: {STOPPED}"));

    // --- Turn 4: the service stops while a tool call is open. ---
    fake.push(Reply::Hang(partial(
        OPUS,
        tool_use_block(0, "toolu_5", "search_library", "{\"query\":\"leaves\"}"),
    )));
    let r = h
        .post(&t, &format!("/api/chats/{chat_id}/messages"))
        .json(&json!({"text": "Look for leaves"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    wait_chat(&h, &t, &chat_id, |d| {
        d.items.last().is_some_and(|i| {
            matches!(&i.kind, ChatItemKind::ToolCall { call_id, label, .. } if call_id == "toolu_5" && !label.is_empty())
        })
    })
    .await;
    let before = detail(&h, &t, &chat_id).await.items.len() - 2;
    let store = h.state.store.clone();
    h.state.turns.abort(&chat_id);
    let mut h = h;
    let _ = h.stop.take().unwrap().send(());
    let (h2, t2) = start_service(Some(store), &claude).await;
    let d = detail(&h2, &t, &chat_id).await;
    let _ = t2;
    assert!(
        matches!(d.chat.state, ChatState::Interrupted { .. }),
        "{:?}",
        d.chat.state
    );
    assert_eq!(
        lines(&d)[before..],
        [
            "user: Look for leaves".to_string(),
            "call search_library: Searching the library for “leaves” (cancelled)".into(),
            "result failed: Interrupted".into(),
            format!("action: {STOPPED_BY_RESTART}"),
        ]
    );

    // --- Continue picks the turn up again through the restarted service. ---
    fake.push(Reply::Stream(response(
        OPUS,
        text_block(0, &["Let me check again."]),
        "end_turn",
        out_usage(),
    )));
    let r = h2
        .post(&t, &format!("/api/chats/{chat_id}/continue"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let d = wait_chat(&h2, &t, &chat_id, |d| {
        d.chat.state == ChatState::Idle
            && lines(d).last().map(String::as_str) == Some("assistant: Let me check again.")
    })
    .await;
    assert!(d.items.windows(2).all(|w| w[0].seq < w[1].seq));

    let bodies = fake.bodies();
    assert_eq!(bodies.len(), 6);
    let resumed = messages_of(&bodies[5]);
    let all = blocks(&resumed);
    // The interrupted call is answered right after it, in the next user message.
    let at = all
        .iter()
        .position(|(_, b)| b["type"] == "tool_use" && b["id"] == "toolu_5")
        .expect("the interrupted call is replayed");
    let (role, result) = &all[at + 1];
    assert_eq!(role, "user");
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["tool_use_id"], "toolu_5");
    assert!(result.to_string().contains(CALL_INTERRUPTED), "{result}");
    // The stopped search had no result: it isn't replayed as an open server call.
    assert!(
        !all.iter()
            .any(|(_, b)| b["type"] == "server_tool_use" && b["id"] == "srvtoolu_2"),
        "{all:#?}"
    );
    let text = serde_json::to_string(&resumed).unwrap();
    assert!(text.contains(STOPPED));
    assert!(text.contains(STOPPED_BY_RESTART));
    // Roles alternate (same-role entries are merged into one message).
    assert!(resumed.windows(2).all(|w| w[0]["role"] != w[1]["role"]));
    // Everything up to the stopped turn is still the same bytes as before.
    assert_eq!(&resumed[..third_messages.len()], &third_messages[..]);
}

// ---------------------------------------------------------------------------
// 2. R1's course on the corpus, on Claude
// ---------------------------------------------------------------------------

fn corpus(path: &str) -> String {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../evals/corpus")
        .join(path);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

async fn library(h: &Harness, t: &str) -> Library {
    let r = h.get(t, "/api/library").send().await.unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

async fn upload_corpus(h: &Harness, t: &str, path: &str, topic: Option<&str>) -> Source {
    let name = path.rsplit('/').next().unwrap();
    let r = h.upload(t, name, topic, corpus(path)).await;
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

async fn wait_ready(h: &Harness, t: &str, ids: &[&str]) {
    eventually(|| async {
        let lib = library(h, t).await;
        ids.iter()
            .all(|id| {
                lib.sources
                    .iter()
                    .any(|s| s.id == *id && matches!(s.status, SourceStatus::Ready { .. }))
            })
            .then_some(())
    })
    .await;
}

/// The writing model's window, small enough that the budget binds: 4 000 input tokens
/// of which half are the share, less 1 200 for the answer.
const WRITE_INPUT: u32 = 4_000;
const WRITE_OUTPUT: u32 = 1_200;

#[tokio::test(flavor = "multi_thread")]
async fn a_claude_course_on_the_corpus_packs_to_the_window_less_the_answer_and_cites() {
    let (claude, fake) = start_fake_claude().await;
    // Opus (chats and writing) advertises a small window.
    fake.models.lock().unwrap()[0] = (OPUS.into(), WRITE_INPUT, WRITE_OUTPUT);
    let (h, t) = start_service(None, &claude).await;
    let r = h
        .post(&t, "/api/topics")
        .json(&json!({"parent_id": null, "title": "Corpus"}))
        .send()
        .await
        .unwrap();
    let topic: Topic = r.json().await.unwrap();
    let mut docs = Vec::new();
    for f in ["bayes.md", "fourier-series.md", "photosynthesis.md"] {
        docs.push(upload_corpus(&h, &t, f, Some(&topic.id)).await);
    }
    let rc = upload_corpus(&h, &t, "sources/rc-circuits.txt", None).await;
    docs.push(rc);
    let ids: Vec<&str> = docs.iter().map(|s| s.id.as_str()).collect();
    wait_ready(&h, &t, &ids).await;
    let mut rows_by_id: HashMap<String, PassageRow> = HashMap::new();
    for s in &docs {
        let rows = h.state.store.passages_of(&s.id).await.unwrap();
        rows_by_id.extend(rows.into_iter().map(|r| (r.id.clone(), r)));
    }

    // The chat: the model proposes a course with a survey; the planner runs on Claude.
    let source_ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
    let draft = json!({
        "prompt": "A course from my notes",
        "topic_title": "Draft course",
        "subject_title": "Science",
        "lessons": [
            {"title": "Draft one", "summary": "First", "minutes": 5},
            {"title": "Draft two", "summary": "Second", "minutes": 5},
        ],
        "level": "intermediate",
        "with_videos": false,
        "with_quiz": false,
        "parent_topic_id": topic.id,
        "source_ids": source_ids,
        "survey": true,
    });
    let planned = |title: &str, queries: [&str; 2]| PlannedLesson {
        title: title.into(),
        summary: format!("{title}, from the notes"),
        minutes: 10,
        queries: queries.iter().map(|q| q.to_string()).collect(),
        must_cite: vec![],
    };
    let plan = CoursePlan {
        topic_title: "Planned course".into(),
        subject_title: "Science".into(),
        lessons: vec![
            planned(
                "Bayes and base rates",
                [
                    "prior posterior likelihood evidence",
                    "base rate fallacy false positives",
                ],
            ),
            planned(
                "Capacitors charging",
                [
                    "capacitor resistor time constant",
                    "charging current battery voltage",
                ],
            ),
        ],
    };
    fake.push(Reply::Stream(response(
        OPUS,
        tool_use_block(0, "toolu_p", "propose_course", &draft.to_string()),
        "tool_use",
        out_usage(),
    )));
    // The planner's answer, fenced as a model might (the client unwraps it).
    fake.push(Reply::Stream(response(
        OPUS,
        text_block(
            0,
            &[&format!(
                "```json\n{}\n```",
                serde_json::to_string(&plan).unwrap()
            )],
        ),
        "end_turn",
        out_usage(),
    )));
    fake.push(Reply::Stream(response(
        OPUS,
        text_block(0, &["Here is a plan."]),
        "end_turn",
        out_usage(),
    )));
    let r = h
        .post(&t, "/api/chats")
        .json(&json!({"message": {"text": "A course from my notes, please", "source_ids": [rc_id(&docs)]}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let chat: ChatDetail = r.json().await.unwrap();
    let d = wait_chat(&h, &t, &chat.chat.id, |d| {
        d.chat.state == ChatState::Idle
            && d.items
                .iter()
                .any(|i| matches!(&i.kind, ChatItemKind::AssistantText { text, .. } if text == "Here is a plan."))
    })
    .await;
    let turn_id = d.items[0].turn_id.clone();

    // The planner was a Claude text call on the writing model: its schema asked in
    // words after the cached constant block.
    let bodies = fake.bodies();
    let planner = bodies
        .iter()
        .find(|b| b.get("tools").is_none())
        .expect("the planner call");
    assert_eq!(planner["model"], OPUS);
    assert_eq!(planner["system"][0]["cache_control"]["type"], "ephemeral");
    assert!(
        planner["messages"].to_string().contains("JSON schema"),
        "{planner}"
    );
    let proposal_id = d
        .items
        .iter()
        .find_map(|i| match &i.kind {
            ChatItemKind::Card {
                card: Card::Outline(c),
                ..
            } => Some(c.proposal_id.clone()),
            _ => None,
        })
        .expect("an outline card");
    let proposal: Proposal = h
        .get(&t, &format!("/api/proposals/{proposal_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let outline = proposal.outline.clone().unwrap();
    assert_eq!(outline.lessons[0].title, "Bayes and base rates");
    let turn_rows = traces_of(&h, &t, &turn_id).await;
    let plan_row = turn_rows
        .iter()
        .find(|r| r.kind == TraceKind::ModelCall && r.prompt_version.as_deref() == Some("plan-v1"))
        .expect("the planner is traced");
    assert_eq!(plan_row.provider.as_deref(), Some("claude"));
    assert_eq!(plan_row.route, Some(Route::Write));
    assert!(plan_row.cost_usd.is_some());

    // Accept: the lessons are written on Claude, each streamed body citing a passage.
    for _ in 0..2 {
        fake.push(Reply::Stream(response(
            OPUS,
            text_block(0, &["From the notes.", "[^1]"]),
            "end_turn",
            out_usage(),
        )));
    }
    let r = h
        .post(&t, &format!("/api/proposals/{proposal_id}/accept"))
        .json(&json!({"outline": outline}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let course: Topic = r.json().await.unwrap();
    let lessons: Vec<Lesson> = eventually(|| async {
        let ls: Vec<Lesson> = library(&h, &t)
            .await
            .lessons
            .into_iter()
            .filter(|l| l.topic_id == course.id)
            .collect();
        (ls.len() == 2 && ls.iter().all(|l| l.status == LessonStatus::Ready)).then_some(ls)
    })
    .await;

    // D13: floor(window × share_of_window) − min(window.output, the text cap).
    let share = h.state.pipeline.load_full().config.budget.share_of_window as f64;
    let budget = (WRITE_INPUT as f64 * share) as u64 - WRITE_OUTPUT as u64;
    for lesson in &lessons {
        let rows = traces_of(&h, &t, &lesson.id).await;
        let retrieval: Vec<&TraceRow> = rows
            .iter()
            .filter(|r| r.kind == TraceKind::Retrieval)
            .collect();
        assert_eq!(retrieval.len(), 1, "{}", lesson.title);
        let packed = &retrieval[0].packed_ids;
        assert!(!packed.is_empty(), "{}", lesson.title);
        let tokens = retrieval[0].packed_tokens.unwrap();
        assert!(
            tokens <= budget,
            "{}: {tokens} packed tokens over the {budget}-token budget",
            lesson.title
        );
        // The lesson call: Claude, the writing model, its prompt version, a price.
        let call = rows
            .iter()
            .find(|r| r.kind == TraceKind::ModelCall)
            .expect("the lesson call is traced");
        assert_eq!(call.provider.as_deref(), Some("claude"));
        assert_eq!(call.model.as_deref(), Some(OPUS));
        assert_eq!(call.prompt_version.as_deref(), Some("lesson-v2"));
        assert_eq!(call.usage.unwrap().cache_write, 100);
        assert!(call.cost_usd.is_some());
        // Citations resolve to a document and its lines.
        assert_eq!(lesson.citations.len(), 1, "{}", lesson.title);
        let c = &lesson.citations[0];
        let doc = c.document_id.as_deref().expect("a document id");
        assert!(ids.contains(&doc));
        assert!(rows_by_id.values().any(|r| r.document_id == doc
            && r.start_line == c.start_line.map(i64::from)
            && c.end_line.is_some()));
    }
    // The lesson requests sent the constant block cached and the passages after it.
    let lesson_bodies: Vec<Value> = fake
        .bodies()
        .into_iter()
        .filter(|b| {
            b["system"][0]["text"]
                .as_str()
                .is_some_and(|s| s == margin_ai::prompts::LESSON_CONSTANT)
        })
        .collect();
    assert_eq!(lesson_bodies.len(), 2);
    for b in &lesson_bodies {
        assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");
        assert!(b["system"][1].get("cache_control").is_none());
    }
}

fn rc_id(docs: &[Source]) -> String {
    docs.iter()
        .find(|s| s.name == "rc-circuits.txt")
        .unwrap()
        .id
        .clone()
}
