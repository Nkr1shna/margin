//! The whole agent path in one test: a chat message over HTTP → the service's turn
//! runner → the agent loop → the real `ResponsesClient` against a fake Responses API that
//! streams real SSE → a read-only library tool → the final answer, with the stored items,
//! the socket's events, replay on the next turn, Stop, and Continue after a simulated
//! restart of the service on the same store.

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
use margin_service::secrets::{GITHUB_CLIENT_SECRET, MemoryStore, OPENAI_API_KEY};
use margin_service::turns::{CALL_INTERRUPTED, STOPPED, STOPPED_BY_RESTART};
use margin_shared::{
    ChatDetail, ChatItem, ChatItemKind, ChatState, Event, ItemStatus, ToolOutputText, Topic,
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
