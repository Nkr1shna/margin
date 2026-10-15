//! Chats with the agent end to end: the chat routes, turns run by the real loop against
//! a scripted model (`FakeGenerator.agent`), the pending message, stop, continue,
//! confirmations from another device, recovery after a restart, the read tools against
//! the in-memory library, and device presence and tool requests over the events socket.

mod common;

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use common::*;
use futures_util::SinkExt;
use margin_agent::scripted::{function_call_item, text_reply, tool_calls_reply};
use margin_agent::{
    DeviceBridge, DeviceCall, DeviceError, ItemRecord, NoDevices, RetryPolicy, Scripted,
    ToolContext, ToolOutput, TurnSink,
};
use margin_ai::AiError;
use margin_service::db::{self, Store};
use margin_service::secrets::{GITHUB_CLIENT_SECRET, MemoryStore, OPENAI_API_KEY};
use margin_service::state::{SETTINGS_ID, SettingsRow, StoredLesson};
use margin_service::turns::{
    CALL_FAILED, CALL_INTERRUPTED, CONFIRMATION_EXPIRED, RunningTurn, STOPPED, STOPPED_BY_RESTART,
    ServiceSink, TITLE_MAX_OUTPUT_TOKENS, WRITE_UNCONFIRMED,
};
use margin_service::{agent_tools, chats, trace};
use margin_shared::context::{TraceKind, TraceRollup, TraceRow};
use margin_shared::pipeline::{PipelineConfig, Route};
use margin_shared::routes;
use margin_shared::{
    ApiError, Card, Chat, ChatDetail, ChatItem, ChatItemKind, ChatState, Citation,
    ConfirmationCard, Decision, Device, DeviceFrame, DeviceRef, DeviceToolCall, DeviceToolOutput,
    DeviceToolResult, DirectedFrame, Event, ItemStatus, Lesson, LessonStatus, Level, Me,
    MessageSent, Note, PendingMessage, Provider, ReasoningEffort, ResultCard, ResultTarget,
    Settings, Source, SourceKind, SourceStatus, TitleSource, ToolOutputText, Topic, Usage, Waiting,
    Workspace, error_code,
};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn signed_in() -> (Harness, String) {
    let h = start().await;
    *h.state.turns.retry.lock().unwrap() = RetryPolicy::immediate();
    let t = h.sign_in("Mac").await;
    h.configure_ai(&t).await;
    (h, t)
}

async fn api_error(r: reqwest::Response) -> (u16, ApiError) {
    let status = r.status().as_u16();
    (status, r.json().await.unwrap())
}

