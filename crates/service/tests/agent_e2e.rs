//! The whole agent path in one test: a chat message over HTTP → the service's turn
//! runner → the agent loop → the real `ResponsesClient` against a fake Responses API that
//! streams real SSE → a read-only library tool → the final answer, with the stored items,
//! the socket's events, replay on the next turn, Stop, and Continue after a simulated
//! restart of the service on the same store. A second test takes a course through the
//! same path (phase 2): `propose_course` and its outline card, Create lessons with chips,
//! the chat's record of it on the next turn, a delete parked on its confirmation card and
//! approved from another device, and owner decision Q2 for overwriting notes.

mod common;

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use common::*;
use futures_util::StreamExt as _;
use margin_agent::RetryPolicy;
use margin_service::db;
use margin_service::secrets::{GITHUB_CLIENT_SECRET, MemoryStore, OPENAI_API_KEY};
use margin_service::state::{Author, StoredNote};
use margin_service::turns::{CALL_INTERRUPTED, STOPPED, STOPPED_BY_RESTART};
use margin_shared::{
    Card, ChatDetail, ChatItem, ChatItemKind, ChatState, Decision, Event, ItemStatus, Lesson,
    LessonStatus, Level, Library, Me, Note, Proposal, ProposalStatus, ToolOutputText, Topic,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;

// ---------------------------------------------------------------------------
// Fake Responses API
// ---------------------------------------------------------------------------

enum Reply {
    /// The whole SSE body, then the connection ends.
    Stream(String),
    /// These SSE events, then the connection stays open without another byte.
    Hang(String),
}

#[derive(Default)]
struct FakeOpenAi {
    replies: Mutex<VecDeque<Reply>>,
    /// Every request: its `Authorization` header and JSON body.
    requests: Mutex<Vec<(Option<String>, Value)>>,
}

impl FakeOpenAi {
    fn push(&self, reply: Reply) {
        self.replies.lock().unwrap().push_back(reply);
    }

    fn bodies(&self) -> Vec<Value> {
        let requests = self.requests.lock().unwrap();
        requests.iter().map(|(_, b)| b.clone()).collect()
    }
}

async fn responses(
    State(fake): State<Arc<FakeOpenAi>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    fake.requests.lock().unwrap().push((auth, body));
    let Some(reply) = fake.replies.lock().unwrap().pop_front() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "no reply scripted").into_response();
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

/// The fake's base URL (for Settings) and its state.
async fn start_fake_openai() -> (String, Arc<FakeOpenAi>) {
    let fake = Arc::new(FakeOpenAi::default());
    let app = Router::new()
        .route("/v1/responses", post(responses))
        .with_state(fake.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, fake)
}

// ---------------------------------------------------------------------------
// SSE bodies, in the shapes the API streams
// ---------------------------------------------------------------------------

fn event(v: Value) -> String {
    format!("event: {}\ndata: {}\n\n", v["type"].as_str().unwrap(), v)
}

fn created(id: &str) -> String {
    event(
        json!({"type": "response.created", "response": {"id": id, "status": "in_progress", "output": []}}),
    )
}

fn completed(id: &str) -> String {
    event(json!({"type": "response.completed", "response": {
        "id": id, "status": "completed", "output": [],
        "usage": {
            "input_tokens": 1000, "input_tokens_details": {"cached_tokens": 800},
            "output_tokens": 50, "output_tokens_details": {"reasoning_tokens": 20},
            "total_tokens": 1050
        }
    }}))
}

fn reasoning_done(id: &str, summary: &str) -> Value {
    json!({
        "id": id, "type": "reasoning",
        "summary": [{"type": "summary_text", "text": summary}],
        "encrypted_content": format!("gAAAA-{id}-ëñç+/==")
    })
}

/// A reasoning item whose summary streams in `pieces`.
fn reasoning(index: u32, id: &str, pieces: &[&str]) -> String {
    let mut s = event(
        json!({"type": "response.output_item.added", "output_index": index,
        "item": {"id": id, "type": "reasoning", "summary": []}}),
    );
    for p in pieces {
        s.push_str(&event(
            json!({"type": "response.reasoning_summary_text.delta",
            "item_id": id, "output_index": index, "summary_index": 0, "delta": p}),
        ));
    }
    s.push_str(&event(
        json!({"type": "response.output_item.done", "output_index": index,
        "item": reasoning_done(id, &pieces.concat())}),
    ));
    s
}

fn call_done(id: &str, call_id: &str, name: &str, args: &str) -> Value {
    json!({"id": id, "type": "function_call", "status": "completed",
        "call_id": call_id, "name": name, "arguments": args})
}

/// A function call whose arguments stream in `pieces`.
fn call(index: u32, id: &str, call_id: &str, name: &str, pieces: &[&str]) -> String {
    let added = json!({"id": id, "type": "function_call", "status": "in_progress",
        "call_id": call_id, "name": name, "arguments": ""});
    let mut s =
        event(json!({"type": "response.output_item.added", "output_index": index, "item": added}));
    for p in pieces {
        s.push_str(&event(
            json!({"type": "response.function_call_arguments.delta",
            "item_id": id, "output_index": index, "delta": p}),
        ));
    }
    let args = pieces.concat();
    s.push_str(&event(
        json!({"type": "response.function_call_arguments.done",
        "item_id": id, "output_index": index, "arguments": args}),
    ));
    s.push_str(&event(
        json!({"type": "response.output_item.done", "output_index": index,
        "item": call_done(id, call_id, name, &args)}),
    ));
    s
}

fn message_done(id: &str, text: &str) -> Value {
    json!({"id": id, "type": "message", "role": "assistant", "status": "completed",
        "content": [{"type": "output_text", "text": text, "annotations": []}]})
}

/// The start of an assistant message and its first `pieces`, without its end.
fn message_start(index: u32, id: &str, pieces: &[&str]) -> String {
    let mut s = event(
        json!({"type": "response.output_item.added", "output_index": index,
        "item": {"id": id, "type": "message", "role": "assistant", "status": "in_progress", "content": []}}),
    );
    s.push_str(&event(
        json!({"type": "response.content_part.added", "item_id": id,
        "output_index": index, "content_index": 0, "part": {"type": "output_text", "text": ""}}),
    ));
    for p in pieces {
        s.push_str(&event(
            json!({"type": "response.output_text.delta", "item_id": id,
            "output_index": index, "content_index": 0, "delta": p}),
        ));
    }
    s
}

/// A whole assistant message streamed in `pieces`.
fn message(index: u32, id: &str, pieces: &[&str]) -> String {
    let text = pieces.concat();
    let mut s = message_start(index, id, pieces);
    s.push_str(&event(
        json!({"type": "response.output_text.done", "item_id": id,
        "output_index": index, "content_index": 0, "text": text}),
    ));
    s.push_str(&event(
        json!({"type": "response.output_item.done", "output_index": index,
        "item": message_done(id, &text)}),
    ));
    s
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A service whose turns call the fake Responses API through the real client.
async fn start_service(store: Option<margin_service::db::Store>) -> Harness {
    let h = start_with(Setup {
        store,
        secrets: MemoryStore::new()
            .with(GITHUB_CLIENT_SECRET, CLIENT_SECRET)
            .with(OPENAI_API_KEY, API_KEY),
        ..Setup::default()
    })
    .await;
    h.generator.agent_over_http.store(true, Ordering::SeqCst);
    *h.state.turns.retry.lock().unwrap() = RetryPolicy::immediate();
    h
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
        ChatItemKind::AssistantText { text } => format!("assistant: {text}"),
        ChatItemKind::Reasoning { summary } => format!("reasoning: {summary}"),
        ChatItemKind::ToolCall { name, label, .. } => format!("call {name}: {label}"),
        ChatItemKind::ToolResult { ok, summary, .. } => {
            format!("result {}: {summary}", if *ok { "ok" } else { "failed" })
        }
        ChatItemKind::Card { .. } => "card".into(),
        ChatItemKind::Action { text } => format!("action: {text}"),
        ChatItemKind::Error { message, .. } => format!("error: {message}"),
    };
    format!("{what}{status}")
}

fn lines(d: &ChatDetail) -> Vec<String> {
    d.items.iter().map(line).collect()
}

/// What an event says about the chat, in one line (other chats' and other kinds of
/// events are `None`).
fn event_line(e: &Event, chat_id: &str) -> Option<String> {
    match e {
        Event::ChatChanged { chat } if chat.id == chat_id => Some(match &chat.state {
            ChatState::Idle => "chat idle".into(),
            ChatState::Running { .. } => "chat running".into(),
            other => format!("chat {other:?}"),
        }),
        Event::ChatItemChanged { item } if item.chat_id == chat_id => {
            Some(format!("item {}", line(item)))
        }
        Event::ChatItemDelta {
            chat_id: c,
            offset,
            text,
            ..
        } if c == chat_id => Some(format!("delta {offset} {text}")),
        _ => None,
    }
}

/// The chat's events in one line each, leaving out a `ChatChanged` that doesn't change
/// the state (every added item bumps `updated_at`; usage changes the chat too) and
/// repeats of an item line.
fn event_lines(events: &[Event], chat_id: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut state: Option<String> = None;
    for line in events.iter().filter_map(|e| event_line(e, chat_id)) {
        if line.starts_with("chat ") {
            if state.as_ref() == Some(&line) {
                continue;
            }
            state = Some(line.clone());
        }
        if out.last() != Some(&line) {
            out.push(line);
        }
    }
    out
}

fn function_call_outputs(input: &[Value]) -> Vec<(String, String)> {
    input
        .iter()
        .filter(|i| i["type"] == "function_call_output")
        .map(|i| {
            (
                i["call_id"].as_str().unwrap().to_string(),
                i["output"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn input_of(body: &Value) -> Vec<Value> {
    body["input"].as_array().unwrap().clone()
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_runs_through_the_real_client_tools_replay_stop_and_restart() {
    let (openai, fake) = start_fake_openai().await;
    let h = start_service(None).await;
    let t = h.sign_in("Mac").await;
    let r = h
        .patch(&t, "/api/settings")
        .json(&json!({"openai_base_url": openai, "openai_model": "test-model"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = h
        .post(&t, "/api/topics")
        .json(&json!({"parent_id": null, "title": "Photosynthesis"}))
        .send()
        .await
        .unwrap();
    let topic: Topic = r.json().await.unwrap();
    let mut sock = h.socket(&t).await;

    // --- Turn 1: reasoning, a library search, then the answer. ---
    fake.push(Reply::Stream(
        [
            created("resp_1"),
            reasoning(0, "rs_1", &["**Plan** ", "Search the library."]),
            call(
                1,
                "fc_1",
                "call_1",
                "search_library",
                &["{\"query\":", "\"photosynthesis\"}"],
            ),
            completed("resp_1"),
        ]
        .concat(),
    ));
    fake.push(Reply::Stream(
        [
            created("resp_2"),
            message(0, "msg_2", &["You have a topic ", "on photosynthesis."]),
            completed("resp_2"),
        ]
        .concat(),
    ));
    let r = h
        .post(&t, "/api/chats")
        .json(&json!({"message": {"text": "What do I have on photosynthesis?"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let created_chat: ChatDetail = r.json().await.unwrap();
    let chat_id = created_chat.chat.id.clone();

    let events = sock
        .until(|e| matches!(e, Event::ChatChanged { chat } if chat.state == ChatState::Idle && chat.usage.model_calls == 2))
        .await;
    let seen = event_lines(&events, &chat_id);
    let label = "Searching the library for “photosynthesis”";
    let d = detail(&h, &t, &chat_id).await;
    let result_line = lines(&d)[3].clone();
    assert!(result_line.starts_with("result ok: "), "{result_line}");
    // Stored before broadcast, in the contract's order: each item appears, streams,
    // finishes; the call's result comes before the call is marked done.
    assert_eq!(
        seen,
        vec![
            "chat idle".to_string(),
            "item user: What do I have on photosynthesis?".into(),
            "chat running".into(),
            "item reasoning: …".into(),
            "delta 0 **Plan** ".into(),
            "delta 9 Search the library.".into(),
            "item reasoning: **Plan** Search the library.".into(),
            "item call search_library: …".into(),
            "delta 0 {\"query\":".into(),
            "delta 9 \"photosynthesis\"}".into(),
            format!("item call search_library: {label}…"),
            format!("item {result_line}"),
            format!("item call search_library: {label}"),
            "item assistant: …".into(),
            "delta 0 You have a topic ".into(),
            "delta 17 on photosynthesis.".into(),
            "item assistant: You have a topic on photosynthesis.".into(),
            "chat idle".into(),
        ],
        "{seen:#?}"
    );
    assert_eq!(
        lines(&d),
        vec![
            "user: What do I have on photosynthesis?".to_string(),
            "reasoning: **Plan** Search the library.".into(),
            format!("call search_library: {label}"),
            result_line.clone(),
            "assistant: You have a topic on photosynthesis.".into(),
        ]
    );
    assert_eq!(d.chat.usage.model_calls, 2);
    assert_eq!(d.chat.usage.input_tokens, 2000);
    assert_eq!(d.chat.usage.cached_input_tokens, 1600);
    assert_eq!(d.chat.title, "What do I have on photosynthesis?");

    // The tool's full output loads on demand and holds the library hit.
    let result_item = &d.items[3];
    let r = h
        .get(
            &t,
            &format!("/api/chats/{chat_id}/items/{}/output", result_item.id),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let output: ToolOutputText = r.json().await.unwrap();
    assert!(output.text.contains(&topic.id), "{}", output.text);

    // What reached the API: the key as a bearer token, the request the contract
    // describes, and the second call replaying the first one's output verbatim.
    {
        let requests = fake.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for (auth, _) in requests.iter() {
            assert_eq!(auth.as_deref(), Some(format!("Bearer {API_KEY}").as_str()));
        }
    }
    let bodies = fake.bodies();
    let first = &bodies[0];
    assert_eq!(first["model"], "test-model");
    assert_eq!(first["stream"], true);
    assert_eq!(first["store"], false);
    assert_eq!(first["prompt_cache_key"], chat_id.as_str());
    assert_eq!(first["parallel_tool_calls"], true);
    assert!(
        first["include"]
            .as_array()
            .unwrap()
            .contains(&json!("reasoning.encrypted_content"))
    );
    let search = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "search_library")
        .expect("search_library is offered");
    assert_eq!(search["strict"], true);
    assert_eq!(first["input"][0]["role"], "developer");
    let first_input = input_of(first);
    let second_input = input_of(&bodies[1]);
    assert_eq!(
        bodies[1]["tools"], first["tools"],
        "the same tools every call"
    );
    assert_eq!(&second_input[..first_input.len()], &first_input[..]);
    let added = &second_input[first_input.len()..];
    assert_eq!(
        added[0],
        reasoning_done("rs_1", "**Plan** Search the library.")
    );
    assert_eq!(
        added[1],
        call_done(
            "fc_1",
            "call_1",
            "search_library",
            "{\"query\":\"photosynthesis\"}"
        )
    );
    assert_eq!(added[2]["type"], "function_call_output");
    assert_eq!(added[2]["call_id"], "call_1");
    assert_eq!(added[2]["output"].as_str().unwrap(), output.text);
    assert_eq!(added.len(), 3);

    // --- Turn 2: the next message replays the whole chat, byte for byte. ---
    fake.push(Reply::Stream(
        [
            created("resp_3"),
            message(0, "msg_3", &["No notes yet."]),
            completed("resp_3"),
        ]
        .concat(),
    ));
    let r = h
        .post(&t, &format!("/api/chats/{chat_id}/messages"))
        .json(&json!({"text": "Any notes?"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let d = wait_chat(&h, &t, &chat_id, |d| {
        d.chat.state == ChatState::Idle && d.items.len() == 7
    })
    .await;
    assert_eq!(lines(&d)[6], "assistant: No notes yet.");
    let bodies = fake.bodies();
    let third_input = input_of(&bodies[2]);
    assert_eq!(&third_input[..second_input.len()], &second_input[..]);
    let added = &third_input[second_input.len()..];
    assert_eq!(
        added[0],
        message_done("msg_2", "You have a topic on photosynthesis.")
    );
    // The new message: its developer context, then the user's words.
    assert_eq!(added[1]["role"], "developer");
    assert_eq!(added.last().unwrap()["role"], "user");
    assert_eq!(added.last().unwrap()["content"][0]["text"], "Any notes?");

    // --- Turn 3: Stop while the answer streams. ---
    fake.push(Reply::Hang(
        [
            created("resp_4"),
            message_start(0, "msg_4", &["Partial ", "answer"]),
        ]
        .concat(),
    ));
    let r = h
        .post(&t, &format!("/api/chats/{chat_id}/messages"))
        .json(&json!({"text": "Explain more"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    // Opening the chat mid-stream shows the text so far.
    wait_chat(&h, &t, &chat_id, |d| {
        d.items
            .last()
            .is_some_and(|i| line(i) == "assistant: Partial answer…")
    })
    .await;
    let r = h
        .post(&t, &format!("/api/chats/{chat_id}/stop"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let d = wait_chat(&h, &t, &chat_id, |d| d.chat.state == ChatState::Idle).await;
    assert_eq!(
        lines(&d)[7..],
        [
            "user: Explain more".to_string(),
            "assistant: Partial answer (cancelled)".into(),
            format!("action: {STOPPED}"),
        ]
    );

    // --- Turn 4: the service stops while a tool call is open. ---
    fake.push(Reply::Hang(
        [
            created("resp_5"),
            call(
                0,
                "fc_5",
                "call_5",
                "search_library",
                &["{\"query\":\"leaves\"}"],
            ),
        ]
        .concat(),
    ));
    let r = h
        .post(&t, &format!("/api/chats/{chat_id}/messages"))
        .json(&json!({"text": "Look for leaves"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    wait_chat(&h, &t, &chat_id, |d| {
        d.items.last().is_some_and(|i| {
            matches!(&i.kind, ChatItemKind::ToolCall { call_id, label, .. } if call_id == "call_5" && !label.is_empty())
        })
    })
    .await;
    // A crash writes nothing: end the turn's task, stop serving, start again on the
    // same store.
    let store = h.state.store.clone();
    h.state.turns.abort(&chat_id);
    let mut h = h;
    let _ = h.stop.take().unwrap().send(());
    sock.close().await;
    let h2 = start_service(Some(store)).await;
    let d = detail(&h2, &t, &chat_id).await;
    assert!(
        matches!(d.chat.state, ChatState::Interrupted { .. }),
        "{:?}",
        d.chat.state
    );
    assert_eq!(
        lines(&d)[10..],
        [
            "user: Look for leaves".to_string(),
            "call search_library: Searching the library for “leaves” (cancelled)".into(),
            "result failed: Interrupted".into(),
            format!("action: {STOPPED_BY_RESTART}"),
        ]
    );

    // --- Continue picks the turn up again through the restarted service. ---
    let mut sock = h2.socket(&t).await;
    fake.push(Reply::Stream(
        [
            created("resp_6"),
            message(0, "msg_6", &["Let me check again."]),
            completed("resp_6"),
        ]
        .concat(),
    ));
    let r = h2
        .post(&t, &format!("/api/chats/{chat_id}/continue"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    sock.until(|e| matches!(e, Event::ChatChanged { chat } if chat.id == chat_id && chat.state == ChatState::Idle))
        .await;
    let d = detail(&h2, &t, &chat_id).await;
    assert_eq!(lines(&d).last().unwrap(), "assistant: Let me check again.");
    assert!(d.items.windows(2).all(|w| w[0].seq < w[1].seq));
    assert_eq!(
        d.chat.usage.model_calls, 4,
        "the stopped and crashed calls never completed"
    );

    let bodies = fake.bodies();
    assert_eq!(bodies.len(), 6);
    let resumed = input_of(&bodies[5]);
    // The replay is valid: the open call is answered right after it, the stop and the
    // restart are told to the model, and cancelled partial text is not replayed.
    let at = resumed
        .iter()
        .position(|i| i["type"] == "function_call" && i["call_id"] == "call_5")
        .expect("the interrupted call is replayed");
    assert_eq!(resumed[at + 1]["type"], "function_call_output");
    assert_eq!(resumed[at + 1]["call_id"], "call_5");
    assert!(
        resumed[at + 1]["output"]
            .as_str()
            .unwrap()
            .contains(CALL_INTERRUPTED)
    );
    let text = serde_json::to_string(&resumed).unwrap();
    assert!(text.contains(STOPPED));
    assert!(text.contains(STOPPED_BY_RESTART));
    assert!(!text.contains("Partial answer"));
    let outputs = function_call_outputs(&resumed);
    assert_eq!(
        outputs
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["call_1", "call_5"]
    );
    // Everything up to the stopped turn is still the same bytes as before.
    assert_eq!(&resumed[..third_input.len()], &third_input[..]);
}

// ---------------------------------------------------------------------------
// Phase 2: a course, a delete and an overwrite through the real client
// ---------------------------------------------------------------------------

async fn library(h: &Harness, token: &str) -> Library {
    let r = h.get(token, "/api/library").send().await.unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

async fn stored_note(h: &Harness, id: &str) -> StoredNote {
    h.state.store.get(db::NOTE, id).await.unwrap().unwrap()
}

/// One model reply that makes the function calls `(call_id, name, arguments)`.
fn calls_reply(id: &str, calls: &[(&str, &str, Value)]) -> Reply {
    let mut sse = created(id);
    for (i, (call_id, name, args)) in calls.iter().enumerate() {
        let args = args.to_string();
        sse.push_str(&call(
            i as u32,
            &format!("fc_{call_id}"),
            call_id,
            name,
            &[&args],
        ));
    }
    sse.push_str(&completed(id));
    Reply::Stream(sse)
}

/// One model reply that answers `text`.
fn text_reply(id: &str, text: &str) -> Reply {
    Reply::Stream(
        [
            created(id),
            message(0, &format!("msg_{id}"), &[text]),
            completed(id),
        ]
        .concat(),
    )
}

async fn confirm(h: &Harness, token: &str, chat_id: &str, item_id: &str, approve: bool) {
    let r = h
        .post(token, &format!("/api/chats/{chat_id}/confirm"))
        .json(&json!({"item_id": item_id, "approve": approve}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
}

/// The confirmation card the chat waits on, once it waits.
async fn wait_for_card(h: &Harness, token: &str, chat_id: &str) -> (ChatDetail, ChatItem) {
    let d = wait_chat(h, token, chat_id, |d| {
        matches!(d.chat.state, ChatState::WaitingForUser { .. })
    })
    .await;
    let ChatState::WaitingForUser { item_id, .. } = &d.chat.state else {
        unreachable!()
    };
    let item = d.items.iter().find(|i| &i.id == item_id).unwrap().clone();
    (d, item)
}

fn confirmation(item: &ChatItem) -> &margin_shared::ConfirmationCard {
    match &item.kind {
        ChatItemKind::Card {
            card: Card::Confirmation(c),
            ..
        } => c,
        other => panic!("not a confirmation card: {other:?}"),
    }
}

/// The lines of the chat's last turn, from its user message on.
fn last_turn(d: &ChatDetail) -> Vec<String> {
    let start = d
        .items
        .iter()
        .rposition(|i| matches!(i.kind, ChatItemKind::UserMessage { .. }))
        .unwrap();
    d.items[start..].iter().map(line).collect()
}

/// The text of the last request's `function_call_output` for `call_id`.
fn output_in(body: &Value, call_id: &str) -> Value {
    let (_, out) = function_call_outputs(&input_of(body))
        .into_iter()
        .find(|(id, _)| id == call_id)
        .unwrap_or_else(|| panic!("no output for {call_id}"));
    serde_json::from_str(&out).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_course_a_delete_and_an_overwrite_run_through_the_real_client() {
    let (openai, fake) = start_fake_openai().await;
    let h = start_service(None).await;
    let t = h.sign_in("Mac").await;
    let phone = h.sign_in("iPhone").await;
    let me: Me = h
        .get(&phone, "/api/me")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let phone_id = me.device.id;
    let r = h
        .patch(&t, "/api/settings")
        .json(&json!({"openai_base_url": openai, "openai_model": "test-model"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = h
        .post(&t, "/api/topics")
        .json(&json!({"parent_id": null, "title": "Old optics"}))
        .send()
        .await
        .unwrap();
    let old: Topic = r.json().await.unwrap();
    let r = h
        .post(&t, "/api/notes")
        .json(
            &json!({"topic_id": old.id, "lesson_id": null, "title": "Mirrors",
            "body_md": "Angles of incidence.", "quote": null}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let mut sock = h.socket(&t).await;

    // --- Turn 1: the model proposes a course; the outline card is stored and sent. ---
    let lessons = json!([
        {"title": "Why waves bend", "summary": "Refraction from wavefronts.", "minutes": 8},
        {"title": "Snell's law", "summary": "Deriving and using it.", "minutes": 10},
        {"title": "Lenses", "summary": "Thin lenses and focal length.", "minutes": 12},
    ]);
    fake.push(calls_reply(
        "resp_1",
        &[(
            "call_propose",
            "propose_course",
            json!({
                "prompt": "Teach me how light refracts",
                "topic_title": "Refraction",
                "subject_title": "Physics",
                "lessons": lessons,
                "level": "intermediate",
                "with_videos": false,
                "with_quiz": true,
                "parent_topic_id": null,
                "source_ids": [],
            }),
        )],
    ));
    fake.push(text_reply(
        "resp_2",
        "Here is an outline; change it on the card.",
    ));
    let r = h
        .post(&t, "/api/chats")
        .json(&json!({"message": {"text": "Teach me how light refracts"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let chat_id = r.json::<ChatDetail>().await.unwrap().chat.id;
    let events = sock
        .until(|e| matches!(e, Event::ChatChanged { chat } if chat.id == chat_id && chat.state == ChatState::Idle && chat.usage.model_calls == 2))
        .await;
    let d = detail(&h, &t, &chat_id).await;
    assert_eq!(
        lines(&d),
        vec![
            "user: Teach me how light refracts".to_string(),
            "call propose_course: Proposing the course “Refraction”".into(),
            "result ok: Proposed “Refraction” · 3 lessons".into(),
            "card".into(),
            "assistant: Here is an outline; change it on the card.".into(),
        ]
    );
    let proposal_id = match &d.items[3].kind {
        ChatItemKind::Card {
            call_id,
            card: Card::Outline(c),
        } => {
            assert_eq!(call_id, "call_propose");
            c.proposal_id.clone()
        }
        other => panic!("expected the outline card, got {other:?}"),
    };
    // Broadcast after it was stored, once.
    let card_events = events
        .iter()
        .filter(|e| matches!(e, Event::ChatItemChanged { item } if item.id == d.items[3].id && item == &d.items[3]))
        .count();
    assert_eq!(card_events, 1);
    let proposal: Proposal = h
        .get(&t, &format!("/api/proposals/{proposal_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(proposal.status, ProposalStatus::Ready);
    assert_eq!(proposal.chat_id.as_deref(), Some(chat_id.as_str()));
    assert_eq!(proposal.request.level, Level::Intermediate);
    assert!(proposal.request.with_quiz && !proposal.request.with_videos);
    let outline = proposal.outline.clone().unwrap();
    assert_eq!(outline.topic_title, "Refraction");
    assert_eq!(outline.lessons.len(), 3);
    // The model was told what it proposed.
    let bodies = fake.bodies();
    assert_eq!(
        output_in(&bodies[1], "call_propose")["proposal_id"],
        proposal_id.as_str()
    );
    let propose = bodies[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "propose_course")
        .expect("propose_course is offered");
    assert_eq!(propose["strict"], true);
    // Nothing exists until the owner accepts.
    assert!(
        library(&h, &t)
            .await
            .topics
            .iter()
            .all(|t| t.title != "Refraction")
    );

    // --- Create lessons on the card, the outline edited and the chips changed. ---
    let mut edited = outline.clone();
    edited.lessons.truncate(2);
    edited.lessons[1].title = "Snell's law in practice".into();
    let r = h
        .post(&t, &format!("/api/proposals/{proposal_id}/accept"))
        .json(&json!({
            "outline": edited,
            "chips": {"level": "advanced", "with_videos": null, "with_quiz": false},
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let topic: Topic = r.json().await.unwrap();
    assert_eq!(topic.title, "Refraction");
    let action = "Created the course “Refraction” with 2 lessons.";
    sock.until(|e| {
        matches!(e, Event::ChatItemChanged { item } if item.chat_id == chat_id
            && matches!(&item.kind, ChatItemKind::Action { text } if text == action))
    })
    .await;
    // The lesson jobs run with the chips' options.
    let written: Vec<Lesson> = eventually(|| async {
        let lessons: Vec<Lesson> = library(&h, &t)
            .await
            .lessons
            .into_iter()
            .filter(|l| l.topic_id == topic.id)
            .collect();
        (lessons.len() == 2 && lessons.iter().all(|l| l.status == LessonStatus::Ready))
            .then_some(lessons)
    })
    .await;
    assert!(written.iter().all(|l| l.level == Level::Advanced));
    let mut titles: Vec<String> = written.iter().map(|l| l.title.clone()).collect();
    titles.sort();
    assert_eq!(titles, ["Snell's law in practice", "Why waves bend"]);
    {
        let requests = h.generator.lesson_requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|c| c.level == Level::Advanced && !c.with_quiz)
        );
        assert!(
            requests
                .iter()
                .all(|c| c.prompt == "Teach me how light refracts")
        );
    }
    assert!(
        library(&h, &t).await.videos.is_empty(),
        "the proposal had no videos"
    );

    // --- Turn 2: the course is in the replay; a delete waits for the owner. ---
    fake.push(calls_reply(
        "resp_3",
        &[("call_delete", "delete_topic", json!({"topic_id": old.id}))],
    ));
    fake.push(text_reply("resp_4", "Deleted the old topic."));
    let r = h
        .post(&t, &format!("/api/chats/{chat_id}/messages"))
        .json(&json!({"text": "Now delete my old optics topic"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let (d, card_item) = wait_for_card(&h, &t, &chat_id).await;
    let card = confirmation(&card_item);
    assert_eq!(card.tool, "delete_topic");
    assert_eq!(card.title, "Delete the topic “Old optics”?");
    assert_eq!(card.detail, "1 note goes with it.");
    assert_eq!(card.approve_label, "Delete");
    assert_eq!(card.decision, Decision::Pending);
    assert_eq!(
        last_turn(&d),
        vec![
            "user: Now delete my old optics topic".to_string(),
            "call delete_topic: Deleting a topic…".into(),
            "card".into(),
        ]
    );
    assert!(
        library(&h, &t).await.topics.iter().any(|t| t.id == old.id),
        "nothing is deleted while it waits"
    );
    let bodies = fake.bodies();
    assert_eq!(bodies.len(), 3);
    // The replay carries the course: after the answer that proposed it, before the
    // new message, told to the model as a developer message.
    let input = input_of(&bodies[2]);
    let at_action = input
        .iter()
        .position(|i| i["role"] == "developer" && i["content"][0]["text"] == action)
        .expect("the course is in the replay");
    let at_answer = input
        .iter()
        .position(|i| i["type"] == "message" && i["role"] == "assistant")
        .unwrap();
    let at_user = input.iter().rposition(|i| i["role"] == "user").unwrap();
    assert!(at_answer < at_action && at_action < at_user);
    // Everything before it replays the earlier turn byte for byte.
    let earlier = input_of(&bodies[1]);
    assert_eq!(&input[..earlier.len()], &earlier[..]);

    // Approved from the phone, over HTTP.
    confirm(&h, &phone, &chat_id, &card_item.id, true).await;
    let d = wait_chat(&h, &t, &chat_id, |d| d.chat.state == ChatState::Idle).await;
    assert_eq!(
        last_turn(&d),
        vec![
            "user: Now delete my old optics topic".to_string(),
            "call delete_topic: Deleting a topic".into(),
            "card".into(),
            "result ok: Deleted “Old optics”".into(),
            "assistant: Deleted the old topic.".into(),
        ]
    );
    let answered = d.items.iter().find(|i| i.id == card_item.id).unwrap();
    assert_eq!(
        confirmation(answered).decision,
        Decision::Approved {
            device_id: phone_id.clone()
        }
    );
    let lib = library(&h, &t).await;
    assert!(lib.topics.iter().all(|t| t.id != old.id));
    assert!(lib.notes.iter().all(|n| n.title != "Mirrors"));
    let bodies = fake.bodies();
    assert_eq!(output_in(&bodies[3], "call_delete")["with_it"]["notes"], 1);

    // --- Turn 3: the agent writes two notes. ---
    fake.push(calls_reply(
        "resp_5",
        &[
            (
                "call_note_a",
                "create_note",
                json!({"topic_id": topic.id, "lesson_id": null, "title": "Indices",
                    "body_md": "Glass is about 1.5."}),
            ),
            (
                "call_note_b",
                "create_note",
                json!({"topic_id": topic.id, "lesson_id": null, "title": "Formulas",
                    "body_md": "n1 sin θ1 = n2 sin θ2"}),
            ),
        ],
    ));
    fake.push(text_reply("resp_6", "Two notes written."));
    let r = h
        .post(&t, &format!("/api/chats/{chat_id}/messages"))
        .json(&json!({"text": "Write me two notes"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    wait_chat(&h, &t, &chat_id, |d| {
        d.chat.state == ChatState::Idle
            && d.items
                .last()
                .is_some_and(|i| line(i) == "assistant: Two notes written.")
    })
    .await;
    let notes: Vec<Note> = library(&h, &t)
        .await
        .notes
        .into_iter()
        .filter(|n| n.topic_id == topic.id)
        .collect();
    let note_id = |title: &str| notes.iter().find(|n| n.title == title).unwrap().id.clone();
    let (agents, edited) = (note_id("Indices"), note_id("Formulas"));
    assert_eq!(stored_note(&h, &agents).await.author, Author::Agent);
    // The owner edits the second one's text: it is their writing now.
    let r = h
        .patch(&t, &format!("/api/notes/{edited}"))
        .json(&json!({"body_md": "n₁ sin θ₁ = n₂ sin θ₂, my way."}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(stored_note(&h, &edited).await.author, Author::Owner);

    // --- Turn 4: replacing the agent's note just happens; the owner's asks. ---
    fake.push(calls_reply(
        "resp_7",
        &[
            (
                "call_replace_a",
                "replace_note_body",
                json!({"note_id": agents, "body_md": "Glass: 1.5; water: 1.33."}),
            ),
            (
                "call_replace_b",
                "replace_note_body",
                json!({"note_id": edited, "body_md": "Snell's law."}),
            ),
        ],
    ));
    fake.push(text_reply("resp_8", "Updated the first; left yours."));
    let r = h
        .post(&t, &format!("/api/chats/{chat_id}/messages"))
        .json(&json!({"text": "Tidy both notes"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let (d, card_item) = wait_for_card(&h, &t, &chat_id).await;
    let card = confirmation(&card_item);
    assert_eq!(card.tool, "replace_note_body");
    assert_eq!(card.title, "Replace the text of your note “Formulas”?");
    assert_eq!(card.approve_label, "Replace");
    // Only the owner's note asked; the agent's was already replaced.
    assert_eq!(
        last_turn(&d)
            .iter()
            .filter(|l| l.as_str() == "card")
            .count(),
        1
    );
    assert!(last_turn(&d).contains(&"result ok: Rewrote “Indices”".to_string()));
    assert_eq!(
        stored_note(&h, &agents).await.note.body_md,
        "Glass: 1.5; water: 1.33."
    );
    // Denied over HTTP: the owner's text stays and the model is told.
    confirm(&h, &t, &chat_id, &card_item.id, false).await;
    let d = wait_chat(&h, &t, &chat_id, |d| d.chat.state == ChatState::Idle).await;
    assert_eq!(
        last_turn(&d).last().unwrap(),
        "assistant: Updated the first; left yours."
    );
    assert!(last_turn(&d).contains(&"result failed: Declined".to_string()));
    let row = stored_note(&h, &edited).await;
    assert_eq!(row.note.body_md, "n₁ sin θ₁ = n₂ sin θ₂, my way.");
    assert_eq!(row.author, Author::Owner);
    let bodies = fake.bodies();
    assert_eq!(bodies.len(), 8);
    assert_eq!(
        output_in(&bodies[7], "call_replace_b")["error"],
        "The owner declined: Replace the text of your note “Formulas”?"
    );
    assert!(d.items.windows(2).all(|w| w[0].seq < w[1].seq));
}