async fn me(h: &Harness, token: &str) -> Me {
    h.get(token, "/api/me")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn create_chat(h: &Harness, token: &str, body: Value) -> ChatDetail {
    let r = h
        .post(token, "/api/chats")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

/// A new chat whose first message is `text`.
async fn ask(h: &Harness, token: &str, text: &str) -> ChatDetail {
    create_chat(h, token, json!({"message": {"text": text}})).await
}

async fn send(h: &Harness, token: &str, chat_id: &str, text: &str) -> reqwest::Response {
    h.post(token, &format!("/api/chats/{chat_id}/messages"))
        .json(&json!({"text": text}))
        .send()
        .await
        .unwrap()
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

/// Waits until the chat satisfies `done`.
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

fn idle(d: &ChatDetail) -> bool {
    d.chat.state == ChatState::Idle
}

fn running(d: &ChatDetail) -> bool {
    matches!(d.chat.state, ChatState::Running { .. })
}

/// One line per item, for asserting a transcript.
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
        ChatItemKind::ToolCall { name, .. } => format!("call {name}"),
        ChatItemKind::ToolResult { ok, summary, .. } => {
            format!("result {}: {summary}", if *ok { "ok" } else { "failed" })
        }
        ChatItemKind::Card { card, .. } => match card {
            Card::Outline(_) => "card outline".into(),
            Card::Result(_) => "card result".into(),
            Card::Passages(p) => format!("card passages: {}", p.passages.len()),
            Card::Confirmation(c) => format!("card confirm: {:?}", c.decision),
            Card::WebResults(w) => format!("card web results: {}", w.query),
        },
        ChatItemKind::Action { text } => format!("action: {text}"),
        ChatItemKind::Error {
            message, retryable, ..
        } => {
            format!(
                "error{}: {message}",
                if *retryable { " (retry)" } else { "" }
            )
        }
    };
    format!("{what}{status}")
}

fn lines(d: &ChatDetail) -> Vec<String> {
    d.items.iter().map(line).collect()
}

/// Each `ChatChanged` of a chat changes something besides `updated_at`.
fn assert_chat_changes_are_real(events: &[Event]) {
    let mut last: std::collections::HashMap<&str, Chat> = Default::default();
    for e in events {
        let Event::ChatChanged { chat } = e else {
            continue;
        };
        if let Some(before) = last.get(chat.id.as_str()) {
            let mut before = before.clone();
            before.updated_at = chat.updated_at;
            assert_ne!(&before, chat, "a ChatChanged that changed nothing");
        }
        last.insert(chat.id.as_str(), chat.clone());
    }
}

/// The text of every `function_call_output` in a request's input, by call id.
fn call_outputs(input: &[Value]) -> Vec<(String, String)> {
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

/// Whether the input has a user message with this text.
fn has_user_text(input: &[Value], text: &str) -> bool {
    input.iter().any(|i| {
        i["role"] == "user"
            && i["content"]
                .as_array()
                .is_some_and(|c| c.iter().any(|p| p["text"] == text))
    })
}

/// Whether the input has a developer message with this text.
fn has_developer_text(input: &[Value], text: &str) -> bool {
    input.iter().any(|i| {
        i["role"] == "developer"
            && i["content"]
                .as_array()
                .is_some_and(|c| c.iter().any(|p| p["text"] == text))
    })
}

/// A reply that starts an answer with `partial` and never finishes.
fn hanging_text(partial: &str) -> Scripted {
    let mut events = text_reply(&[partial, "never sent"]);
    events.truncate(2);
    Scripted::Hang(events)
}

/// A reply that calls a tool and then never finishes.
fn hanging_call(call_id: &str, name: &str, args: &str) -> Scripted {
    let mut events = tool_calls_reply(&[(call_id, name, args)]);
    events.pop(); // no `Completed`
    Scripted::Hang(events)
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

async fn upload_ready(
    h: &Harness,
    token: &str,
    name: &str,
    topic: Option<&str>,
    text: &str,
) -> Source {
    let r = h.upload(token, name, topic, text.to_string()).await;
    assert_eq!(r.status(), 200);
    let source: Source = r.json().await.unwrap();
    eventually(|| async {
        let s: Option<Source> = h.state.store.get(db::SOURCE, &source.id).await.unwrap();
        s.filter(|s| matches!(s.status, SourceStatus::Ready { .. }))
    })
    .await
}

// ---------------------------------------------------------------------------
// Turns
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_message_starts_a_turn_that_streams_and_finishes() {
    let (h, t) = signed_in().await;
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Fourier ", "series are…"])));
    let mut sock = h.socket(&t).await;

    let created = ask(&h, &t, "  Teach me Fourier series\nwith examples  ").await;
    let chat_id = created.chat.id.clone();
    assert_eq!(created.chat.title, "Teach me Fourier series");
    assert!(matches!(created.chat.state, ChatState::Running { .. }));
    assert_eq!(created.items.len(), 1);
    match &created.items[0].kind {
        ChatItemKind::UserMessage {
            text, device_name, ..
        } => {
            assert_eq!(text, "Teach me Fourier series\nwith examples");
            assert_eq!(device_name, "Mac");
        }
        other => panic!("{other:?}"),
    }

    // Stored before broadcast, in the contract's order.
    let events = sock
        .until(|e| matches!(e, Event::ChatChanged { chat } if chat.state == ChatState::Idle && chat.usage.model_calls == 1))
        .await;
    let chat_events: Vec<&Event> = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                Event::ChatChanged { .. }
                    | Event::ChatItemChanged { .. }
                    | Event::ChatItemDelta { .. }
            )
        })
        .collect();
    assert!(
        matches!(chat_events[0], Event::ChatChanged { chat } if chat.state == ChatState::Idle && chat.title.is_empty())
    );
    assert!(
        matches!(chat_events[1], Event::ChatItemChanged { item } if matches!(item.kind, ChatItemKind::UserMessage { .. }))
    );
    assert!(
        matches!(chat_events[2], Event::ChatChanged { chat } if matches!(chat.state, ChatState::Running { .. }))
    );
    // New items only bump `updated_at`: the chat goes Running → usage → Idle.
    assert_chat_changes_are_real(&events);
    let changes = events
        .iter()
        .filter(|e| matches!(e, Event::ChatChanged { chat } if chat.id == chat_id))
        .count();
    assert_eq!(changes, 4, "created, running, usage, idle: {events:?}");
    let deltas: Vec<(u64, &str)> = events
        .iter()
        .filter_map(|e| match e {
            Event::ChatItemDelta {
                offset,
                text,
                chat_id: c,
                ..
            } if *c == chat_id => Some((*offset, text.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec![(0, "Fourier "), (8, "series are…")]);

    let d = detail(&h, &t, &chat_id).await;
    assert_eq!(
        lines(&d),
        vec![
            "user: Teach me Fourier series\nwith examples",
            "assistant: Fourier series are…",
        ]
    );
    assert_eq!(d.chat.usage.model_calls, 1);
    assert_eq!(d.chat.usage.input_tokens, 100);
    assert!(d.items.windows(2).all(|w| w[0].seq < w[1].seq));

    let reqs = h.generator.agent.requests();
    assert_eq!(reqs.len(), 1);
    let req = &reqs[0];
    assert_eq!(req.prompt_cache_key.as_deref(), Some(chat_id.as_str()));
    assert_eq!(
        req.model.as_deref(),
        Some("test-model"),
        "no agent model: the main model's slot"
    );
    assert_eq!(
        req.input[0]["role"], "developer",
        "the system prompt comes first"
    );
    assert!(has_user_text(
        &req.input,
        "Teach me Fourier series\nwith examples"
    ));
    // Function tools by name; the hosted web search has only its type.
    let names: Vec<&str> = req
        .tools
        .iter()
        .map(|t| t["name"].as_str().or(t["type"].as_str()).unwrap())
        .collect();
    for name in [
        "search_library",
        "list_topics",
        "get_topic",
        "read_lesson",
        "read_note",
        "list_notes",
        "search",
        "read_passage",
        "document_outline",
        "view_picture",
        "list_sources",
    ] {
        assert!(names.contains(&name), "{name} missing from {names:?}");
    }
    assert!(
        names.contains(&"web_search"),
        "the hosted web search is offered"
    );
    assert!(
        req.tools
            .iter()
            .filter(|t| t["type"] == "function")
            .all(|t| t["strict"] == true)
    );

    let chats: Vec<Chat> = h
        .get(&t, "/api/chats")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(chats[0].id, chat_id);

    // The next message continues the same chat with the whole history.
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Sure."])));
    let r = send(&h, &t, &chat_id, "Shorter please").await;
    assert_eq!(r.status(), 200);
    let sent: MessageSent = r.json().await.unwrap();
    assert!(sent.item.is_some());
    assert_eq!(sent.chat.title, "Teach me Fourier series", "kept");
    let d = wait_chat(&h, &t, &chat_id, |d| idle(d) && d.items.len() == 4).await;
    assert_eq!(lines(&d)[3], "assistant: Sure.");
    let second = &h.generator.agent.requests()[1];
    assert!(has_user_text(
        &second.input,
        "Teach me Fourier series\nwith examples"
    ));
    assert!(has_user_text(&second.input, "Shorter please"));
    assert!(
        second
            .input
            .iter()
            .any(|i| i["type"] == "message" && i["role"] == "assistant"),
        "the first answer is replayed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_settings_pick_the_model_and_reasoning_effort() {
    let (h, t) = signed_in().await;
    let r = h
        .patch(&t, "/api/settings")
        .json(&json!({"agent_model": "  gpt-6-luna ", "reasoning_effort": "high"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let s: Settings = r.json().await.unwrap();
    assert_eq!(s.agent_model.as_deref(), Some("gpt-6-luna"));
    assert_eq!(s.reasoning_effort, ReasoningEffort::High);
    assert_eq!(s.openai_model, "test-model", "lessons keep their model");

    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["ok"])));
    let chat = ask(&h, &t, "hi").await.chat;
    wait_chat(&h, &t, &chat.id, idle).await;
    let req = &h.generator.agent.requests()[0];
    assert_eq!(req.model.as_deref(), Some("gpt-6-luna"));
    assert_eq!(req.reasoning.map(|r| r.effort), Some(ReasoningEffort::High));

    // Null and blank both go back to the main model.
    for clear in [json!(null), json!("  ")] {
        let s: Settings = h
            .patch(&t, "/api/settings")
            .json(&json!({"agent_model": clear}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(s.agent_model, None);
        assert_eq!(s.reasoning_effort, ReasoningEffort::High, "unchanged");
    }
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["ok"])));
    assert_eq!(send(&h, &t, &chat.id, "again").await.status(), 200);
    wait_chat(&h, &t, &chat.id, |d| idle(d) && d.items.len() == 4).await;
    assert_eq!(
        h.generator.agent.requests()[1].model.as_deref(),
        Some("test-model")
    );

    // An agent model alone is enough for chats.
    let r = h
        .patch(&t, "/api/settings")
        .json(&json!({"openai_model": "", "agent_model": "gpt-6-luna"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["still here"])));
    assert_eq!(send(&h, &t, &chat.id, "third").await.status(), 200);
    // Six items appear while the answer still streams; wait for the turn to end.
    let d = wait_chat(&h, &t, &chat.id, |d| d.items.len() == 6 && idle(d)).await;
    assert_eq!(lines(&d)[5], "assistant: still here");
    assert_eq!(
        h.generator.agent.requests()[2].model.as_deref(),
        Some("gpt-6-luna")
    );

    // Neither: the turn fails with `no_model`'s message.
    h.patch(&t, "/api/settings")
        .json(&json!({"agent_model": null}))
        .send()
        .await
        .unwrap();
    assert_eq!(send(&h, &t, &chat.id, "fourth").await.status(), 200);
    let d = wait_chat(&h, &t, &chat.id, |d| {
        matches!(d.chat.state, ChatState::Failed { .. })
    })
    .await;
    assert!(
        lines(&d)
            .last()
            .unwrap()
            .starts_with("error (retry): Choose a model"),
        "{:?}",
        lines(&d)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_settings_are_stored() {
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
    assert_eq!(s.agent_model, None);
    assert_eq!(s.reasoning_effort, ReasoningEffort::Medium);
    let r = h
        .patch(&t, "/api/settings")
        .json(&json!({"agent_model": "gpt-6-astra", "reasoning_effort": "minimal"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let row: SettingsRow = h
        .state
        .store
        .get(db::SETTINGS, SETTINGS_ID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.agent_model.as_deref(), Some("gpt-6-astra"));
    assert_eq!(row.reasoning_effort, ReasoningEffort::Minimal);
    let r = h
        .patch(&t, "/api/settings")
        .json(&json!({"reasoning_effort": "extreme"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422);
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_calls_run_and_their_outputs_load_on_demand() {
    let (h, t) = signed_in().await;
    let topic = new_topic(&h, &t, None, "Fourier series").await;
    h.generator.agent.push(Scripted::Events(tool_calls_reply(&[
        ("call_1", "list_topics", "{}"),
        ("call_2", "search_library", r#"{"query":"fourier"}"#),
        ("call_3", "no_such_tool", "{}"),
    ])));
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["You have one topic."])));
    let chat = ask(&h, &t, "What do I have?").await.chat;
    let d = wait_chat(&h, &t, &chat.id, idle).await;
    let l = lines(&d);
    assert_eq!(l[0], "user: What do I have?");
    assert_eq!(l.last().unwrap(), "assistant: You have one topic.");
    assert!(l.contains(&"call list_topics".to_string()), "{l:?}");
    assert!(l.contains(&"result ok: 1 topic".to_string()), "{l:?}");
    assert!(l.contains(&"result ok: 1 match".to_string()), "{l:?}");
    assert!(
        l.iter().any(|x| x.starts_with("result failed")),
        "the unknown tool fails: {l:?}"
    );
    let call = d
        .items
        .iter()
        .find(
            |i| matches!(&i.kind, ChatItemKind::ToolCall { name, .. } if name == "search_library"),
        )
        .unwrap();
    match &call.kind {
        ChatItemKind::ToolCall {
            label, arguments, ..
        } => {
            assert_eq!(label, "Searching the library for “fourier”");
            assert_eq!(arguments, r#"{"query":"fourier"}"#);
        }
        _ => unreachable!(),
    }
    assert_eq!(call.status, ItemStatus::Done);

    // Outputs aren't pushed; a device fetches one when its row is expanded.
    let result = d
        .items
        .iter()
        .find(
            |i| matches!(&i.kind, ChatItemKind::ToolResult { call_id, .. } if call_id == "call_1"),
        )
        .unwrap();
    let out: ToolOutputText = h
        .get(
            &t,
            &format!("/api/chats/{}/items/{}/output", chat.id, result.id),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let topics: Value = serde_json::from_str(&out.text).unwrap();
    assert_eq!(topics["topics"][0]["id"], topic.id.as_str());
    match &result.kind {
        ChatItemKind::ToolResult { output_bytes, .. } => {
            assert_eq!(*output_bytes, out.text.len() as u64)
        }
        _ => unreachable!(),
    }
    // Only tool results of this chat.
    let user = &d.items[0];
    let r = h
        .get(
            &t,
            &format!("/api/chats/{}/items/{}/output", chat.id, user.id),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let r = h
        .get(&t, &format!("/api/chats/other/items/{}/output", result.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // The second model call has every output, in the model's call order.
    let reqs = h.generator.agent.requests();
    assert_eq!(reqs.len(), 2);
    let outputs = call_outputs(&reqs[1].input);
    let ids: Vec<&str> = outputs.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, vec!["call_1", "call_2", "call_3"]);
    assert_eq!(outputs[0].1, out.text);
}

#[tokio::test(flavor = "multi_thread")]
async fn read_tools_answer_from_the_library() {
    let (h, t) = signed_in().await;
    let subject = new_topic(&h, &t, None, "Signals").await;
    let topic = new_topic(&h, &t, Some(&subject.id), "Fourier series").await;
    let now = Utc::now();
    let lesson = StoredLesson {
        lesson: Lesson {
            id: "l1".into(),
            topic_id: topic.id.clone(),
            title: "Coefficients".into(),
            summary: "Projecting onto cosines".into(),
            position: 0,
            level: Level::Intermediate,
            minutes: 10,
            body_md: "The coefficient a_n is a projection.[^1]".into(),
            status: LessonStatus::Ready,
            created_at: now,
            updated_at: now,
            citations: vec![Citation {
                n: 1,
                source_id: "s1".into(),
                source_name: "notes.pdf".into(),
                kind: SourceKind::Pdf,
                page: Some(3),
                start_ms: None,
                end_ms: None,
                excerpt: "a_n".into(),
                image_url: None,
                document_id: None,
                start_line: None,
                end_line: None,
                file: None,
                section_path: None,
            }],
        },
        proposal_id: None,
        previous_body: None,
        previous_citations: None,
        author: Default::default(),
        prompt: None,
        with_quiz: false,
        workspace_ids: Vec::new(),
    };
    h.state.store.put(db::LESSON, "l1", &lesson).await.unwrap();
    let r = h
        .post(&t, "/api/notes")
        .json(&json!({"topic_id": topic.id, "lesson_id": "l1", "title": "Gibbs", "body_md": "Overshoot of 9%", "quote": "a projection"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let note: Note = r.json().await.unwrap();
    let source = upload_ready(
        &h,
        &t,
        "fourier.md",
        Some(&topic.id),
        "Gibbs ringing appears near jumps.\n\nKnead the dough for ten minutes.",
    )
    .await;

    let chat = create_chat(&h, &t, json!({})).await.chat;
    let registry = agent_tools::registry(&h.state);
    let ctx = ToolContext {
        chat_id: chat.id.clone(),
        turn_id: "t".into(),
        call_id: "call".into(),
        origin_device: DeviceRef {
            device_id: "d".into(),
            name: "Mac".into(),
        },
        workspaces: Vec::new(),
        devices: Arc::new(NoDevices),
    };
    let run = |name: &'static str, args: Value| {
        let registry = registry.clone();
        let ctx = ctx.clone();
        async move {
            let tool = registry.get(name).unwrap().clone();
            assert_eq!(tool.kind(), margin_agent::ToolKind::Read, "{name}");
            let out = tool.run(&ctx, args).await;
            let json: Value = if out.ok {
                serde_json::from_str(&out.output).unwrap()
            } else {
                json!({"error": out.summary})
            };
            (out, json)
        }
    };

    let (out, v) = run("list_topics", json!({})).await;
    assert_eq!(out.summary, "2 topics");
    assert_eq!(v["topics"][0]["title"], "Signals");
    assert_eq!(v["topics"][1]["title"], "Fourier series");
    assert_eq!(v["topics"][1]["depth"], 1);
    assert_eq!(v["topics"][1]["lessons"], 1);
    assert_eq!(v["topics"][1]["notes"], 1);

    let (out, v) = run("get_topic", json!({"topic_id": topic.id})).await;
    assert_eq!(out.summary, "Fourier series: 1 lesson, 1 note");
    assert_eq!(v["path"], "Signals / Fourier series");
    assert_eq!(v["lessons"][0]["status"], "ready");
    assert_eq!(v["lessons"][0]["video"], "none");
    assert_eq!(v["notes"][0]["id"], note.id.as_str());
    assert_eq!(v["sources"][0]["id"], source.id.as_str());
    let (out, _) = run("get_topic", json!({"topic_id": "nope"})).await;
    assert!(!out.ok);
    assert_eq!(out.summary, "That topic doesn't exist.");

    let (out, v) = run("read_lesson", json!({"lesson_id": "l1"})).await;
    assert_eq!(out.summary, "Read “Coefficients”");
    assert_eq!(v["body_md"], "The coefficient a_n is a projection.[^1]");
    assert_eq!(v["topic"], "Signals / Fourier series");
    assert_eq!(v["citations"][0]["page"], 3);

    let (out, v) = run("read_note", json!({"note_id": note.id})).await;
    assert_eq!(out.summary, "Read “Gibbs”");
    assert_eq!(v["lesson"], "Coefficients");
    assert_eq!(v["quote"], "a projection");
    assert_eq!(v["body_md"], "Overshoot of 9%");

    let (out, v) = run("list_notes", json!({"topic_id": subject.id})).await;
    assert_eq!(
        out.summary, "1 note",
        "a subject includes its topics' notes"
    );
    assert_eq!(v["notes"][0]["lesson"], "Coefficients");
    let (out, _) = run("list_notes", json!({"topic_id": null})).await;
    assert_eq!(out.summary, "1 note");

    let (out, v) = run("search_library", json!({"query": "projection"})).await;
    assert_eq!(out.summary, "2 matches");
    assert_eq!(v["hits"][0]["kind"], "lesson");

    let (out, v) = run("list_sources", json!({"topic_id": null})).await;
    assert_eq!(out.summary, "1 source");
    assert_eq!(v["sources"][0]["status"], "ready");
    assert_eq!(v["sources"][0]["topic"], "Signals / Fourier series");
    assert_eq!(v["sources"][0]["attached_to_this_chat"], false);

    let (out, v) = run(
        "search",
        json!({"query": "Gibbs ringing", "scope": {"topic_id": subject.id, "source_ids": null, "workspace_ids": null, "chat_files": null, "library": null}, "k": null, "modalities": null}),
    )
    .await;
    assert!(out.ok, "{}", out.output);
    assert_eq!(v["passages"][0]["source"], "fourier.md");
    assert!(
        v["passages"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Gibbs ringing")
    );
    match out.cards.as_slice() {
        [Card::Passages(card)] => {
            assert_eq!(card.query, "Gibbs ringing");
            assert_eq!(card.passages[0].n, 1);
            assert_eq!(card.passages[0].source_id, source.id);
        }
        other => panic!("{other:?}"),
    }
    let (out, _) = run(
        "search",
        json!({"query": "x", "scope": {"topic_id": null, "source_ids": ["gone"], "workspace_ids": null, "chat_files": null, "library": null}, "k": null, "modalities": null}),
    )
    .await;
    assert!(!out.ok);
    // A topic without files is searched all the same (its lessons and notes are
    // documents too); it just finds nothing.
    let empty = new_topic(&h, &t, None, "Empty").await;
    let (out, v) = run(
        "search",
        json!({"query": "x", "scope": {"topic_id": empty.id, "source_ids": null, "workspace_ids": null, "chat_files": null, "library": null}, "k": null, "modalities": null}),
    )
    .await;
    assert!(out.ok, "{}", out.output);
    assert_eq!(v["passages"].as_array().map(Vec::len), Some(0), "{v}");
}

// ---------------------------------------------------------------------------
// Pending message, stop, continue
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_message_sent_while_busy_waits_and_goes_next() {
    let (h, t) = signed_in().await;
    let phone = h.sign_in("iPhone").await;
    h.generator.agent.push(hanging_text("Thinking"));
    let chat = ask(&h, &t, "first").await.chat;
    wait_chat(&h, &t, &chat.id, |d| d.items.len() == 2).await;

    let r = send(&h, &phone, &chat.id, "second").await;
    assert_eq!(r.status(), 200);
    let sent: MessageSent = r.json().await.unwrap();
    assert_eq!(sent.item, None);
    let pending = sent.chat.pending.clone().unwrap();
    assert_eq!(pending.text, "second");
    assert_eq!(pending.device_id, me(&h, &phone).await.device.id);

    let (status, err) = api_error(send(&h, &t, &chat.id, "third").await).await;
    assert_eq!((status, err.code.as_str()), (409, "already_pending"));

    // Dropped, then queued again.
    let r = h
        .delete(&t, &format!("/api/chats/{}/pending", chat.id))
        .send()
        .await
        .unwrap();
    let c: Chat = r.json().await.unwrap();
    assert_eq!(c.pending, None);
    assert_eq!(
        send(&h, &phone, &chat.id, "second, again").await.status(),
        200
    );

    // Stopping the first answer sends the pending one.
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Done."])));
    let r = h
        .post(&t, &format!("/api/chats/{}/stop", chat.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let after_stop: Chat = r.json().await.unwrap();
    assert!(
        matches!(after_stop.state, ChatState::Running { ref device_id, .. } if *device_id == pending.device_id)
    );
    assert_eq!(after_stop.pending, None);
    let d = wait_chat(&h, &t, &chat.id, idle).await;
    assert_eq!(
        lines(&d),
        vec![
            "user: first",
            "assistant: Thinking (cancelled)",
            &format!("action: {STOPPED}"),
            "user: second, again",
            "assistant: Done.",
        ]
    );
    match &d.items[3].kind {
        ChatItemKind::UserMessage { device_name, .. } => assert_eq!(device_name, "iPhone"),
        other => panic!("{other:?}"),
    }
    // The stop is in the next turn's history.
    let reqs = h.generator.agent.requests();
    assert!(has_developer_text(&reqs[1].input, STOPPED));
    assert!(has_user_text(&reqs[1].input, "second, again"));

    // A finished turn sends its pending message too.
    h.generator.agent.push(Scripted::Delayed(
        Duration::from_millis(300),
        Box::new(Scripted::Events(text_reply(&["One."]))),
    ));
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Two."])));
    assert_eq!(send(&h, &t, &chat.id, "one").await.status(), 200);
    let r = send(&h, &t, &chat.id, "two").await;
    let sent: MessageSent = r.json().await.unwrap();
    assert!(sent.chat.pending.is_some());
    let d = wait_chat(&h, &t, &chat.id, |d| idle(d) && d.items.len() == 9).await;
    assert_eq!(
        lines(&d)[5..],
        [
            "user: one",
            "assistant: One.",
            "user: two",
            "assistant: Two."
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_keeps_streamed_text_and_answers_open_calls() {
    let (h, t) = signed_in().await;
    let phone = h.sign_in("iPhone").await;
    let (status, err) = api_error(h.post(&t, "/api/chats/nope/stop").send().await.unwrap()).await;
    assert_eq!((status, err.code.as_str()), (404, "not_found"));

    h.generator.agent.push(hanging_text("Half an ans"));
    let chat = ask(&h, &t, "explain").await.chat;
    // A device that opens the chat mid-stream sees the text so far.
    let d = wait_chat(&h, &phone, &chat.id, |d| {
        d.items.len() == 2 && lines(d)[1] == "assistant: Half an ans…"
    })
    .await;
    assert!(running(&d));

    let mut sock = h.socket(&t).await;
    let r = h
        .post(&phone, &format!("/api/chats/{}/stop", chat.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let stopped: Chat = r.json().await.unwrap();
    assert_eq!(stopped.state, ChatState::Idle);
    let events = sock
        .until(|e| matches!(e, Event::ChatChanged { chat } if chat.state == ChatState::Idle))
        .await;
    assert!(events.iter().any(
        |e| matches!(e, Event::ChatItemChanged { item } if item.status == ItemStatus::Cancelled)
    ));
    let d = detail(&h, &t, &chat.id).await;
    assert_eq!(
        lines(&d),
        vec![
            "user: explain",
            "assistant: Half an ans (cancelled)",
            &format!("action: {STOPPED}"),
        ]
    );
    let (status, err) = api_error(
        h.post(&t, &format!("/api/chats/{}/stop", chat.id))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!((status, err.code.as_str()), (409, "not_running"));

    // Stopped while a tool call waited: the call gets a "Cancelled" result, so the next
    // model call's history is valid.
    h.generator
        .agent
        .push(hanging_call("call_9", "list_topics", "{}"));
    assert_eq!(send(&h, &t, &chat.id, "list them").await.status(), 200);
    // Once its arguments are complete (the call is stored with its model item).
    wait_chat(&h, &t, &chat.id, |d| {
        d.items
            .iter()
            .any(|i| matches!(&i.kind, ChatItemKind::ToolCall { label, .. } if !label.is_empty()))
    })
    .await;
    let r = h
        .post(&t, &format!("/api/chats/{}/stop", chat.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let d = detail(&h, &t, &chat.id).await;
    assert_eq!(
        lines(&d)[3..],
        [
            "user: list them".to_string(),
            "call list_topics (cancelled)".to_string(),
            "result failed: Cancelled".to_string(),
            format!("action: {STOPPED}"),
        ]
    );
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["ok"])));
    assert_eq!(send(&h, &t, &chat.id, "go on").await.status(), 200);
    wait_chat(&h, &t, &chat.id, |d| idle(d) && d.items.len() == 9).await;
    let reqs = h.generator.agent.requests();
    let outputs = call_outputs(&reqs.last().unwrap().input);
    assert_eq!(
        outputs,
        vec![(
            "call_9".to_string(),
            r#"{"error":"cancelled by the user"}"#.to_string()
        )]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_turn_can_be_continued() {
    let h = start().await;
    *h.state.turns.retry.lock().unwrap() = RetryPolicy::immediate();
    let t = h.sign_in("Mac").await;

    // No key yet: the turn fails like a failed model call, with Retry.
    let chat = ask(&h, &t, "hello").await.chat;
    let d = wait_chat(&h, &t, &chat.id, |d| {
        matches!(d.chat.state, ChatState::Failed { .. })
    })
    .await;
    match &d.items[1].kind {
        ChatItemKind::Error {
            message,
            retryable,
            code,
        } => {
            assert!(message.starts_with("Add an OpenAI API key"), "{message}");
            assert!(*retryable);
            assert_eq!(code.as_deref(), Some(error_code::NO_API_KEY));
        }
        other => panic!("{other:?}"),
    }
    let turn = d.chat.state.turn_id().unwrap().to_string();

    h.configure_ai(&t).await;
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Hi!"])));
    let r = h
        .post(&t, &format!("/api/chats/{}/continue", chat.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let c: Chat = r.json().await.unwrap();
    assert!(matches!(&c.state, ChatState::Running { turn_id, .. } if *turn_id == turn));
    let d = wait_chat(&h, &t, &chat.id, idle).await;
    assert_eq!(lines(&d)[2], "assistant: Hi!");
    assert!(d.items.iter().all(|i| i.turn_id == turn), "same turn");

    let (status, err) = api_error(
        h.post(&t, &format!("/api/chats/{}/continue", chat.id))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!((status, err.code.as_str()), (409, "cannot_continue"));

    // A model error that retrying won't fix.
    h.generator.agent.push(Scripted::Fail(AiError::Http {
        status: 400,
        message: "bad request".into(),
        retry_after: None,
        code: None,
    }));
    assert_eq!(send(&h, &t, &chat.id, "again").await.status(), 200);
    let d = wait_chat(&h, &t, &chat.id, |d| {
        matches!(d.chat.state, ChatState::Failed { .. })
    })
    .await;
    assert!(
        matches!(d.items.last().unwrap().kind, ChatItemKind::Error { .. }),
        "{:?}",
        lines(&d)
    );
    // A new message on a failed chat starts a new turn.
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Fine."])));
    assert_eq!(send(&h, &t, &chat.id, "try this").await.status(), 200);
    let d = wait_chat(&h, &t, &chat.id, idle).await;
    assert_eq!(lines(&d).last().unwrap(), "assistant: Fine.");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_that_panics_finishes_what_it_left() {
    let (h, t) = signed_in().await;
    let mut events = tool_calls_reply(&[("call_9", "list_topics", "{}")]);
    events.pop(); // no `Completed`: the panic comes first
    h.generator.agent.push(Scripted::EventsThenPanic(events));
    let chat = ask(&h, &t, "list them").await.chat;
    let d = wait_chat(&h, &t, &chat.id, |d| {
        matches!(d.chat.state, ChatState::Failed { .. })
    })
    .await;
    assert_eq!(
        lines(&d),
        vec![
            "user: list them",
            "call list_topics (cancelled)",
            "result failed: Not run",
            "error (retry): Something went wrong on the service while answering. Check its log.",
        ]
    );
    assert!(h.state.turns.live_text.lock().unwrap().is_empty());

    // Continue goes on from a valid history: the call has its answer, next to it.
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["ok"])));
    let r = h
        .post(&t, &format!("/api/chats/{}/continue", chat.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let d = wait_chat(&h, &t, &chat.id, idle).await;
    assert_eq!(lines(&d).last().unwrap(), "assistant: ok");
    let reqs = h.generator.agent.requests();
    let input = &reqs.last().unwrap().input;
    assert_eq!(
        call_outputs(input),
        vec![(
            "call_9".to_string(),
            margin_agent::error_output(CALL_FAILED)
        )]
    );
    let call = input
        .iter()
        .position(|i| i["type"] == "function_call")
        .unwrap();
    assert_eq!(input[call + 1]["type"], "function_call_output");
}

#[tokio::test(flavor = "multi_thread")]
async fn at_most_two_turns_run_at_once() {
    let (h, t) = signed_in().await;
    h.generator.agent.push(hanging_text("a"));
    h.generator.agent.push(hanging_text("b"));
    let a = ask(&h, &t, "one").await.chat;
    let b = ask(&h, &t, "two").await.chat;
    let c = ask(&h, &t, "three").await.chat;
    wait_chat(&h, &t, &a.id, |d| d.items.len() == 2).await;
    wait_chat(&h, &t, &b.id, |d| d.items.len() == 2).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(h.generator.agent.requests().len(), 2);
    let waiting = detail(&h, &t, &c.id).await;
    assert!(running(&waiting), "it shows Running while it waits");
    assert_eq!(waiting.items.len(), 1);

    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["third"])));
    let r = h
        .post(&t, &format!("/api/chats/{}/stop", a.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let d = wait_chat(&h, &t, &c.id, idle).await;
    assert_eq!(lines(&d)[1], "assistant: third");
}

// ---------------------------------------------------------------------------
// Confirmations
// ---------------------------------------------------------------------------

/// Puts `chat_id` in the state a turn is in while the loop asks for a confirmation, the
/// way the loop does it (through `ServiceSink`), and returns the card and the waiting
/// decision.
/// Sets the chat `Running` turn "turn-1" with a stand-in task, and returns its sink.
async fn fake_turn(h: &Harness, chat_id: &str, device_id: &str) -> Arc<ServiceSink> {
    let turn_id = "turn-1".to_string();
    let now = Utc::now();
    {
        let _w = h.state.write_lock.lock().await;
        let mut chat = chats::chat(&h.state, chat_id).await.unwrap();
        chat.state = ChatState::Running {
            turn_id: turn_id.clone(),
            device_id: device_id.into(),
            started_at: now,
        };
        chats::save_chat_locked(&h.state, &mut chat).await.unwrap();
        // Stands in for the turn's task.
        let task = tokio::spawn(std::future::pending::<()>());
        h.state.turns.running.lock().unwrap().insert(
            chat_id.into(),
            RunningTurn {
                turn_id: turn_id.clone(),
                task,
                decision: None,
                started_at: now,
            },
        );
    }
    // Holds a slot, as a real turn does.
    let permit = h.state.turns.slots.clone().try_acquire_owned().ok();
    Arc::new(ServiceSink {
        state: h.state.clone(),
        chat_id: chat_id.into(),
        turn_id,
        slot: Arc::new(std::sync::Mutex::new(permit)),
        model: None,
    })
}

async fn park_on_card(
    h: &Harness,
    chat_id: &str,
    device_id: &str,
) -> (
    Arc<ServiceSink>,
    ChatItem,
    tokio::task::JoinHandle<Result<Decision, margin_agent::SinkError>>,
) {
    let sink = fake_turn(h, chat_id, device_id).await;
    let call = sink
        .add_item(
            ItemStatus::InProgress,
            ChatItemKind::ToolCall {
                call_id: "call_1".into(),
                name: "delete_topic".into(),
                arguments: r#"{"topic_id":"t1"}"#.into(),
                label: "Deleting a topic".into(),
                device: None,
            },
            ItemRecord {
                model_items: Some(vec![function_call_item(
                    "fc_1",
                    "call_1",
                    "delete_topic",
                    r#"{"topic_id":"t1"}"#,
                )]),
                full_output: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(call.seq, 1);
    let card = sink
        .add_item(
            ItemStatus::Done,
            ChatItemKind::Card {
                call_id: "call_1".into(),
                card: Card::Confirmation(ConfirmationCard {
                    tool: "delete_topic".into(),
                    title: "Delete the topic “Fourier series”?".into(),
                    detail: "3 lessons go with it.".into(),
                    approve_label: "Delete".into(),
                    decision: Decision::Pending,
                }),
            },
            ItemRecord::default(),
        )
        .await
        .unwrap();
    let waiting = {
        let sink = sink.clone();
        let card = card.clone();
        tokio::spawn(async move { sink.wait_for_decision(&card).await })
    };
    (sink, card, waiting)
}

#[tokio::test(flavor = "multi_thread")]
async fn any_device_answers_a_confirmation() {
    let (h, t) = signed_in().await;
    let phone = h.sign_in("iPhone").await;
    let mac_id = me(&h, &t).await.device.id;
    let phone_id = me(&h, &phone).await.device.id;
    let chat = create_chat(&h, &t, json!({})).await.chat;
    assert_eq!(chat.title, "");
    assert_eq!(chat.state, ChatState::Idle);
    let confirm = |token: &str, item: &str, approve: bool| {
        h.post(token, &format!("/api/chats/{}/confirm", chat.id))
            .json(&json!({"item_id": item, "approve": approve}))
            .send()
    };
    let (status, err) = api_error(confirm(&t, "x", true).await.unwrap()).await;
    assert_eq!((status, err.code.as_str()), (409, "not_waiting"));

    let (_sink, card, waiting) = park_on_card(&h, &chat.id, &mac_id).await;
    let d = wait_chat(&h, &t, &chat.id, |d| {
        matches!(&d.chat.state, ChatState::WaitingForUser { item_id, .. } if *item_id == card.id)
    })
    .await;
    assert_eq!(
        d.chat.state,
        ChatState::WaitingForUser {
            turn_id: "turn-1".into(),
            device_id: mac_id.clone(),
            item_id: card.id.clone(),
        }
    );
    // A busy chat queues messages, so this needs no model.
    let (status, err) = api_error(confirm(&phone, "another-card", true).await.unwrap()).await;
    assert_eq!((status, err.code.as_str()), (409, "not_waiting"));

    let mut sock = h.socket(&t).await;
    let r = confirm(&phone, &card.id, true).await.unwrap();
    assert_eq!(r.status(), 200);
    let c: Chat = r.json().await.unwrap();
    assert!(matches!(&c.state, ChatState::Running { device_id, .. } if *device_id == mac_id));
    assert_eq!(
        waiting.await.unwrap(),
        Ok(Decision::Approved {
            device_id: phone_id.clone()
        })
    );
    let events = sock.until(|e| matches!(e, Event::ChatChanged { .. })).await;
    assert!(events.iter().any(|e| matches!(
        e,
        Event::ChatItemChanged { item } if item.id == card.id
            && matches!(&item.kind, ChatItemKind::Card { card: Card::Confirmation(c), .. } if c.decision == Decision::Approved { device_id: phone_id.clone() })
    )));
    let (status, _) = api_error(confirm(&t, &card.id, false).await.unwrap()).await;
    assert_eq!(status, 409, "answered once");
}

#[tokio::test(flavor = "multi_thread")]
async fn turns_waiting_on_the_owner_leave_their_slot() {
    let (h, t) = signed_in().await;
    let mac_id = me(&h, &t).await.device.id;
    let a = create_chat(&h, &t, json!({})).await.chat;
    let b = create_chat(&h, &t, json!({})).await.chat;
    let (sink_a, card_a, waiting_a) = park_on_card(&h, &a.id, &mac_id).await;
    let (_sink_b, _card_b, _waiting_b) = park_on_card(&h, &b.id, &mac_id).await;
    for chat in [&a, &b] {
        wait_chat(&h, &t, &chat.id, |d| {
            matches!(d.chat.state, ChatState::WaitingForUser { .. })
        })
        .await;
    }
    assert_eq!(h.state.turns.slots.available_permits(), 2);

    // Two parked turns don't hold up a third chat.
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["third"])));
    let c = ask(&h, &t, "three").await.chat;
    let d = wait_chat(&h, &t, &c.id, idle).await;
    assert_eq!(lines(&d)[1], "assistant: third");

    // Answered, the turn takes a slot again before it goes on.
    let r = h
        .post(&t, &format!("/api/chats/{}/confirm", a.id))
        .json(&json!({"item_id": card_a.id, "approve": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(matches!(
        waiting_a.await.unwrap(),
        Ok(Decision::Approved { .. })
    ));
    assert!(sink_a.slot.lock().unwrap().is_some());
    assert_eq!(h.state.turns.slots.available_permits(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn deny_answers_a_confirmation() {
    let (h, t) = signed_in().await;
    let mac_id = me(&h, &t).await.device.id;
    let chat = create_chat(&h, &t, json!({})).await.chat;
    let (_sink, card, waiting) = park_on_card(&h, &chat.id, &mac_id).await;
    wait_chat(&h, &t, &chat.id, |d| {
        matches!(d.chat.state, ChatState::WaitingForUser { .. })
    })
    .await;
    let r = h
        .post(&t, &format!("/api/chats/{}/confirm", chat.id))
        .json(&json!({"item_id": card.id, "approve": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        waiting.await.unwrap(),
        Ok(Decision::Denied {
            device_id: mac_id,
            by_stop: false
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_denies_a_waiting_confirmation() {
    let (h, t) = signed_in().await;
    let phone = h.sign_in("iPhone").await;
    let mac_id = me(&h, &t).await.device.id;
    let phone_id = me(&h, &phone).await.device.id;
    let chat = create_chat(&h, &t, json!({})).await.chat;
    let (_sink, card, _waiting) = park_on_card(&h, &chat.id, &mac_id).await;
    wait_chat(&h, &t, &chat.id, |d| {
        matches!(d.chat.state, ChatState::WaitingForUser { .. })
    })
    .await;
    let r = h
        .post(&phone, &format!("/api/chats/{}/stop", chat.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let d = detail(&h, &t, &chat.id).await;
    assert_eq!(d.chat.state, ChatState::Idle);
    assert_eq!(
        lines(&d),
        vec![
            "call delete_topic (cancelled)".to_string(),
            format!(
                "card confirm: {:?}",
                Decision::Denied {
                    device_id: phone_id,
                    by_stop: true
                }
            ),
            "result failed: Cancelled".to_string(),
            format!("action: {STOPPED}"),
        ]
    );
    assert!(d.items.iter().any(|i| i.id == card.id));
}

// ---------------------------------------------------------------------------
// Recovery
// ---------------------------------------------------------------------------

fn item(id: &str, seq: u64, status: ItemStatus, kind: ChatItemKind) -> ChatItem {
    let now = Utc::now();
    ChatItem {
        id: id.into(),
        chat_id: "c1".into(),
        turn_id: "t1".into(),
        seq,
        status,
        created_at: now,
        updated_at: now,
        kind,
    }
}

async fn put_item(store: &Store, item: ChatItem, model_items: Vec<Value>) {
    store
        .put(
            db::CHAT_ITEM,
            &item.id.clone(),
            &chats::StoredChatItem {
                item,
                model_items,
                full_output: None,
            },
        )
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_interrupts_running_turns_and_continue_resumes() {
    let store = Store::memory().await.unwrap();
    let now = Utc::now();
    let chat = Chat {
        id: "c1".into(),
        title: "Fourier".into(),
        title_by: Default::default(),
        state: ChatState::WaitingForUser {
            turn_id: "t1".into(),
            device_id: "old-mac".into(),
            item_id: "i5".into(),
        },
        workspace_ids: vec![],
        source_ids: vec![],
        pending: Some(PendingMessage {
            text: "and then?".into(),
            source_ids: vec![],
            device_id: "old-mac".into(),
            created_at: now,
        }),
        usage: Usage::default(),
        waiting: None,
        created_at: now,
        updated_at: now,
        provider: Default::default(),
    };
    store.put(db::CHAT, "c1", &chat).await.unwrap();
    let user_input =
        json!({"role": "user", "content": [{"type": "input_text", "text": "teach me"}]});
    put_item(
        &store,
        item(
            "i1",
            1,
            ItemStatus::Done,
            ChatItemKind::UserMessage {
                text: "teach me".into(),
                source_ids: vec![],
                device_id: "old-mac".into(),
                device_name: "Old Mac".into(),
            },
        ),
        vec![user_input],
    )
    .await;
    put_item(
        &store,
        item(
            "i2",
            2,
            ItemStatus::InProgress,
            ChatItemKind::AssistantText {
                text: "Half".into(),
                citations: vec![],
            },
        ),
        vec![],
    )
    .await;
    for (id, seq, call_id, name) in [
        ("i3", 3, "call_1", "list_topics"),
        ("i4", 4, "call_2", "delete_topic"),
    ] {
        put_item(
            &store,
            item(
                id,
                seq,
                ItemStatus::InProgress,
                ChatItemKind::ToolCall {
                    call_id: call_id.into(),
                    name: name.into(),
                    arguments: "{}".into(),
                    label: String::new(),
                    device: None,
                },
            ),
            vec![function_call_item(
                &format!("fc_{call_id}"),
                call_id,
                name,
                "{}",
            )],
        )
        .await;
    }
    put_item(
        &store,
        item(
            "i5",
            5,
            ItemStatus::Done,
            ChatItemKind::Card {
                call_id: "call_2".into(),
                card: Card::Confirmation(ConfirmationCard {
                    tool: "delete_topic".into(),
                    title: "Delete?".into(),
                    detail: String::new(),
                    approve_label: "Delete".into(),
                    decision: Decision::Pending,
                }),
            },
        ),
        vec![],
    )
    .await;
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
    *h.state.turns.retry.lock().unwrap() = RetryPolicy::immediate();
    let t = h.sign_in("Mac").await;

    let d = detail(&h, &t, "c1").await;
    assert_eq!(
        d.chat.state,
        ChatState::Interrupted {
            turn_id: "t1".into()
        }
    );
    assert_eq!(d.chat.pending.as_ref().unwrap().text, "and then?", "kept");
    assert_eq!(
        lines(&d),
        vec![
            "user: teach me".to_string(),
            "assistant: Half (cancelled)".to_string(),
            "call list_topics (cancelled)".to_string(),
            "call delete_topic (cancelled)".to_string(),
            "card confirm: Expired".to_string(),
            "result failed: Interrupted".to_string(),
            "result failed: Interrupted".to_string(),
            format!("action: {STOPPED_BY_RESTART}"),
        ]
    );
    let output = |call: &str| {
        let r = d
            .items
            .iter()
            .find(
                |i| matches!(&i.kind, ChatItemKind::ToolResult { call_id, .. } if call_id == call),
            )
            .unwrap();
        r.id.clone()
    };
    let text: ToolOutputText = h
        .get(
            &t,
            &format!("/api/chats/c1/items/{}/output", output("call_1")),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(text.text, json!({"error": CALL_INTERRUPTED}).to_string());
    let text: ToolOutputText = h
        .get(
            &t,
            &format!("/api/chats/c1/items/{}/output", output("call_2")),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        text.text,
        json!({"error": CONFIRMATION_EXPIRED}).to_string()
    );

    // A new message waits behind the kept one.
    let (status, err) = api_error(send(&h, &t, "c1", "new").await).await;
    assert_eq!((status, err.code.as_str()), (409, "already_pending"));

    // Continue resumes the turn for the device that started it; then the pending
    // message goes.
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Resumed."])));
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Then this."])));
    let r = h.post(&t, "/api/chats/c1/continue").send().await.unwrap();
    assert_eq!(r.status(), 200);
    let c: Chat = r.json().await.unwrap();
    assert!(
        matches!(&c.state, ChatState::Running { turn_id, device_id, .. } if turn_id == "t1" && device_id == "old-mac")
    );
    let d = wait_chat(&h, &t, "c1", |d| idle(d) && d.items.len() == 11).await;
    assert_eq!(
        lines(&d)[8..],
        [
            "assistant: Resumed.",
            "user: and then?",
            "assistant: Then this.",
        ]
    );
    match &d.items[9].kind {
        ChatItemKind::UserMessage { device_name, .. } => {
            assert_eq!(device_name, "A removed device")
        }
        other => panic!("{other:?}"),
    }
    let reqs = h.generator.agent.requests();
    let outputs = call_outputs(&reqs[0].input);
    assert_eq!(outputs.len(), 2);
    assert!(has_developer_text(&reqs[0].input, STOPPED_BY_RESTART));
    assert_eq!(d.chat.pending, None);
}

// ---------------------------------------------------------------------------
// Chat records
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn chats_are_renamed_listed_and_checked() {
    let (h, t) = signed_in().await;
    let a = create_chat(&h, &t, json!({})).await.chat;
    let b = create_chat(&h, &t, json!({})).await.chat;
    let chats: Vec<Chat> = h
        .get(&t, "/api/chats")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        chats.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        vec![b.id.as_str(), a.id.as_str()],
        "newest first"
    );

    let mut sock = h.socket(&t).await;
    let r = h
        .patch(&t, &format!("/api/chats/{}", a.id))
        .json(&json!({"title": "  Signals  "}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let renamed: Chat = r.json().await.unwrap();
    assert_eq!(renamed.title, "Signals");
    assert_eq!(renamed.title_by, TitleSource::Owner);
    let events = sock.until(|e| matches!(e, Event::ChatChanged { .. })).await;
    assert!(matches!(events.last(), Some(Event::ChatChanged { chat }) if chat.title == "Signals"));
    let long: Chat = h
        .patch(&t, &format!("/api/chats/{}", a.id))
        .json(&json!({"title": "x".repeat(300)}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(long.title.chars().count(), 200);

    for (body, code) in [
        (json!({"title": "  "}), "bad_request"),
        (json!({"workspace_ids": ["nope"]}), "unknown_workspace"),
        (json!({"source_ids": ["nope"]}), "unknown_source"),
    ] {
        let (status, err) = api_error(
            h.patch(&t, &format!("/api/chats/{}", a.id))
                .json(&body)
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!((status, err.code.as_str()), (400, code), "{body}");
    }
    for (body, code) in [
        (json!({"message": {"text": "   "}}), "bad_request"),
        (
            json!({"message": {"text": "x".repeat(chats::MAX_MESSAGE_BYTES + 1)}}),
            "bad_request",
        ),
        (
            json!({"message": {"text": "hi", "source_ids": ["gone"]}}),
            "unknown_source",
        ),
        (json!({"workspace_ids": ["gone"]}), "unknown_workspace"),
    ] {
        let (status, err) =
            api_error(h.post(&t, "/api/chats").json(&body).send().await.unwrap()).await;
        assert_eq!((status, err.code.as_str()), (400, code));
    }
    let (status, _) = api_error(send(&h, &t, &a.id, "").await).await;
    assert_eq!(status, 400);
    for path in [
        "/api/chats/nope".to_string(),
        "/api/chats/nope/items/x/output".to_string(),
        format!("/api/chats/{}/items/x/output", a.id),
    ] {
        let r = h.get(&t, &path).send().await.unwrap();
        assert_eq!(r.status(), 404, "{path}");
    }
    let (status, err) = api_error(
        h.post(&t, &format!("/api/chats/{}/stop", a.id))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!((status, err.code.as_str()), (409, "not_running"));
    let (status, err) = api_error(
        h.post(&t, &format!("/api/chats/{}/continue", a.id))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!((status, err.code.as_str()), (409, "cannot_continue"));
    let r = h
        .delete(&t, &format!("/api/chats/{}/pending", a.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "nothing pending is fine");

    let r = h
        .delete(&t, &format!("/api/chats/{}", b.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let events = sock.until(|e| matches!(e, Event::ChatDeleted { .. })).await;
    assert!(matches!(events.last(), Some(Event::ChatDeleted { id }) if *id == b.id));
    let r = h
        .delete(&t, &format!("/api/chats/{}", b.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_chat_stops_it_and_takes_its_own_files() {
    let (h, t) = signed_in().await;
    let topic = new_topic(&h, &t, None, "Kept").await;
    let mine = upload_ready(&h, &t, "mine.md", None, "chat only").await;
    let shared = upload_ready(&h, &t, "shared.md", None, "also in another chat").await;
    let in_topic = upload_ready(&h, &t, "topic.md", Some(&topic.id), "in a topic").await;

    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["ok"])));
    let other = create_chat(
        &h,
        &t,
        json!({"message": {"text": "other", "source_ids": [shared.id]}}),
    )
    .await
    .chat;
    wait_chat(&h, &t, &other.id, idle).await;

    h.generator.agent.push(hanging_text("busy"));
    let chat = create_chat(
        &h,
        &t,
        json!({"message": {"text": "read these", "source_ids": [mine.id, shared.id, in_topic.id, mine.id]}}),
    )
    .await;
    assert_eq!(
        chat.chat.source_ids,
        vec![mine.id.clone(), shared.id.clone(), in_topic.id.clone()]
    );
    match &chat.items[0].kind {
        ChatItemKind::UserMessage { source_ids, .. } => assert_eq!(source_ids.len(), 3),
        other => panic!("{other:?}"),
    }
    let req_input =
        eventually(|| async { h.generator.agent.requests().get(1).map(|r| r.input.clone()) }).await;
    // The message's context names the attached files.
    assert!(req_input.iter().any(|i| i.to_string().contains("mine.md")));

    let mut sock = h.socket(&t).await;
    let r = h
        .delete(&t, &format!("/api/chats/{}", chat.chat.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let events = sock.until(|e| matches!(e, Event::ChatDeleted { .. })).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::SourceDeleted { id } if *id == mine.id))
    );
    let gone: Vec<chats::StoredChatItem> = h
        .state
        .store
        .list_where(db::CHAT_ITEM, "chat_id", &chat.chat.id)
        .await
        .unwrap();
    assert!(gone.is_empty());
    let left: Vec<Source> = h.state.store.list(db::SOURCE).await.unwrap();
    let mut names: Vec<&str> = left.iter().map(|s| s.name.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["shared.md", "topic.md"]);
    assert!(h.state.turns.running.lock().unwrap().is_empty());

    // Dropping a file from a chat deletes it when nothing else has it.
    let r = h
        .patch(&t, &format!("/api/chats/{}", other.id))
        .json(&json!({"source_ids": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        h.state
            .store
            .get::<Source>(db::SOURCE, &shared.id)
            .await
            .unwrap()
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// Devices and folders
// ---------------------------------------------------------------------------

async fn devices(h: &Harness, token: &str) -> Vec<Device> {
    h.get(token, "/api/devices")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn sidebar_call(timeout: Duration) -> DeviceCall {
    DeviceCall {
        chat_id: "c1".into(),
        call_id: "call_1".into(),
        call: DeviceToolCall::ToggleSidebar { visible: None },
        timeout,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn devices_are_online_while_their_socket_is_open_and_answer_tool_requests() {
    let h = start().await;
    let mac = h.sign_in("Mac").await;
    let phone = h.sign_in("iPhone").await;
    let phone_id = me(&h, &phone).await.device.id;
    let mut watcher = h.socket(&mac).await;
    assert!(
        !devices(&h, &mac)
            .await
            .iter()
            .any(|d| d.id == phone_id && d.online)
    );
    assert!(matches!(
        h.state
            .devices
            .call(&phone_id, sidebar_call(Duration::from_secs(1)))
            .await,
        Err(DeviceError::Offline)
    ));

    let mut device = h.socket(&phone).await;
    let events = watcher
        .until(|e| matches!(e, Event::DeviceChanged { device } if device.id == phone_id))
        .await;
    match events.last() {
        Some(Event::DeviceChanged { device }) => {
            assert!(device.online);
            assert!(!device.current);
            assert_eq!(device.name, "iPhone");
        }
        other => panic!("{other:?}"),
    }
    assert!(
        devices(&h, &mac)
            .await
            .iter()
            .any(|d| d.id == phone_id && d.online)
    );
    assert!(me(&h, &phone).await.device.online);

    // A request reaches only that device; its answer completes the call.
    let call = {
        let hub = h.state.devices.clone();
        let id = phone_id.clone();
        tokio::spawn(async move { hub.call(&id, sidebar_call(Duration::from_secs(5))).await })
    };
    let request = match device.next_directed().await {
        DirectedFrame::ToolRequest { request } => request,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        request.call,
        DeviceToolCall::ToggleSidebar { visible: None }
    );
    assert_eq!(request.call_id, "call_1");
    // Something a device shouldn't send is ignored.
    device
        .ws
        .send(tokio_tungstenite::tungstenite::Message::Text(
            "{\"type\":\"nonsense\"}".into(),
        ))
        .await
        .unwrap();
    device
        .send_frame(&DeviceFrame::ToolResult {
            request_id: request.request_id.clone(),
            result: DeviceToolResult::Ok {
                output: DeviceToolOutput::Done {
                    message: "Sidebar hidden.".into(),
                },
            },
        })
        .await;
    assert_eq!(
        call.await.unwrap(),
        Ok(DeviceToolOutput::Done {
            message: "Sidebar hidden.".into()
        })
    );

    // Ignored: the call times out and the device is told to stop.
    assert_eq!(
        h.state
            .devices
            .call(&phone_id, sidebar_call(Duration::from_millis(200)))
            .await,
        Err(DeviceError::TimedOut)
    );
    let request = match device.next_directed().await {
        DirectedFrame::ToolRequest { request } => request,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        device.next_directed().await,
        DirectedFrame::ToolCancel {
            request_id: request.request_id
        }
    );

    device.close().await;
    let events = watcher
        .until(|e| matches!(e, Event::DeviceChanged { device } if device.id == phone_id && !device.online))
        .await;
    assert!(!events.is_empty());
    assert!(
        !devices(&h, &mac)
            .await
            .iter()
            .any(|d| d.id == phone_id && d.online)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn folders_belong_to_their_device_and_leave_chats_when_removed() {
    let (h, mac) = signed_in().await;
    let phone = h.sign_in("iPhone").await;
    let phone_id = me(&h, &phone).await.device.id;
    let r = h
        .post(&phone, "/api/workspaces")
        .json(&json!({"name": " notes ", "platform": "ios", "location": "iCloud Drive/notes"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let ws: Workspace = r.json().await.unwrap();
    assert_eq!(ws.name, "notes");
    assert_eq!(ws.device_id, phone_id);
    assert_eq!(ws.device_name, "iPhone");
    let (status, _) = api_error(
        h.post(&phone, "/api/workspaces")
            .json(&json!({"name": "", "platform": "ios", "location": "x"}))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, 400);
    let all: Vec<Workspace> = h
        .get(&mac, "/api/workspaces")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(all, vec![ws.clone()]);

    let chat = create_chat(&h, &mac, json!({"workspace_ids": [ws.id]}))
        .await
        .chat;
    assert_eq!(chat.workspace_ids, vec![ws.id.clone()]);

    let (status, err) = api_error(
        h.delete(&mac, &format!("/api/workspaces/{}", ws.id))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!((status, err.code.as_str()), (403, "not_yours"));

    let mut sock = h.socket(&mac).await;
    let r = h
        .delete(&phone, &format!("/api/workspaces/{}", ws.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let events = sock
        .until(|e| matches!(e, Event::WorkspaceDeleted { .. }))
        .await;
    assert!(events.iter().any(|e| matches!(e, Event::ChatChanged { chat: c } if c.id == chat.id && c.workspace_ids.is_empty())));
    assert!(
        detail(&h, &mac, &chat.id)
            .await
            .chat
            .workspace_ids
            .is_empty()
    );

    // Revoking a device removes its folders.
    let r = h
        .post(&phone, "/api/workspaces")
        .json(&json!({"name": "docs", "platform": "android", "location": "Documents"}))
        .send()
        .await
        .unwrap();
    let ws2: Workspace = r.json().await.unwrap();
    let r = h
        .patch(&mac, &format!("/api/chats/{}", chat.id))
        .json(&json!({"workspace_ids": [ws2.id]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let mut phone_sock = h.socket(&phone).await;
    eventually(|| async { h.state.devices.is_online(&phone_id).then_some(()) }).await;
    let r = h
        .delete(&mac, &format!("/api/devices/{phone_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    // The hub forgets it and its socket closes.
    assert!(!h.state.devices.is_online(&phone_id));
    phone_sock.closed().await;
    let events = sock
        .until(|e| matches!(e, Event::DeviceDeleted { .. }))
        .await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::WorkspaceDeleted { id } if *id == ws2.id))
    );
    assert!(
        detail(&h, &mac, &chat.id)
            .await
            .chat
            .workspace_ids
            .is_empty()
    );
    let all: Vec<Workspace> = h
        .get(&mac, "/api/workspaces")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(all.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sink_streams_deltas_with_offsets_and_keeps_live_text() {
    let (h, t) = signed_in().await;
    let mac_id = me(&h, &t).await.device.id;
    let chat = create_chat(&h, &t, json!({})).await.chat;
    let now = Utc::now();
    {
        let _w = h.state.write_lock.lock().await;
        let mut c = chats::chat(&h.state, &chat.id).await.unwrap();
        c.state = ChatState::Running {
            turn_id: "turn-1".into(),
            device_id: mac_id,
            started_at: now,
        };
        chats::save_chat_locked(&h.state, &mut c).await.unwrap();
        let task = tokio::spawn(std::future::pending::<()>());
        h.state.turns.running.lock().unwrap().insert(
            chat.id.clone(),
            RunningTurn {
                turn_id: "turn-1".into(),
                task,
                decision: None,
                started_at: now,
            },
        );
    }
    let sink = ServiceSink {
        state: h.state.clone(),
        chat_id: chat.id.clone(),
        turn_id: "turn-1".into(),
        slot: Default::default(),
        model: None,
    };
    let mut sock = h.socket(&t).await;
    let item = sink
        .add_item(
            ItemStatus::InProgress,
            ChatItemKind::AssistantText {
                text: String::new(),
                citations: vec![],
            },
            ItemRecord::default(),
        )
        .await
        .unwrap();
    sink.delta(&item, "Ü");
    sink.delta(&item, "ber");
    let events = sock
        .until(|e| matches!(e, Event::ChatItemDelta { text, .. } if text == "ber"))
        .await;
    let offsets: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            Event::ChatItemDelta { offset, .. } => Some(*offset),
            _ => None,
        })
        .collect();
    assert_eq!(offsets, vec![0, 2], "byte offsets");
    let d = detail(&h, &t, &chat.id).await;
    assert_eq!(lines(&d), vec!["assistant: Über…"]);

    let mut done = item.clone();
    done.status = ItemStatus::Done;
    done.kind = ChatItemKind::AssistantText {
        text: "Über".into(),
        citations: vec![],
    };
    sink.update_item(
        &done,
        ItemRecord {
            model_items: Some(vec![json!({"type": "message"})]),
            full_output: None,
        },
    )
    .await
    .unwrap();
    assert!(h.state.turns.live_text.lock().unwrap().is_empty());
    let stored: chats::StoredChatItem = h
        .state
        .store
        .get(db::CHAT_ITEM, &item.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.model_items, vec![json!({"type": "message"})]);
    assert_eq!(line(&stored.item), "assistant: Über");

    sink.add_usage(&Usage {
        input_tokens: 10,
        cached_input_tokens: 5,
        output_tokens: 3,
        reasoning_tokens: 1,
        model_calls: 1,
        ..Default::default()
    })
    .await
    .unwrap();
    sink.add_usage(&Usage {
        input_tokens: 1,
        model_calls: 1,
        ..Usage::default()
    })
    .await
    .unwrap();
    let d = detail(&h, &t, &chat.id).await;
    assert_eq!(d.chat.usage.input_tokens, 11);
    assert_eq!(d.chat.usage.model_calls, 2);

    // A stopped turn's sink refuses to write, and its late deltas are dropped.
    h.state.turns.abort(&chat.id);
    sink.delta(&item, "late");
    assert!(h.state.turns.live_text.lock().unwrap().is_empty());
    assert!(
        sink.add_item(
            ItemStatus::Done,
            ChatItemKind::Action { text: "x".into() },
            ItemRecord::default()
        )
        .await
        .is_err()
    );
}

// ---------------------------------------------------------------------------
// Stop and started writes, waiting, titles
// ---------------------------------------------------------------------------

/// Stores an in-progress call of the fake turn, as the loop does once its arguments are
/// in.
async fn open_call(sink: &ServiceSink, call_id: &str, name: &str) -> ChatItem {
    sink.add_item(
        ItemStatus::InProgress,
        ChatItemKind::ToolCall {
            call_id: call_id.into(),
            name: name.into(),
            arguments: "{}".into(),
            label: name.into(),
            device: None,
        },
        ItemRecord {
            model_items: Some(vec![function_call_item(
                &format!("fc_{call_id}"),
                call_id,
                name,
                "{}",
            )]),
            full_output: None,
        },
    )
    .await
    .unwrap()
}

/// A tool result's full output, by call id.
async fn output_of(h: &Harness, token: &str, d: &ChatDetail, call: &str) -> String {
    let id = &d
        .items
        .iter()
        .find(|i| matches!(&i.kind, ChatItemKind::ToolResult { call_id, .. } if call_id == call))
        .unwrap()
        .id;
    let out: ToolOutputText = h
        .get(
            token,
            &format!("/api/chats/{}/items/{id}/output", d.chat.id),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    out.text
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_waits_for_started_writes_and_records_their_real_results() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let (h, t) = signed_in().await;
    let mac_id = me(&h, &t).await.device.id;
    let chat = create_chat(&h, &t, json!({})).await.chat;
    *h.state.turns.stop_wait.lock().unwrap() = Duration::from_millis(500);
    let sink = fake_turn(&h, &chat.id, &mac_id).await;
    open_call(&sink, "call_topic", "create_topic").await;
    open_call(&sink, "call_slow", "rename_topic").await;
    open_call(&sink, "call_read", "list_topics").await;
    // A change that finishes while Stop waits, and one that doesn't in time.
    h.state.turns.register_write(
        &chat.id,
        "call_topic",
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let mut out = ToolOutput::ok(r#"{"id":"t9","title":"Waves"}"#, "Created “Waves”");
            out.cards.push(Card::Result(ResultCard {
                target: ResultTarget::Topic {
                    topic_id: "t9".into(),
                },
                title: "Waves".into(),
            }));
            out
        }),
    );
    let (slow_tx, slow_rx) = tokio::sync::oneshot::channel::<()>();
    let slow_done = Arc::new(AtomicBool::new(false));
    h.state.turns.register_write(&chat.id, "call_slow", {
        let done = slow_done.clone();
        tokio::spawn(async move {
            let _ = slow_rx.await;
            done.store(true, Ordering::SeqCst);
            ToolOutput::ok("{}", "Renamed")
        })
    });

    let mut sock = h.socket(&t).await;
    let r = h
        .post(&t, &format!("/api/chats/{}/stop", chat.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let stopped: Chat = r.json().await.unwrap();
    assert_eq!(stopped.state, ChatState::Idle);
    let d = detail(&h, &t, &chat.id).await;
    assert_eq!(
        lines(&d),
        vec![
            "call create_topic".to_string(),
            "call rename_topic (cancelled)".to_string(),
            "call list_topics (cancelled)".to_string(),
            "result ok: Created “Waves”".to_string(),
            "card result".to_string(),
            "result failed: Stopped".to_string(),
            "result failed: Cancelled".to_string(),
            format!("action: {STOPPED}"),
        ]
    );
    assert_eq!(
        output_of(&h, &t, &d, "call_topic").await,
        r#"{"id":"t9","title":"Waves"}"#
    );
    assert_eq!(
        output_of(&h, &t, &d, "call_slow").await,
        margin_agent::error_output(WRITE_UNCONFIRMED)
    );
    // In the contract's order: the real result, the call done, its card.
    let events = sock
        .until(|e| matches!(e, Event::ChatChanged { chat } if chat.state == ChatState::Idle))
        .await;
    let order: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            Event::ChatItemChanged { item } => Some(line(item)),
            _ => None,
        })
        .collect();
    assert_eq!(
        &order[..3],
        [
            "result ok: Created “Waves”",
            "call create_topic",
            "card result"
        ]
    );
    assert!(h.state.turns.take_writes(&chat.id).is_empty());

    // The slow change still runs to its end.
    slow_tx.send(()).unwrap();
    eventually(|| async { slow_done.load(Ordering::SeqCst).then_some(()) }).await;

    // The next turn's model sees the real result, so it doesn't make the change again.
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Done."])));
    assert_eq!(send(&h, &t, &chat.id, "go on").await.status(), 200);
    wait_chat(&h, &t, &chat.id, idle).await;
    let req = h.generator.agent.requests().pop().unwrap();
    let outputs = call_outputs(&req.input);
    assert_eq!(outputs.len(), 3);
    assert_eq!(
        outputs[0],
        (
            "call_topic".to_string(),
            r#"{"id":"t9","title":"Waves"}"#.to_string()
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_stop_while_the_first_waits_finishes_nothing_twice() {
    let (h, t) = signed_in().await;
    let mac_id = me(&h, &t).await.device.id;
    let chat = create_chat(&h, &t, json!({})).await.chat;
    *h.state.turns.stop_wait.lock().unwrap() = Duration::from_millis(400);
    let sink = fake_turn(&h, &chat.id, &mac_id).await;
    open_call(&sink, "call_slow", "rename_topic").await;
    h.state.turns.register_write(
        &chat.id,
        "call_slow",
        tokio::spawn(std::future::pending::<ToolOutput>()),
    );
    let first = {
        let h_url = h.u(&format!("/api/chats/{}/stop", chat.id));
        let http = h.http.clone();
        let t = t.clone();
        tokio::spawn(async move { http.post(h_url).bearer_auth(t).send().await.unwrap() })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let second = h
        .post(&t, &format!("/api/chats/{}/stop", chat.id))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 200);
    assert_eq!(first.await.unwrap().status(), 200);
    let d = detail(&h, &t, &chat.id).await;
    assert_eq!(
        lines(&d),
        vec![
            "call rename_topic (cancelled)".to_string(),
            "result failed: Stopped".to_string(),
            format!("action: {STOPPED}"),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_whose_request_is_dropped_still_finishes_the_turn() {
    let (h, t) = signed_in().await;
    let mac_id = me(&h, &t).await.device.id;
    let chat = create_chat(&h, &t, json!({})).await.chat;
    *h.state.turns.stop_wait.lock().unwrap() = Duration::from_millis(300);
    let sink = fake_turn(&h, &chat.id, &mac_id).await;
    open_call(&sink, "call_slow", "rename_topic").await;
    h.state.turns.register_write(
        &chat.id,
        "call_slow",
        tokio::spawn(std::future::pending::<ToolOutput>()),
    );
    // The client goes away while Stop waits for the change.
    let request = {
        let url = h.u(&format!("/api/chats/{}/stop", chat.id));
        let http = h.http.clone();
        let t = t.clone();
        tokio::spawn(async move { http.post(url).bearer_auth(t).send().await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    request.abort();
    let d = wait_chat(&h, &t, &chat.id, idle).await;
    assert_eq!(
        lines(&d),
        vec![
            "call rename_topic (cancelled)".to_string(),
            "result failed: Stopped".to_string(),
            format!("action: {STOPPED}"),
        ]
    );
    assert!(h.state.turns.stopping.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_finishes_a_write_the_loop_was_still_recording() {
    let (h, t) = signed_in().await;
    let mac_id = me(&h, &t).await.device.id;
    let chat = create_chat(&h, &t, json!({})).await.chat;
    let sink = fake_turn(&h, &chat.id, &mac_id).await;
    open_call(&sink, "call_topic", "create_topic").await;
    let mut out = ToolOutput::ok(r#"{"id":"t9","title":"Waves"}"#, "Created “Waves”");
    out.cards.push(Card::Result(ResultCard {
        target: ResultTarget::Topic {
            topic_id: "t9".into(),
        },
        title: "Waves".into(),
    }));
    h.state.turns.register_write(&chat.id, "call_topic", {
        let out = out.clone();
        tokio::spawn(async move { out })
    });
    // The loop's `record` stored the result; Stop lands before the call is `Done` and
    // before its card.
    sink.add_item(
        ItemStatus::Done,
        ChatItemKind::ToolResult {
            call_id: "call_topic".into(),
            ok: true,
            summary: out.summary.clone(),
            output_bytes: out.output.len() as u64,
        },
        ItemRecord {
            model_items: Some(vec![margin_agent::function_call_output(
                "call_topic",
                &out.output,
            )]),
            full_output: Some(out.output.clone()),
        },
    )
    .await
    .unwrap();

    let r = h
        .post(&t, &format!("/api/chats/{}/stop", chat.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let d = detail(&h, &t, &chat.id).await;
    assert_eq!(
        lines(&d),
        vec![
            "call create_topic".to_string(),
            "result ok: Created “Waves”".to_string(),
            "card result".to_string(),
            format!("action: {STOPPED}"),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_turn_records_the_writes_it_finished() {
    let (h, t) = signed_in().await;
    let mut events = tool_calls_reply(&[("call_1", "list_topics", "{}")]);
    events.pop(); // no `Completed`: the panic comes first
    h.generator.agent.push(Scripted::EventsThenPanic(events));
    let chat = create_chat(&h, &t, json!({})).await.chat;
    // Stands in for a change the call started that finished before the turn failed.
    h.state.turns.register_write(
        &chat.id,
        "call_1",
        tokio::spawn(async { ToolOutput::ok("[]", "Listed") }),
    );
    assert_eq!(send(&h, &t, &chat.id, "list").await.status(), 200);
    let d = wait_chat(&h, &t, &chat.id, |d| {
        matches!(d.chat.state, ChatState::Failed { .. })
    })
    .await;
    assert_eq!(
        lines(&d),
        vec![
            "user: list",
            "call list_topics",
            "result ok: Listed",
            "error (retry): Something went wrong on the service while answering. Check its log.",
        ]
    );
    match &d.items.last().unwrap().kind {
        ChatItemKind::Error { code, .. } => {
            assert_eq!(code.as_deref(), Some(error_code::INTERRUPTED));
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_shows_while_its_turn_waits_to_retry() {
    let (h, t) = signed_in().await;
    let mac_id = me(&h, &t).await.device.id;
    let chat = create_chat(&h, &t, json!({})).await.chat;
    let sink = fake_turn(&h, &chat.id, &mac_id).await;
    let mut sock = h.socket(&t).await;
    let until = Utc::now() + chrono::Duration::seconds(90);
    let reason = "The model API asked Margin to slow down.";
    sink.retrying(until, reason).await.unwrap();
    let events = sock
        .until(|e| matches!(e, Event::ChatChanged { chat } if chat.waiting.is_some()))
        .await;
    let Some(Event::ChatChanged { chat: c }) = events.last() else {
        unreachable!()
    };
    assert_eq!(
        c.waiting,
        Some(Waiting {
            until,
            reason: reason.into()
        })
    );
    sink.clear_waiting().await.unwrap();
    let events = sock
        .until(|e| matches!(e, Event::ChatChanged { chat } if chat.waiting.is_none()))
        .await;
    assert_chat_changes_are_real(&events);

    // Stop clears it too.
    sink.retrying(until, reason).await.unwrap();
    let r = h
        .post(&t, &format!("/api/chats/{}/stop", chat.id))
        .send()
        .await
        .unwrap();
    let c: Chat = r.json().await.unwrap();
    assert_eq!((c.state, c.waiting), (ChatState::Idle, None));
    // A stopped turn's sink can't set it again.
    assert!(sink.retrying(until, reason).await.is_err());
    assert_eq!(detail(&h, &t, &chat.id).await.chat.waiting, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_model_titles_a_chat_after_its_first_answer() {
    let (h, t) = signed_in().await;
    h.title_reply("“Fourier series, step by step.”");
    h.generator.agent.push(Scripted::Events(text_reply(&[
        "Fourier series are sums of waves.",
    ])));
    let created = ask(&h, &t, "Teach me Fourier series\nplease").await;
    assert_eq!(created.chat.title_by, TitleSource::FirstLine);
    let d = wait_chat(&h, &t, &created.chat.id, |d| {
        d.chat.title_by == TitleSource::Model
    })
    .await;
    assert_eq!(d.chat.title, "Fourier series, step by step");
    assert_eq!(d.chat.usage.model_calls, 2, "the title call counts");
    assert_eq!(d.items.len(), 2, "not an item");

    // A plain text call on the default provider's small slot.
    let calls = h.generator.text_calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.opts.route, Route::Small);
    assert_eq!(call.opts.max_output_tokens, Some(TITLE_MAX_OUTPUT_TOKENS));
    assert_eq!(call.opts.cache_key.as_deref(), Some(d.chat.id.as_str()));
    assert_eq!(
        (call.provider, call.model.as_str()),
        (Provider::OpenAi, "test-model")
    );
    assert!(call.instructions.starts_with("Write a title for this chat"));
    assert!(
        call.input.contains("Teach me Fourier series"),
        "{}",
        call.input
    );
    assert!(call.input.contains("sums of waves"), "{}", call.input);
    assert_eq!(
        h.generator.agent.requests().len(),
        1,
        "not the turn's model"
    );

    // Only after the first answer.
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["More."])));
    assert_eq!(send(&h, &t, &d.chat.id, "more").await.status(), 200);
    wait_chat(&h, &t, &d.chat.id, |d| idle(d) && d.items.len() == 4).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.generator.text_calls.lock().unwrap().len(), 1);

    // The owner's name sticks.
    let renamed: Chat = h
        .patch(&t, &format!("/api/chats/{}", d.chat.id))
        .json(&json!({"title": "Waves"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (renamed.title.as_str(), renamed.title_by),
        ("Waves", TitleSource::Owner)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_title_that_fails_or_comes_after_a_rename_changes_nothing() {
    let (h, t) = signed_in().await;
    // Nothing queued: the call fails and the first line stays.
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Hi!"])));
    let a = ask(&h, &t, "Hello there").await.chat;
    wait_chat(&h, &t, &a.id, idle).await;
    eventually(|| async { (h.generator.text_calls.lock().unwrap().len() == 1).then_some(()) })
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let d = detail(&h, &t, &a.id).await;
    assert_eq!(
        (d.chat.title.as_str(), d.chat.title_by),
        ("Hello there", TitleSource::FirstLine)
    );

    // Renamed while the title call runs: the owner's title stays.
    h.generator.titles.push(Scripted::Delayed(
        Duration::from_millis(300),
        Box::new(Scripted::Events(text_reply(&["Model title"]))),
    ));
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Hi!"])));
    let b = ask(&h, &t, "Another one").await.chat;
    wait_chat(&h, &t, &b.id, idle).await;
    let r = h
        .patch(&t, &format!("/api/chats/{}", b.id))
        .json(&json!({"title": "Mine"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let d = wait_chat(&h, &t, &b.id, |d| d.chat.usage.model_calls == 2).await;
    assert_eq!(
        (d.chat.title.as_str(), d.chat.title_by),
        ("Mine", TitleSource::Owner)
    );
}

// ---------------------------------------------------------------------------
// Traces
// ---------------------------------------------------------------------------

/// Prices for the test model, in the data folder (replacing the built-in table).
const TEST_PRICES: &str = "[models.test-model]\ninput = 1.0\ncached_input = 0.5\noutput = 2.0\n";

async fn traced() -> (Harness, String) {
    let h = start_with(Setup {
        files: vec![("prices.toml".into(), TEST_PRICES.as_bytes().to_vec())],
        ..Setup::default()
    })
    .await;
    *h.state.turns.retry.lock().unwrap() = RetryPolicy::immediate();
    let t = h.sign_in("Mac").await;
    h.configure_ai(&t).await;
    (h, t)
}

/// `GET /api/traces` with `query`, as the rows and the raw body.
async fn traces(h: &Harness, token: &str, query: &[(&str, String)]) -> (Vec<TraceRow>, String) {
    let r = h
        .get(token, routes::TRACES)
        .query(query)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers()["content-type"].to_str().unwrap(),
        "application/x-ndjson"
    );
    let body = r.text().await.unwrap();
    let rows = body
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    (rows, body)
}

async fn rollup(h: &Harness, token: &str, bucket: &str) -> Vec<TraceRollup> {
    let r = h
        .get(token, routes::TRACES_ROLLUP)
        .query(&[("bucket", bucket)])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_is_traced_with_ids_and_numbers_only() {
    let (h, t) = traced().await;
    let mut live = h.state.tracer.subscribe();
    new_topic(&h, &t, None, "Zanzibar archipelago").await;
    h.generator.agent.push(Scripted::Events(tool_calls_reply(&[
        ("call_1", "list_topics", "{}"),
        ("call_2", "search_library", r#"{"query":"zanzibar"}"#),
    ])));
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["The quokka answer."])));
    h.title_reply("Islands of spice");
    let chat = ask(&h, &t, "Tell me about the narwhal secret").await.chat;
    let d = wait_chat(&h, &t, &chat.id, |d| {
        idle(d) && d.chat.title_by == TitleSource::Model
    })
    .await;
    let turn_id = d.items[0].turn_id.clone();

    let (rows, body) = traces(&h, &t, &[("trace_id", turn_id.clone())]).await;
    assert!(rows.windows(2).all(|w| w[0].at <= w[1].at), "oldest first");
    assert!(rows.iter().all(|r| r.trace_id == turn_id));
    let of = |kind: TraceKind| rows.iter().filter(move |r| r.kind == kind);

    let turn: Vec<&TraceRow> = of(TraceKind::Turn).collect();
    assert_eq!(turn.len(), 1);
    assert!(turn[0].ok && turn[0].duration_ms > 0.0);
    assert_eq!(turn[0].chat_id.as_deref(), Some(chat.id.as_str()));

    let calls: Vec<&TraceRow> = of(TraceKind::ModelCall).collect();
    assert_eq!(calls.len(), 3, "two steps and the title: {calls:#?}");
    // The service stamps its own embedder (ONNX).
    let mut pipeline = PipelineConfig::default();
    pipeline.embedder.name = margin_embed::Backend::default().name().into();
    let pipeline = pipeline.hash();
    for c in &calls {
        assert!(c.ok);
        assert_eq!(c.pipeline_hash.as_deref(), Some(pipeline.as_str()));
        assert_eq!(c.model.as_deref(), Some("test-model"));
        assert_eq!(c.provider.as_deref(), Some("openai"));
        assert_eq!(c.stop_reason.as_deref(), Some("completed"));
        assert_eq!(c.actor.as_deref(), Some("owner"));
        let u = c.usage.expect("usage");
        assert!(u.input == 100 && u.output > 0, "{u:?}");
        let cost = c.cost_usd.expect("priced at write time");
        let expected = (u.input as f64 * 1.0 + u.output as f64 * 2.0) / 1e6;
        assert!((cost - expected).abs() < 1e-12, "{cost}");
    }
    let steps: Vec<&&TraceRow> = calls
        .iter()
        .filter(|c| c.route == Some(Route::Chat))
        .collect();
    assert_eq!(
        steps.iter().map(|c| c.step).collect::<Vec<_>>(),
        [Some(1), Some(2)]
    );
    assert!(steps.iter().all(|c| c.prompt_version.as_deref()
        == Some(margin_agent::AGENT_PROMPT_VERSION)
        && c.parent_id.as_deref() == Some(turn[0].id.as_str())));
    let title = calls
        .iter()
        .find(|c| c.route == Some(Route::Small))
        .unwrap();
    assert!(
        title
            .prompt_version
            .as_deref()
            .unwrap()
            .starts_with("title-")
    );
    assert_eq!(title.step, None);

    let tools: Vec<&TraceRow> = of(TraceKind::ToolCall).collect();
    let mut names: Vec<&str> = tools.iter().map(|r| r.tool.as_deref().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["list_topics", "search_library"]);
    let tool_outputs: Vec<u64> = d
        .items
        .iter()
        .filter_map(|i| match &i.kind {
            ChatItemKind::ToolResult { output_bytes, .. } => Some(*output_bytes),
            _ => None,
        })
        .collect();
    for r in &tools {
        assert!(r.ok);
        assert!(tool_outputs.contains(&r.output_bytes.unwrap()));
        assert_eq!(r.parent_id.as_deref(), Some(turn[0].id.as_str()));
    }

    // Nothing the owner or the model wrote, no key, no tool output.
    for secret in [
        "narwhal",
        "quokka",
        "Zanzibar",
        "zanzibar",
        "Islands of spice",
        API_KEY,
        "Margin",
    ] {
        assert!(!body.contains(secret), "{secret} is in a trace: {body}");
    }

    // By kind and window, in order.
    let from = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let to = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let (by_kind, _) = traces(
        &h,
        &t,
        &[
            ("kind", "model_call".into()),
            ("from", from.clone()),
            ("to", to.clone()),
        ],
    )
    .await;
    assert_eq!(
        by_kind.iter().map(|r| &r.id).collect::<Vec<_>>(),
        calls.iter().map(|r| &r.id).collect::<Vec<_>>()
    );
    let (none, _) = traces(
        &h,
        &t,
        &[
            ("kind", "model_call".into()),
            ("from", to.clone()),
            ("to", to.clone()),
        ],
    )
    .await;
    assert!(none.is_empty());
    let r = h
        .get(&t, routes::TRACES)
        .query(&[("from", to.clone()), ("to", from.clone())])
        .send()
        .await
        .unwrap();
    let (status, err) = api_error(r).await;
    assert_eq!((status, err.code.as_str()), (400, "bad_range"));

    // The rollups sum them, once the chat's background re-index has traced too (it
    // lands after the turn, and would otherwise change the rollup between snapshots).
    let hourly = eventually(|| async {
        let r = rollup(&h, &t, "hour").await;
        r.iter().any(|r| r.kind == TraceKind::Ingest).then_some(r)
    })
    .await;
    let model_calls: Vec<&TraceRollup> = hourly
        .iter()
        .filter(|r| r.kind == TraceKind::ModelCall)
        .collect();
    assert_eq!(model_calls.iter().map(|r| r.calls).sum::<u64>(), 3);
    assert_eq!(model_calls.iter().map(|r| r.input).sum::<u64>(), 300);
    let cost: f64 = model_calls.iter().map(|r| r.cost_usd).sum();
    let recorded: f64 = calls.iter().map(|c| c.cost_usd.unwrap()).sum();
    assert!((cost - recorded).abs() < 1e-12, "{cost} vs {recorded}");

    // Model calls go out live to whoever subscribed.
    let mut live_calls = 0;
    while let Ok(row) = live.try_recv() {
        assert!(matches!(
            row.kind,
            TraceKind::ModelCall | TraceKind::Retrieval
        ));
        live_calls += 1;
    }
    assert_eq!(live_calls, 3);

    // Retention deletes raw rows by range and keeps the rollups.
    let r = h
        .patch(&t, routes::SETTINGS)
        .json(&json!({"trace_retention_days": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let daily = rollup(&h, &t, "day").await;
    trace::apply_retention(&h.state, Utc::now() + chrono::Duration::days(3))
        .await
        .unwrap();
    let (left, _) = traces(&h, &t, &[]).await;
    assert!(left.is_empty(), "{left:#?}");
    assert_eq!(rollup(&h, &t, "day").await, daily);
    assert_eq!(rollup(&h, &t, "hour").await, hourly);
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_and_stopped_calls_are_traced_with_codes() {
    let (h, t) = traced().await;
    h.generator.agent.push(Scripted::Fail(AiError::Http {
        status: 400,
        message: "your prompt mentions the secret pelican".into(),
        retry_after: None,
        code: None,
    }));
    let chat = ask(&h, &t, "Hello").await.chat;
    let d = wait_chat(&h, &t, &chat.id, |d| {
        matches!(d.chat.state, ChatState::Failed { .. })
    })
    .await;
    let turn_id = d.items[0].turn_id.clone();
    let (rows, body) = traces(&h, &t, &[("trace_id", turn_id)]).await;
    assert!(!body.contains("pelican"), "{body}");
    let call = rows
        .iter()
        .find(|r| r.kind == TraceKind::ModelCall)
        .unwrap();
    assert!(!call.ok);
    assert_eq!(call.error_code.as_deref(), Some("bad_request"));
    let turn = rows.iter().find(|r| r.kind == TraceKind::Turn).unwrap();
    assert!(!turn.ok && turn.error_code.is_some());

    // Stop while the model streams: the call and the turn are recorded as cancelled.
    h.generator
        .agent
        .push(Scripted::Hang(text_reply(&["Partial"])[..1].to_vec()));
    let chat = ask(&h, &t, "Again").await.chat;
    // Stop once the model streams (its answer item is open), not merely once the turn
    // runs: a Stop before the call starts leaves no model_call row to check.
    wait_chat(&h, &t, &chat.id, |d| {
        running(d)
            && d.items.iter().any(|i| {
                i.status == ItemStatus::InProgress
                    && matches!(i.kind, ChatItemKind::AssistantText { .. })
            })
    })
    .await;
    let r = h
        .post(&t, &format!("/api/chats/{}/stop", chat.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let d = detail(&h, &t, &chat.id).await;
    let turn_id = d.items[0].turn_id.clone();
    let rows = eventually(|| async {
        let (rows, _) = traces(&h, &t, &[("trace_id", turn_id.clone())]).await;
        (rows.iter().any(|r| r.kind == TraceKind::Turn)
            && rows.iter().any(|r| r.kind == TraceKind::ModelCall))
        .then_some(rows)
    })
    .await;
    for r in &rows {
        assert!(!r.ok);
        assert_eq!(r.error_code.as_deref(), Some("cancelled"), "{r:#?}");
    }
}
