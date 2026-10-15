//! Courses and library changes through the agent, end to end with the scripted model
//! (`FakeGenerator.agent`): `propose_course` and its outline card, Create lessons with
//! chips and the chat's record of it, `accept_proposal`, the library write tools, the
//! deletes and their confirmation cards (approve, deny, Stop, from another device), and
//! owner decision Q2 (overwriting the owner's own writing asks; the agent's doesn't;
//! appending never does), superseded outlines, and the lesson and video jobs
//! (`write_lesson`, `regenerate_lesson`, `make_video`) and `attach_sources`.

mod common;

use std::sync::Arc;

use common::*;
use margin_agent::scripted::{text_reply, tool_calls_reply};
use margin_agent::{NoDevices, RetryPolicy, Scripted, ToolContext, ToolKind};
use margin_service::agent_tools;
use margin_service::db;
use margin_service::state::{Author, StoredLesson, StoredNote};
use margin_shared::{
    Card, ChatDetail, ChatItem, ChatItemKind, ChatState, Decision, DeviceRef, Event, ItemStatus,
    Lesson, LessonStatus, Level, Library, Me, Note, Proposal, ProposalStatus, ResultTarget, Source,
    Topic, VideoStatus,
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

async fn me(h: &Harness, token: &str) -> Me {
    h.get(token, "/api/me")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
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

/// A note written by the owner through the API.
async fn owner_note(h: &Harness, token: &str, topic_id: &str, title: &str, body: &str) -> Note {
    let r = h
        .post(token, "/api/notes")
        .json(&json!({"topic_id": topic_id, "lesson_id": null, "title": title, "body_md": body, "quote": null}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

async fn stored_note(h: &Harness, id: &str) -> Option<StoredNote> {
    h.state.store.get(db::NOTE, id).await.unwrap()
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

fn idle(d: &ChatDetail) -> bool {
    d.chat.state == ChatState::Idle
}

fn waiting(d: &ChatDetail) -> bool {
    matches!(d.chat.state, ChatState::WaitingForUser { .. })
}

/// Queues a reply calling `calls` (`(call_id, name, arguments)`), then one answering
/// `answer`.
fn script(h: &Harness, calls: &[(&str, &str, Value)], answer: &str) {
    let calls: Vec<(String, String, String)> = calls
        .iter()
        .map(|(id, name, args)| (id.to_string(), name.to_string(), args.to_string()))
        .collect();
    let refs: Vec<(&str, &str, &str)> = calls
        .iter()
        .map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str()))
        .collect();
    h.generator
        .agent
        .push(Scripted::Events(tool_calls_reply(&refs)));
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&[answer])));
}

/// Starts a chat with `text` (a new chat when `chat_id` is None); returns its id.
async fn say(h: &Harness, token: &str, chat_id: Option<&str>, text: &str) -> String {
    match chat_id {
        None => {
            let r = h
                .post(token, "/api/chats")
                .json(&json!({"message": {"text": text}}))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
            let d: ChatDetail = r.json().await.unwrap();
            d.chat.id
        }
        Some(id) => {
            let r = h
                .post(token, &format!("/api/chats/{id}/messages"))
                .json(&json!({"text": text}))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
            id.to_string()
        }
    }
}

fn line(item: &ChatItem) -> String {
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
            Card::Result(r) => format!("card result: {}", r.title),
            Card::Passages(p) => format!("card passages: {}", p.passages.len()),
            Card::Confirmation(c) => format!("card confirm: {}", c.title),
            Card::WebResults(w) => format!("card web results: {}", w.query),
        },
        ChatItemKind::Action { text } => format!("action: {text}"),
        ChatItemKind::Error { message, .. } => format!("error: {message}"),
    };
    match item.status {
        ItemStatus::Cancelled => format!("{what} (cancelled)"),
        _ => what,
    }
}

fn lines(d: &ChatDetail) -> Vec<String> {
    d.items.iter().map(line).collect()
}

/// The lines of the chat's last turn (from its last user message).
fn last_turn(d: &ChatDetail) -> Vec<String> {
    let start = d
        .items
        .iter()
        .rposition(|i| matches!(i.kind, ChatItemKind::UserMessage { .. }))
        .unwrap();
    d.items[start..].iter().map(line).collect()
}

/// The pending confirmation card the chat waits on.
fn waiting_card(d: &ChatDetail) -> (ChatItem, margin_shared::ConfirmationCard) {
    let ChatState::WaitingForUser { item_id, .. } = &d.chat.state else {
        panic!("not waiting: {:?}", d.chat.state);
    };
    let item = d.items.iter().find(|i| &i.id == item_id).unwrap().clone();
    let ChatItemKind::Card {
        card: Card::Confirmation(card),
        ..
    } = &item.kind
    else {
        panic!("not a confirmation card: {item:?}");
    };
    let card = card.clone();
    (item, card)
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

/// The `function_call_output` the model got for `call_id` in its last request.
fn output_for(h: &Harness, call_id: &str) -> Value {
    let reqs = h.generator.agent.requests();
    let input = &reqs.last().unwrap().input;
    let out = input
        .iter()
        .find(|i| i["type"] == "function_call_output" && i["call_id"] == call_id)
        .unwrap_or_else(|| panic!("no output for {call_id}"));
    serde_json::from_str(out["output"].as_str().unwrap()).unwrap()
}

fn tool_ctx(chat_id: &str) -> ToolContext {
    ToolContext {
        chat_id: chat_id.into(),
        turn_id: "t".into(),
        call_id: "call".into(),
        origin_device: DeviceRef {
            device_id: "d".into(),
            name: "Mac".into(),
        },
        workspaces: Vec::new(),
        devices: Arc::new(NoDevices),
    }
}

async fn wait_lessons_ready(h: &Harness, token: &str, topic_id: &str) -> Vec<Lesson> {
    eventually(|| async {
        let lessons: Vec<Lesson> = library(h, token)
            .await
            .lessons
            .into_iter()
            .filter(|l| l.topic_id == topic_id)
            .collect();
        (!lessons.is_empty() && lessons.iter().all(|l| l.status == LessonStatus::Ready))
            .then_some(lessons)
    })
    .await
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn every_service_tool_is_strict_and_of_its_kind() {
    let h = start().await;
    let registry = agent_tools::registry(&h.state);
    for tool in registry.iter() {
        margin_agent::check_strict_schema(&tool.parameters())
            .unwrap_or_else(|e| panic!("{}: {e}", tool.name()));
        assert!(!tool.description().is_empty(), "{}", tool.name());
    }
    let mut writes = registry.names_of(ToolKind::Write);
    writes.sort();
    assert_eq!(
        writes,
        vec![
            "accept_proposal",
            "append_to_note",
            "attach_sources",
            "create_note",
            "create_topic",
            "edit_lesson_title",
            "make_video",
            "move_topic",
            "propose_course",
            "regenerate_lesson",
            "rename_topic",
            "replace_note_body",
            "write_lesson",
        ]
    );
    assert_eq!(
        registry.names_of(ToolKind::Destructive),
        vec!["delete_topic", "delete_lesson", "delete_note"]
    );
    // The `tools` array is the same on every call (the prompt cache holds).
    assert_eq!(
        registry.definitions(),
        agent_tools::registry(&h.state).definitions()
    );
}

// ---------------------------------------------------------------------------
// propose_course and Create lessons
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_proposed_course_is_an_outline_card_and_create_lessons_tells_the_chat() {
    let (h, t) = signed_in().await;
    let mut req = ask_request("Fourier", 3, false);
    req.level = Level::Advanced;
    req.with_quiz = false;
    let (p, d) = h.propose_in_chat(&t, &req).await.unwrap();
    let chat_id = d.chat.id.clone();
    assert_eq!(
        lines(&d),
        vec![
            "user: A course, please: Fourier",
            "call propose_course",
            "result ok: Proposed “Topic: Fourier” · 3 lessons",
            "card outline",
            "assistant: Here is an outline.",
        ]
    );
    assert_eq!(p.status, ProposalStatus::Ready);
    assert_eq!(p.chat_id.as_deref(), Some(chat_id.as_str()));
    assert_eq!(p.request.prompt, "Fourier");
    assert_eq!(p.request.lesson_count, 3);
    assert_eq!(p.request.level, Level::Advanced);
    assert!(!p.request.with_quiz && !p.request.with_videos);
    let outline = p.outline.clone().unwrap();
    assert_eq!(outline.topic_title, "Topic: Fourier");
    assert_eq!(outline.lessons[2].title, "Lesson 3");
    // The model is told what it proposed and what happens next.
    let out = output_for(&h, "call_propose");
    assert_eq!(out["proposal_id"], p.id.as_str());
    assert_eq!(out["level"], "advanced");
    assert_eq!(out["filed_under"], "Signals & Systems");
    assert!(out["next"].as_str().unwrap().contains("accept_proposal"));
    // Nothing is created before the owner accepts.
    assert!(library(&h, &t).await.topics.is_empty());

    // Create lessons with the chips changed: the lessons follow the chips, and the chat
    // records that the course exists.
    let mut sock = h.socket(&t).await;
    let r = h
        .post(&t, &format!("/api/proposals/{}/accept", p.id))
        .json(&json!({
            "outline": outline,
            "chips": {"level": "intro", "with_videos": true, "with_quiz": true},
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let topic: Topic = r.json().await.unwrap();
    let text = "Created the course “Topic: Fourier” with 3 lessons.";
    sock.until(|e| {
        matches!(e, Event::ChatItemChanged { item } if item.chat_id == chat_id
            && matches!(&item.kind, ChatItemKind::Action { text: t } if t == text))
    })
    .await;
    let d = detail(&h, &t, &chat_id).await;
    assert_eq!(lines(&d).last().unwrap(), &format!("action: {text}"));
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
    assert_eq!(accepted.request.level, Level::Intro);
    assert!(accepted.request.with_quiz && accepted.request.with_videos);
    let lessons = wait_lessons_ready(&h, &t, &topic.id).await;
    assert_eq!(lessons.len(), 3);
    assert!(lessons.iter().all(|l| l.level == Level::Intro));
    assert!(
        h.generator
            .lesson_requests
            .lock()
            .unwrap()
            .iter()
            .all(|ctx| ctx.with_quiz && ctx.level == Level::Intro)
    );
    // with_videos from the chip: each lesson gets a video once written.
    eventually(|| async {
        let lib = library(&h, &t).await;
        (lib.videos.len() == 3).then_some(())
    })
    .await;

    // The next turn's model input carries the record.
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Enjoy it."])));
    say(&h, &t, Some(&chat_id), "Thanks").await;
    wait_chat(&h, &t, &chat_id, idle).await;
    let reqs = h.generator.agent.requests();
    let input = &reqs.last().unwrap().input;
    assert!(
        input
            .iter()
            .any(|i| i["role"] == "developer" && i["content"][0]["text"] == text)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn proposals_are_checked_like_outlines() {
    let (h, t) = signed_in().await;
    let subject = new_topic(&h, &t, None, "Maths").await;

    // Filed under an existing topic: the subject title isn't needed.
    let mut req = ask_request("Laplace", 2, false);
    req.parent_topic_id = Some(subject.id.clone());
    let (p, _) = h.propose_in_chat(&t, &req).await.unwrap();
    assert_eq!(
        p.request.parent_topic_id.as_deref(),
        Some(subject.id.as_str())
    );
    assert_eq!(output_for(&h, "call_propose")["filed_under"], "Maths");

    req.parent_topic_id = Some("gone".into());
    assert_eq!(
        h.propose_in_chat(&t, &req).await.unwrap_err(),
        "That topic doesn't exist."
    );

    // Bad outlines never become proposals.
    let mut bad = propose_args(&ask_request("Waves", 2, false));
    bad["lessons"][1]["title"] = json!("  ");
    h.generator.agent.push(Scripted::Events(tool_calls_reply(&[(
        "call_bad",
        "propose_course",
        &bad.to_string(),
    )])));
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Sorry."])));
    let chat = say(&h, &t, None, "Waves").await;
    let d = wait_chat(&h, &t, &chat, idle).await;
    assert!(
        lines(&d).contains(&"result failed: Every lesson needs a title.".to_string()),
        "{:?}",
        lines(&d)
    );
    assert!(!lines(&d).contains(&"card outline".to_string()));
    let mut no_subject = propose_args(&ask_request("Waves", 2, false));
    no_subject["subject_title"] = json!(" ");
    h.generator.agent.push(Scripted::Events(tool_calls_reply(&[(
        "call_bad2",
        "propose_course",
        &no_subject.to_string(),
    )])));
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Sorry."])));
    say(&h, &t, Some(&chat), "Again").await;
    let d = wait_chat(&h, &t, &chat, idle).await;
    assert!(lines(&d).contains(&"result failed: Give the subject a title.".to_string()));
    let proposals: Vec<Proposal> = h.state.store.list(db::PROPOSAL).await.unwrap();
    assert_eq!(proposals.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_course_whose_subject_was_deleted_goes_under_the_outlines_subject() {
    let (h, t) = signed_in().await;
    let subject = new_topic(&h, &t, None, "Maths").await;
    let mut req = ask_request("Laplace", 2, false);
    req.parent_topic_id = Some(subject.id.clone());
    let p = h.propose(&t, &req).await;
    let r = h
        .delete(&t, &format!("/api/topics/{}", subject.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    // The card shows the subject field; what it says is where the course goes.
    let mut outline = p.outline.unwrap();
    outline.subject_title = "Engineering maths".into();
    let r = h
        .post(&t, &format!("/api/proposals/{}/accept", p.id))
        .json(&json!({"outline": outline}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let course: Topic = r.json().await.unwrap();
    let lib = library(&h, &t).await;
    let parent = lib
        .topics
        .iter()
        .find(|t| Some(&t.id) == course.parent_id.as_ref())
        .unwrap();
    assert_eq!(parent.title, "Engineering maths");
    assert_eq!(parent.parent_id, None);
    let stored: Proposal = h
        .state
        .store
        .get(db::PROPOSAL, &p.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.request.parent_topic_id, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn accept_proposal_creates_the_course_when_the_owner_says_go() {
    let (h, t) = signed_in().await;
    let (p, d) = h
        .propose_in_chat(&t, &ask_request("Fourier", 2, false))
        .await
        .unwrap();
    let chat_id = d.chat.id;

    script(
        &h,
        &[("call_go", "accept_proposal", json!({"proposal_id": p.id}))],
        "The lessons are on their way.",
    );
    say(&h, &t, Some(&chat_id), "Looks good, go ahead").await;
    let d = wait_chat(&h, &t, &chat_id, idle).await;
    let turn = last_turn(&d);
    assert_eq!(
        turn,
        vec![
            "user: Looks good, go ahead",
            "call accept_proposal",
            "result ok: Created “Topic: Fourier” · 2 lessons",
            "card result: Topic: Fourier",
            "assistant: The lessons are on their way.",
        ]
    );
    let out = output_for(&h, "call_go");
    let topic_id = out["topic_id"].as_str().unwrap().to_string();
    assert_eq!(out["lessons"].as_array().unwrap().len(), 2);
    let card = d
        .items
        .iter()
        .find_map(|i| match &i.kind {
            ChatItemKind::Card {
                card: Card::Result(r),
                ..
            } => Some(r.target.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        card,
        ResultTarget::Topic {
            topic_id: topic_id.clone()
        }
    );
    // The tool's result is the chat's record; no separate action.
    assert!(!lines(&d).iter().any(|l| l.starts_with("action:")));
    let lessons = wait_lessons_ready(&h, &t, &topic_id).await;
    assert_eq!(lessons.len(), 2);
    let row: StoredLesson = h
        .state
        .store
        .get(db::LESSON, &lessons[0].id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.author, Author::Agent, "a written lesson is the agent's");

    // Twice: refused. Another chat's proposal: refused.
    script(
        &h,
        &[(
            "call_again",
            "accept_proposal",
            json!({"proposal_id": p.id}),
        )],
        "Already done.",
    );
    say(&h, &t, Some(&chat_id), "Again?").await;
    let d = wait_chat(&h, &t, &chat_id, idle).await;
    assert!(
        last_turn(&d)
            .contains(&"result failed: This outline was already turned into lessons.".to_string())
    );
    let (other, _) = h
        .propose_in_chat(&t, &ask_request("Laplace", 1, false))
        .await
        .unwrap();
    script(
        &h,
        &[(
            "call_other",
            "accept_proposal",
            json!({"proposal_id": other.id}),
        )],
        "Can't.",
    );
    say(&h, &t, Some(&chat_id), "And the Laplace one").await;
    let d = wait_chat(&h, &t, &chat_id, idle).await;
    assert!(last_turn(&d).contains(
        &"result failed: That proposal wasn't made in this chat; propose the course here first."
            .to_string()
    ));
}

// ---------------------------------------------------------------------------
// Library write tools
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn write_tools_change_the_library() {
    let (h, t) = signed_in().await;
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Hi"])));
    let chat = say(&h, &t, None, "hello").await;
    wait_chat(&h, &t, &chat, idle).await;
    let registry = agent_tools::registry(&h.state);
    let ctx = tool_ctx(&chat);
    let run = |name: &'static str, args: Value| {
        let registry = registry.clone();
        let ctx = ctx.clone();
        async move {
            let tool = registry.get(name).unwrap().clone();
            assert_eq!(tool.kind(), ToolKind::Write, "{name}");
            assert_eq!(tool.confirmation(&ctx, &args).await, Ok(None), "{name}");
            let out = tool.run(&ctx, args).await;
            let v: Value = if out.ok {
                serde_json::from_str(&out.output).unwrap()
            } else {
                json!({"error": out.summary})
            };
            (out, v)
        }
    };
    let mut sock = h.socket(&t).await;

    // create_topic: a subject (emitted), then topics under it.
    let (out, v) = run(
        "create_topic",
        json!({"title": "Signals", "parent_id": null}),
    )
    .await;
    assert!(out.ok);
    assert_eq!(out.summary, "Created “Signals”");
    let signals = v["id"].as_str().unwrap().to_string();
    match out.cards.as_slice() {
        [Card::Result(c)] => {
            assert_eq!(
                c.target,
                ResultTarget::Topic {
                    topic_id: signals.clone()
                }
            );
            assert_eq!(c.title, "Signals");
        }
        other => panic!("{other:?}"),
    }
    sock.until(|e| matches!(e, Event::TopicChanged { topic } if topic.id == signals))
        .await;
    sock.close().await;
    let mut ids = Vec::new();
    for title in ["Fourier", "Laplace", "Z transform"] {
        let (out, v) = run(
            "create_topic",
            json!({"title": title, "parent_id": signals}),
        )
        .await;
        assert!(out.ok);
        ids.push(v["id"].as_str().unwrap().to_string());
    }
    let (out, _) = run("create_topic", json!({"title": " ", "parent_id": null})).await;
    assert_eq!(out.summary, "Give the topic a title.");
    let (out, _) = run("create_topic", json!({"title": "x", "parent_id": "gone"})).await;
    assert_eq!(out.summary, "That topic doesn't exist.");

    // rename_topic
    let (out, v) = run(
        "rename_topic",
        json!({"topic_id": ids[2], "title": "Z-transform"}),
    )
    .await;
    assert_eq!(out.summary, "Renamed to “Z-transform”");
    assert_eq!(v["previous_title"], "Z transform");

    // move_topic: reorder among siblings, then out to the top level.
    let (out, v) = run(
        "move_topic",
        json!({"topic_id": ids[2], "parent_id": signals, "position": 0}),
    )
    .await;
    assert_eq!(out.summary, "Moved “Z-transform” under “Signals”");
    assert_eq!(v["path"], "Signals / Z-transform");
    let order = |lib: &Library| {
        let mut kids: Vec<&Topic> = lib
            .topics
            .iter()
            .filter(|t| t.parent_id.as_deref() == Some(signals.as_str()))
            .collect();
        kids.sort_by_key(|t| t.position);
        kids.iter().map(|t| t.title.clone()).collect::<Vec<_>>()
    };
    assert_eq!(
        order(&library(&h, &t).await),
        vec!["Z-transform", "Fourier", "Laplace"]
    );
    let (out, _) = run(
        "move_topic",
        json!({"topic_id": ids[0], "parent_id": null, "position": null}),
    )
    .await;
    assert_eq!(out.summary, "Moved “Fourier” to the top level");
    let lib = library(&h, &t).await;
    assert_eq!(order(&lib), vec!["Z-transform", "Laplace"]);
    let roots: Vec<&str> = {
        let mut r: Vec<&Topic> = lib
            .topics
            .iter()
            .filter(|t| t.parent_id.is_none())
            .collect();
        r.sort_by_key(|t| t.position);
        r.iter().map(|t| t.title.as_str()).collect()
    };
    assert_eq!(roots, vec!["Signals", "Fourier"]);
    let (out, _) = run(
        "move_topic",
        json!({"topic_id": signals, "parent_id": ids[1], "position": null}),
    )
    .await;
    assert_eq!(out.summary, "A topic can't move inside itself.");

    // edit_lesson_title
    let p = h.propose(&t, &ask_request("Waves", 1, false)).await;
    let r = h
        .post(&t, &format!("/api/proposals/{}/accept", p.id))
        .json(&json!({"outline": p.outline.unwrap()}))
        .send()
        .await
        .unwrap();
    let course: Topic = r.json().await.unwrap();
    let lesson = wait_lessons_ready(&h, &t, &course.id).await.remove(0);
    let (out, v) = run(
        "edit_lesson_title",
        json!({"lesson_id": lesson.id, "title": "Standing waves"}),
    )
    .await;
    assert_eq!(out.summary, "Retitled “Standing waves”");
    assert_eq!(v["previous_title"], "Lesson 1");
    let row: StoredLesson = h
        .state
        .store
        .get(db::LESSON, &lesson.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.lesson.title, "Standing waves");
    assert_eq!(row.lesson.body_md, lesson.body_md, "the body is kept");
    assert_eq!(row.author, Author::Agent, "a retitle isn't an overwrite");

    // create_note: the agent's, with a result card.
    let (out, v) = run(
        "create_note",
        json!({"topic_id": course.id, "lesson_id": lesson.id, "title": "Summary", "body_md": "Nodes stay still."}),
    )
    .await;
    assert_eq!(out.summary, "Wrote “Summary”");
    let note_id = v["id"].as_str().unwrap().to_string();
    assert!(matches!(
        out.cards.as_slice(),
        [Card::Result(c)] if c.target == ResultTarget::Note { note_id: note_id.clone() }
    ));
    let row = stored_note(&h, &note_id).await.unwrap();
    assert_eq!(row.author, Author::Agent);
    assert_eq!(row.note.lesson_id.as_deref(), Some(lesson.id.as_str()));
    let (out, _) = run(
        "create_note",
        json!({"topic_id": signals, "lesson_id": lesson.id, "title": "x", "body_md": ""}),
    )
    .await;
    assert_eq!(out.summary, "That lesson belongs to a different topic.");

    // append_to_note keeps what's there.
    let (out, _) = run(
        "append_to_note",
        json!({"note_id": note_id, "text": "Antinodes swing most."}),
    )
    .await;
    assert_eq!(out.summary, "Added to “Summary”");
    let row = stored_note(&h, &note_id).await.unwrap();
    assert_eq!(
        row.note.body_md,
        "Nodes stay still.\n\nAntinodes swing most."
    );
    assert_eq!(row.author, Author::Agent);
    let (out, _) = run("append_to_note", json!({"note_id": note_id, "text": "  "})).await;
    assert_eq!(out.summary, "Give some text to add.");

    // replace_note_body on the agent's own note: no question.
    let (out, _) = run(
        "replace_note_body",
        json!({"note_id": note_id, "body_md": "Rewritten."}),
    )
    .await;
    assert_eq!(out.summary, "Rewrote “Summary”");
    assert_eq!(
        stored_note(&h, &note_id).await.unwrap().note.body_md,
        "Rewritten."
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_cards_and_outputs_land_in_the_chat() {
    let (h, t) = signed_in().await;
    script(
        &h,
        &[
            (
                "call_1",
                "create_topic",
                json!({"title": "Optics", "parent_id": null}),
            ),
            ("call_2", "list_topics", json!({})),
        ],
        "Made it.",
    );
    let chat = say(&h, &t, None, "Make a topic called Optics").await;
    let d = wait_chat(&h, &t, &chat, idle).await;
    // The Read call runs first (so it doesn't see the new topic) but its result is
    // recorded in call order, after the write's result and card.
    assert_eq!(
        lines(&d),
        vec![
            "user: Make a topic called Optics",
            "call create_topic",
            "call list_topics",
            "result ok: Created “Optics”",
            "card result: Optics",
            "result ok: The library is empty",
            "assistant: Made it.",
        ]
    );
    let topic_id = output_for(&h, "call_1")["id"].as_str().unwrap().to_string();
    assert!(
        library(&h, &t)
            .await
            .topics
            .iter()
            .any(|t| t.id == topic_id)
    );
}

// ---------------------------------------------------------------------------
// Owner decision Q2: the owner's writing
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn replacing_the_owners_note_asks_and_appending_never_does() {
    let (h, t) = signed_in().await;
    let phone = h.sign_in("iPhone").await;
    let phone_id = me(&h, &phone).await.device.id;
    let topic = new_topic(&h, &t, None, "Optics").await;
    let mine = owner_note(&h, &t, &topic.id, "Lenses", "My own words about lenses.").await;
    assert_eq!(
        stored_note(&h, &mine.id).await.unwrap().author,
        Author::Owner
    );

    // Appending to the owner's note just happens.
    script(
        &h,
        &[(
            "call_a",
            "append_to_note",
            json!({"note_id": mine.id, "text": "Focal length f."}),
        )],
        "Added.",
    );
    let chat = say(&h, &t, None, "Add the focal length to my note").await;
    let d = wait_chat(&h, &t, &chat, idle).await;
    assert_eq!(
        last_turn(&d),
        vec![
            "user: Add the focal length to my note",
            "call append_to_note",
            "result ok: Added to “Lenses”",
            "assistant: Added.",
        ]
    );
    let row = stored_note(&h, &mine.id).await.unwrap();
    assert_eq!(
        row.note.body_md,
        "My own words about lenses.\n\nFocal length f."
    );
    assert_eq!(row.author, Author::Owner, "appending keeps it the owner's");

    // Replacing it asks; approved from another device, it's replaced and is now the
    // agent's writing.
    script(
        &h,
        &[(
            "call_r",
            "replace_note_body",
            json!({"note_id": mine.id, "body_md": "Tidied."}),
        )],
        "Replaced.",
    );
    say(&h, &t, Some(&chat), "Tidy it up").await;
    let d = wait_chat(&h, &t, &chat, waiting).await;
    let (item, card) = waiting_card(&d);
    assert_eq!(card.tool, "replace_note_body");
    assert_eq!(card.title, "Replace the text of your note “Lenses”?");
    assert_eq!(
        card.detail,
        "You wrote this note. Its text (8 words) would be replaced by the assistant's; this can't be undone."
    );
    assert_eq!(card.approve_label, "Replace");
    assert_eq!(card.decision, Decision::Pending);
    assert_eq!(
        stored_note(&h, &mine.id).await.unwrap().note.body_md,
        "My own words about lenses.\n\nFocal length f.",
        "nothing changes while it waits"
    );
    confirm(&h, &phone, &chat, &item.id, true).await;
    let d = wait_chat(&h, &t, &chat, idle).await;
    assert_eq!(
        last_turn(&d),
        vec![
            "user: Tidy it up",
            "call replace_note_body",
            "card confirm: Replace the text of your note “Lenses”?",
            "result ok: Rewrote “Lenses”",
            "assistant: Replaced.",
        ]
    );
    let answered = d.items.iter().find(|i| i.id == item.id).unwrap();
    assert!(matches!(
        &answered.kind,
        ChatItemKind::Card { card: Card::Confirmation(c), .. }
            if c.decision == Decision::Approved { device_id: phone_id.clone() }
    ));
    let row = stored_note(&h, &mine.id).await.unwrap();
    assert_eq!(row.note.body_md, "Tidied.");
    assert_eq!(row.author, Author::Agent);

    // Now the agent's: replacing again doesn't ask.
    script(
        &h,
        &[(
            "call_r2",
            "replace_note_body",
            json!({"note_id": mine.id, "body_md": "Tidier."}),
        )],
        "Done.",
    );
    say(&h, &t, Some(&chat), "Tidier").await;
    let d = wait_chat(&h, &t, &chat, idle).await;
    assert!(!last_turn(&d).iter().any(|l| l.starts_with("card confirm")));
    assert_eq!(
        stored_note(&h, &mine.id).await.unwrap().note.body_md,
        "Tidier."
    );

    // A retitle by the owner keeps the body the agent's; an edit of the body makes it
    // the owner's again.
    let r = h
        .patch(&t, &format!("/api/notes/{}", mine.id))
        .json(&json!({"title": "Thin lenses", "body_md": "Tidier."}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        stored_note(&h, &mine.id).await.unwrap().author,
        Author::Agent
    );
    let r = h
        .patch(&t, &format!("/api/notes/{}", mine.id))
        .json(&json!({"body_md": "Tidier, with my changes."}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        stored_note(&h, &mine.id).await.unwrap().author,
        Author::Owner
    );

    // Denied: the note stays and the model is told.
    script(
        &h,
        &[(
            "call_r3",
            "replace_note_body",
            json!({"note_id": mine.id, "body_md": "Mine now."}),
        )],
        "Left it.",
    );
    say(&h, &t, Some(&chat), "Rewrite it again").await;
    let d = wait_chat(&h, &t, &chat, waiting).await;
    let (item, card) = waiting_card(&d);
    assert_eq!(card.title, "Replace the text of your note “Thin lenses”?");
    confirm(&h, &t, &chat, &item.id, false).await;
    let d = wait_chat(&h, &t, &chat, idle).await;
    assert!(last_turn(&d).contains(&"result failed: Declined".to_string()));
    assert_eq!(
        output_for(&h, "call_r3")["error"],
        "The owner declined: Replace the text of your note “Thin lenses”?"
    );
    assert_eq!(
        stored_note(&h, &mine.id).await.unwrap().note.body_md,
        "Tidier, with my changes."
    );

    // An empty note of the owner's has no writing to overwrite: filling it doesn't ask.
    let blank = owner_note(&h, &t, &topic.id, "Mirrors", "").await;
    assert_eq!(
        stored_note(&h, &blank.id).await.unwrap().author,
        Author::Owner
    );
    script(
        &h,
        &[(
            "call_r4",
            "replace_note_body",
            json!({"note_id": blank.id, "body_md": "Angle in equals angle out."}),
        )],
        "Filled.",
    );
    say(&h, &t, Some(&chat), "Fill in my mirrors note").await;
    let d = wait_chat(&h, &t, &chat, idle).await;
    assert!(!last_turn(&d).iter().any(|l| l.starts_with("card confirm")));
    assert_eq!(
        stored_note(&h, &blank.id).await.unwrap().note.body_md,
        "Angle in equals angle out."
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rows_from_before_authorship_read_as_the_owners() {
    let (h, t) = signed_in().await;
    let topic = new_topic(&h, &t, None, "Old").await;
    let now = chrono::Utc::now();
    // A note and a lesson as stored before `author` existed.
    let note = json!({
        "id": "n-old", "topic_id": topic.id, "lesson_id": null, "title": "Old note",
        "body_md": "Written long ago.", "quote": null, "created_at": now, "updated_at": now,
    });
    h.state.store.put(db::NOTE, "n-old", &note).await.unwrap();
    let lesson = json!({
        "id": "l-old", "topic_id": topic.id, "title": "Old lesson", "summary": "",
        "position": 0, "level": "intro", "minutes": 5, "body_md": "Old body.",
        "status": {"state": "ready"}, "created_at": now, "updated_at": now, "citations": [],
    });
    h.state
        .store
        .put(db::LESSON, "l-old", &lesson)
        .await
        .unwrap();
    assert_eq!(
        stored_note(&h, "n-old").await.unwrap().author,
        Author::Owner
    );
    let row: StoredLesson = h
        .state
        .store
        .get(db::LESSON, "l-old")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.author, Author::Owner);
    // They still load as library records.
    let lib = library(&h, &t).await;
    assert_eq!(lib.notes[0].id, "n-old");
    assert_eq!(lib.lessons[0].id, "l-old");

    // So replacing the old note asks.
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Hi"])));
    let chat = say(&h, &t, None, "hi").await;
    wait_chat(&h, &t, &chat, idle).await;
    let registry = agent_tools::registry(&h.state);
    let tool = registry.get("replace_note_body").unwrap();
    let prompt = tool
        .confirmation(
            &tool_ctx(&chat),
            &json!({"note_id": "n-old", "body_md": "x"}),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(prompt.title, "Replace the text of your note “Old note”?");
    assert_eq!(
        tool.confirmation(
            &tool_ctx(&chat),
            &json!({"note_id": "gone", "body_md": "x"})
        )
        .await,
        Err("That note doesn't exist.".into())
    );

    // A lesson body edited through the API is the owner's; a lesson job's is the agent's.
    let r = h
        .patch(&t, "/api/lessons/l-old")
        .json(&json!({"title": "Old lesson, renamed"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let p = h.propose(&t, &ask_request("New", 1, false)).await;
    let r = h
        .post(&t, &format!("/api/proposals/{}/accept", p.id))
        .json(&json!({"outline": p.outline.unwrap()}))
        .send()
        .await
        .unwrap();
    let course: Topic = r.json().await.unwrap();
    let written = wait_lessons_ready(&h, &t, &course.id).await.remove(0);
    let author = |id: String| {
        let h = &h;
        async move {
            let row: StoredLesson = h.state.store.get(db::LESSON, &id).await.unwrap().unwrap();
            row.author
        }
    };
    assert_eq!(author(written.id.clone()).await, Author::Agent);
    let r = h
        .patch(&t, &format!("/api/lessons/{}", written.id))
        .json(&json!({"body_md": "My rewrite."}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(author(written.id.clone()).await, Author::Owner);
}

// ---------------------------------------------------------------------------
// Deletes
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_topic_asks_first_and_any_device_approves() {
    let (h, t) = signed_in().await;
    let phone = h.sign_in("iPhone").await;
    let phone_id = me(&h, &phone).await.device.id;
    let subject = new_topic(&h, &t, None, "Signals").await;
    let p = {
        let mut req = ask_request("Fourier", 2, false);
        req.parent_topic_id = Some(subject.id.clone());
        h.propose(&t, &req).await
    };
    let r = h
        .post(&t, &format!("/api/proposals/{}/accept", p.id))
        .json(&json!({"outline": p.outline.unwrap()}))
        .send()
        .await
        .unwrap();
    let course: Topic = r.json().await.unwrap();
    wait_lessons_ready(&h, &t, &course.id).await;
    owner_note(&h, &t, &course.id, "Mine", "My note.").await;
    let upload = h
        .upload(&t, "f.md", Some(&course.id), "Fourier.".to_string())
        .await;
    assert_eq!(upload.status(), 200);

    script(
        &h,
        &[("call_del", "delete_topic", json!({"topic_id": subject.id}))],
        "Deleted.",
    );
    let chat = say(&h, &t, None, "Delete Signals").await;
    let d = wait_chat(&h, &t, &chat, waiting).await;
    let (item, card) = waiting_card(&d);
    assert_eq!(card.tool, "delete_topic");
    assert_eq!(card.title, "Delete the topic “Signals”?");
    assert_eq!(
        card.detail,
        "1 topic under it, 2 lessons, 1 note and 1 source go with it."
    );
    assert_eq!(card.approve_label, "Delete");
    // Nothing is deleted while it waits; the turn lets its slot go.
    assert_eq!(library(&h, &t).await.topics.len(), 2);
    assert_eq!(h.state.turns.slots.available_permits(), 2);

    let mut sock = h.socket(&t).await;
    confirm(&h, &phone, &chat, &item.id, true).await;
    sock.until(|e| matches!(e, Event::TopicDeleted { id } if *id == subject.id))
        .await;
    let d = wait_chat(&h, &t, &chat, idle).await;
    assert_eq!(
        last_turn(&d),
        vec![
            "user: Delete Signals",
            "call delete_topic",
            "card confirm: Delete the topic “Signals”?",
            "result ok: Deleted “Signals”",
            "assistant: Deleted.",
        ]
    );
    let answered = d.items.iter().find(|i| i.id == item.id).unwrap();
    assert!(matches!(
        &answered.kind,
        ChatItemKind::Card { card: Card::Confirmation(c), .. }
            if c.decision == Decision::Approved { device_id: phone_id.clone() }
    ));
    let out = output_for(&h, "call_del");
    assert_eq!(out["with_it"]["lessons"], 2);
    let lib = library(&h, &t).await;
    assert!(lib.topics.is_empty() && lib.lessons.is_empty() && lib.notes.is_empty());
    assert!(lib.sources.is_empty());

    // Deleting what isn't there fails without asking.
    script(
        &h,
        &[("call_gone", "delete_topic", json!({"topic_id": subject.id}))],
        "It's gone already.",
    );
    say(&h, &t, Some(&chat), "Delete it again").await;
    let d = wait_chat(&h, &t, &chat, idle).await;
    assert_eq!(
        last_turn(&d),
        vec![
            "user: Delete it again",
            "call delete_topic",
            "result failed: That topic doesn't exist.",
            "assistant: It's gone already.",
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn denied_and_stopped_deletes_leave_things_as_they_were() {
    let (h, t) = signed_in().await;
    let phone = h.sign_in("iPhone").await;
    let phone_id = me(&h, &phone).await.device.id;
    let p = h.propose(&t, &ask_request("Optics", 1, false)).await;
    let r = h
        .post(&t, &format!("/api/proposals/{}/accept", p.id))
        .json(&json!({"outline": p.outline.unwrap()}))
        .send()
        .await
        .unwrap();
    let course: Topic = r.json().await.unwrap();
    let lesson = wait_lessons_ready(&h, &t, &course.id).await.remove(0);
    let note = owner_note(&h, &t, &course.id, "Lens maker", "1/f = (n-1)(1/R1 - 1/R2)").await;
    let r = h
        .post(&t, "/api/notes")
        .json(&json!({"topic_id": course.id, "lesson_id": lesson.id, "title": "On the lesson", "body_md": "", "quote": null}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // Deny a note's delete.
    script(
        &h,
        &[("call_n", "delete_note", json!({"note_id": note.id}))],
        "Kept it.",
    );
    let chat = say(&h, &t, None, "Delete my lens note").await;
    let d = wait_chat(&h, &t, &chat, waiting).await;
    let (item, card) = waiting_card(&d);
    assert_eq!(card.title, "Delete the note “Lens maker”?");
    assert_eq!(
        card.detail,
        "You wrote it. It begins “1/f = (n-1)(1/R1 - 1/R2)”."
    );
    confirm(&h, &t, &chat, &item.id, false).await;
    let d = wait_chat(&h, &t, &chat, idle).await;
    assert!(last_turn(&d).contains(&"result failed: Declined".to_string()));
    assert_eq!(
        output_for(&h, "call_n")["error"],
        "The owner declined: Delete the note “Lens maker”?"
    );
    assert!(stored_note(&h, &note.id).await.is_some());

    // Stop while a lesson's delete waits: denied by Stop, the lesson stays.
    h.generator.agent.push(Scripted::Events(tool_calls_reply(&[(
        "call_l",
        "delete_lesson",
        &json!({"lesson_id": lesson.id}).to_string(),
    )])));
    say(&h, &t, Some(&chat), "Delete the lesson").await;
    let d = wait_chat(&h, &t, &chat, waiting).await;
    let (item, card) = waiting_card(&d);
    assert_eq!(card.title, "Delete the lesson “Lesson 1”?");
    assert_eq!(card.detail, "1 note goes with it.");
    let r = h
        .post(&phone, &format!("/api/chats/{chat}/stop"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let d = detail(&h, &t, &chat).await;
    assert_eq!(d.chat.state, ChatState::Idle);
    let stopped = d.items.iter().find(|i| i.id == item.id).unwrap();
    assert!(matches!(
        &stopped.kind,
        ChatItemKind::Card { card: Card::Confirmation(c), .. }
            if c.decision == Decision::Denied { device_id: phone_id.clone(), by_stop: true }
    ));
    assert!(last_turn(&d).contains(&"result failed: Cancelled".to_string()));
    // Answering after Stop is refused.
    let r = h
        .post(&t, &format!("/api/chats/{chat}/confirm"))
        .json(&json!({"item_id": item.id, "approve": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
    let lib = library(&h, &t).await;
    assert!(lib.lessons.iter().any(|l| l.id == lesson.id));
    assert_eq!(lib.notes.len(), 2);

    // Approved, the lesson goes with its note.
    script(
        &h,
        &[("call_l2", "delete_lesson", json!({"lesson_id": lesson.id}))],
        "Gone.",
    );
    say(&h, &t, Some(&chat), "Delete the lesson, really").await;
    let d = wait_chat(&h, &t, &chat, waiting).await;
    let (item, _) = waiting_card(&d);
    confirm(&h, &t, &chat, &item.id, true).await;
    let d = wait_chat(&h, &t, &chat, idle).await;
    assert!(last_turn(&d).contains(&"result ok: Deleted “Lesson 1”".to_string()));
    let lib = library(&h, &t).await;
    assert!(lib.lessons.is_empty());
    assert_eq!(lib.notes.len(), 1, "the lesson's note went with it");
}

// ---------------------------------------------------------------------------
// Superseded outlines
// ---------------------------------------------------------------------------

async fn stored_proposal(h: &Harness, id: &str) -> Proposal {
    h.state.store.get(db::PROPOSAL, id).await.unwrap().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_newer_outline_in_a_chat_supersedes_the_earlier_one() {
    let (h, t) = signed_in().await;
    let (first, d) = h
        .propose_in_chat(&t, &ask_request("Fourier", 2, false))
        .await
        .unwrap();
    let chat_id = d.chat.id;
    // Another chat's outline is left alone.
    let (other, _) = h
        .propose_in_chat(&t, &ask_request("Laplace", 1, false))
        .await
        .unwrap();

    let mut sock = h.socket(&t).await;
    script(
        &h,
        &[(
            "call_p2",
            "propose_course",
            propose_args(&ask_request("Fourier, shorter", 1, false)),
        )],
        "Here is a shorter one.",
    );
    say(&h, &t, Some(&chat_id), "Make it shorter").await;
    wait_chat(&h, &t, &chat_id, idle).await;
    let second = output_for(&h, "call_p2")["proposal_id"]
        .as_str()
        .unwrap()
        .to_string();
    let superseded = ProposalStatus::Superseded { by: second.clone() };
    let events = sock
        .until(|e| matches!(e, Event::ProposalChanged { proposal } if proposal.id == second))
        .await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ProposalChanged { proposal }
            if proposal.id == first.id && proposal.status == superseded)),
        "the earlier outline is announced as superseded first: {events:?}"
    );
    assert_eq!(stored_proposal(&h, &first.id).await.status, superseded);
    assert_eq!(
        stored_proposal(&h, &second).await.status,
        ProposalStatus::Ready
    );
    assert_eq!(
        stored_proposal(&h, &other.id).await.status,
        ProposalStatus::Ready
    );

    // Create lessons on the old card is refused.
    let r = h
        .post(&t, &format!("/api/proposals/{}/accept", first.id))
        .json(&json!({"outline": first.outline.clone().unwrap()}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["code"], "superseded");
    assert_eq!(body["message"], "A newer outline replaced this one.");

    // So is accept_proposal, and the model is told which outline replaced it.
    script(
        &h,
        &[(
            "call_old",
            "accept_proposal",
            json!({"proposal_id": first.id}),
        )],
        "That one was replaced.",
    );
    say(&h, &t, Some(&chat_id), "Go with the first one").await;
    wait_chat(&h, &t, &chat_id, idle).await;
    let error = output_for(&h, "call_old")["error"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(error.contains(&second), "{error}");
    assert!(library(&h, &t).await.topics.is_empty());

    // The newest one still goes.
    script(
        &h,
        &[(
            "call_new",
            "accept_proposal",
            json!({"proposal_id": second}),
        )],
        "On their way.",
    );
    say(&h, &t, Some(&chat_id), "Then the short one").await;
    let d = wait_chat(&h, &t, &chat_id, idle).await;
    assert!(
        last_turn(&d).contains(&"result ok: Created “Topic: Fourier, shorter” · 1 lesson".into()),
        "{:?}",
        last_turn(&d)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn accept_proposal_says_where_a_course_went_when_its_topic_was_deleted() {
    let (h, t) = signed_in().await;
    let subject = new_topic(&h, &t, None, "Maths").await;
    let mut req = ask_request("Laplace", 1, false);
    req.parent_topic_id = Some(subject.id.clone());
    let (p, d) = h.propose_in_chat(&t, &req).await.unwrap();
    let r = h
        .delete(&t, &format!("/api/topics/{}", subject.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    script(
        &h,
        &[("call_go", "accept_proposal", json!({"proposal_id": p.id}))],
        "Done.",
    );
    say(&h, &t, Some(&d.chat.id), "Go ahead").await;
    wait_chat(&h, &t, &d.chat.id, idle).await;
    let out = output_for(&h, "call_go");
    let subject_title = p.outline.unwrap().subject_title;
    assert_eq!(
        out["note"],
        format!(
            "The topic it was to go under was deleted; filed under the subject “{subject_title}” instead."
        )
    );

    // Not said when nothing was deleted.
    let (p, d) = h
        .propose_in_chat(&t, &ask_request("Fourier", 1, false))
        .await
        .unwrap();
    script(
        &h,
        &[("call_go2", "accept_proposal", json!({"proposal_id": p.id}))],
        "Done.",
    );
    say(&h, &t, Some(&d.chat.id), "Go ahead").await;
    wait_chat(&h, &t, &d.chat.id, idle).await;
    assert!(output_for(&h, "call_go2").get("note").is_none());
}

// ---------------------------------------------------------------------------
// Lesson and video jobs
// ---------------------------------------------------------------------------

async fn stored_lesson(h: &Harness, id: &str) -> StoredLesson {
    h.state.store.get(db::LESSON, id).await.unwrap().unwrap()
}

/// An accepted course of `n` lessons (from `ask_request(prompt, ..)`) with `chips`,
/// written.
async fn course(h: &Harness, token: &str, prompt: &str, n: u32, chips: Value) -> Topic {
    let p = h.propose(token, &ask_request(prompt, n, false)).await;
    let r = h
        .post(token, &format!("/api/proposals/{}/accept", p.id))
        .json(&json!({"outline": p.outline.unwrap(), "chips": chips}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let topic: Topic = r.json().await.unwrap();
    wait_lessons_ready(h, token, &topic.id).await;
    topic
}

/// The result card's target in the chat's last turn.
fn result_target(d: &ChatDetail) -> ResultTarget {
    let start = d
        .items
        .iter()
        .rposition(|i| matches!(i.kind, ChatItemKind::UserMessage { .. }))
        .unwrap();
    d.items[start..]
        .iter()
        .find_map(|i| match &i.kind {
            ChatItemKind::Card {
                card: Card::Result(r),
                ..
            } => Some(r.target.clone()),
            _ => None,
        })
        .expect("a result card")
}

#[tokio::test(flavor = "multi_thread")]
async fn write_lesson_adds_a_lesson_written_for_the_course() {
    let (h, t) = signed_in().await;
    let topic = course(
        &h,
        &t,
        "Fourier",
        1,
        json!({"with_videos": true, "with_quiz": false}),
    )
    .await;
    // The course's own lesson gets its video.
    eventually(|| async { (library(&h, &t).await.videos.len() == 1).then_some(()) }).await;

    script(
        &h,
        &[(
            "call_w",
            "write_lesson",
            json!({
                "topic_id": topic.id,
                "title": "Gibbs phenomenon",
                "summary": "The overshoot at a jump",
                "minutes": 8,
                "level": "advanced",
                "with_quiz": true,
            }),
        )],
        "It's being written.",
    );
    let chat = say(&h, &t, None, "Add a lesson on the Gibbs phenomenon").await;
    let d = wait_chat(&h, &t, &chat, idle).await;
    assert_eq!(
        last_turn(&d),
        vec![
            "user: Add a lesson on the Gibbs phenomenon",
            "call write_lesson",
            "result ok: Added “Gibbs phenomenon”",
            "card result: Gibbs phenomenon",
            "assistant: It's being written.",
        ]
    );
    let out = output_for(&h, "call_w");
    let id = out["id"].as_str().unwrap().to_string();
    assert_eq!(out["position"], 1);
    assert_eq!(out["level"], "advanced");
    assert_eq!(
        result_target(&d),
        ResultTarget::Lesson {
            lesson_id: id.clone()
        }
    );
    let lessons = wait_lessons_ready(&h, &t, &topic.id).await;
    assert_eq!(lessons.len(), 2);
    let row = stored_lesson(&h, &id).await;
    assert_eq!(row.lesson.position, 1);
    assert_eq!(row.lesson.level, Level::Advanced);
    assert_eq!(row.lesson.summary, "The overshoot at a jump");
    assert_eq!(row.author, Author::Agent);
    assert_eq!(row.proposal_id, None);
    assert_eq!(
        row.prompt.as_deref(),
        Some("Fourier"),
        "the course's request"
    );
    assert!(row.with_quiz);
    // The writer is told the course's request, the tool's quiz option and its place.
    let ctx = h
        .generator
        .lesson_requests
        .lock()
        .unwrap()
        .iter()
        .find(|c| c.outline[c.index].title == "Gibbs phenomenon")
        .cloned()
        .unwrap();
    assert_eq!(ctx.prompt, "Fourier");
    assert!(ctx.with_quiz);
    assert_eq!(ctx.level, Level::Advanced);
    assert_eq!((ctx.index, ctx.outline.len()), (1, 2));
    // No video follows a lesson the agent added.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(library(&h, &t).await.videos.len(), 1);

    // A topic with no course behind it: the lesson's title is its prompt.
    let loose = new_topic(&h, &t, None, "Loose ends").await;
    let registry = agent_tools::registry(&h.state);
    let tool = registry.get("write_lesson").unwrap();
    let args = |topic_id: &str, title: &str| {
        json!({
            "topic_id": topic_id,
            "title": title,
            "summary": "",
            "minutes": 5,
            "level": "intro",
            "with_quiz": false,
        })
    };
    let out = tool
        .run(&tool_ctx(&chat), args(&loose.id, "Dirichlet conditions"))
        .await;
    assert!(out.ok, "{out:?}");
    wait_lessons_ready(&h, &t, &loose.id).await;
    let ctx = h
        .generator
        .lesson_requests
        .lock()
        .unwrap()
        .last()
        .cloned()
        .unwrap();
    assert_eq!(ctx.prompt, "Dirichlet conditions");
    assert!(!ctx.with_quiz);

    let out = tool.run(&tool_ctx(&chat), args("gone", "x")).await;
    assert_eq!(out.summary, "That topic doesn't exist.");
    let out = tool.run(&tool_ctx(&chat), args(&loose.id, "  ")).await;
    assert_eq!(out.summary, "Give the lesson a title.");
}

#[tokio::test(flavor = "multi_thread")]
async fn regenerating_a_lesson_asks_only_when_the_owner_edited_it() {
    let (h, t) = signed_in().await;
    let topic = course(&h, &t, "Optics", 1, json!({})).await;
    let lesson = library(&h, &t)
        .await
        .lessons
        .into_iter()
        .find(|l| l.topic_id == topic.id)
        .unwrap();
    let written = || h.generator.lesson_requests.lock().unwrap().len();
    let before = written();

    // The agent's own lesson is rewritten right away.
    script(
        &h,
        &[(
            "call_g",
            "regenerate_lesson",
            json!({"lesson_id": lesson.id}),
        )],
        "Rewriting it.",
    );
    let chat = say(&h, &t, None, "Rewrite the first lesson").await;
    let d = wait_chat(&h, &t, &chat, idle).await;
    assert_eq!(
        last_turn(&d),
        vec![
            "user: Rewrite the first lesson",
            "call regenerate_lesson",
            "result ok: Rewriting “Lesson 1”",
            "card result: Lesson 1",
            "assistant: Rewriting it.",
        ]
    );
    assert_eq!(
        result_target(&d),
        ResultTarget::Lesson {
            lesson_id: lesson.id.clone()
        }
    );
    eventually(|| async { (written() == before + 1).then_some(()) }).await;
    wait_lessons_ready(&h, &t, &topic.id).await;

    // Edited by the owner: it asks, and a denial keeps the owner's text.
    let r = h
        .patch(&t, &format!("/api/lessons/{}", lesson.id))
        .json(&json!({"body_md": "My rewrite."}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    script(
        &h,
        &[(
            "call_g2",
            "regenerate_lesson",
            json!({"lesson_id": lesson.id}),
        )],
        "Left it.",
    );
    say(&h, &t, Some(&chat), "Rewrite it again").await;
    let d = wait_chat(&h, &t, &chat, waiting).await;
    let (item, card) = waiting_card(&d);
    assert_eq!(card.tool, "regenerate_lesson");
    assert_eq!(card.title, "Rewrite the lesson “Lesson 1”?");
    assert_eq!(
        card.detail,
        "You edited its text; your edits would be lost."
    );
    assert_eq!(card.approve_label, "Rewrite");
    confirm(&h, &t, &chat, &item.id, false).await;
    wait_chat(&h, &t, &chat, idle).await;
    let row = stored_lesson(&h, &lesson.id).await;
    assert_eq!(row.lesson.body_md, "My rewrite.");
    assert_eq!(row.author, Author::Owner);
    assert_eq!(written(), before + 1);

    // Approved: rewritten, and the agent's again.
    script(
        &h,
        &[(
            "call_g3",
            "regenerate_lesson",
            json!({"lesson_id": lesson.id}),
        )],
        "Rewriting it.",
    );
    say(&h, &t, Some(&chat), "Yes, really rewrite it").await;
    let d = wait_chat(&h, &t, &chat, waiting).await;
    let (item, _) = waiting_card(&d);
    confirm(&h, &t, &chat, &item.id, true).await;
    wait_chat(&h, &t, &chat, idle).await;
    let row = eventually(|| async {
        let row = stored_lesson(&h, &lesson.id).await;
        (row.lesson.status == LessonStatus::Ready && row.lesson.body_md != "My rewrite.")
            .then_some(row)
    })
    .await;
    assert_eq!(row.author, Author::Agent);

    // An unknown lesson can't be confirmed.
    let registry = agent_tools::registry(&h.state);
    let tool = registry.get("regenerate_lesson").unwrap();
    assert_eq!(
        tool.confirmation(&tool_ctx(&chat), &json!({"lesson_id": "gone"}))
            .await,
        Err("That lesson doesn't exist.".into())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn make_video_makes_a_ready_lessons_video() {
    let (h, t) = signed_in().await;
    let topic = course(&h, &t, "Optics", 1, json!({"with_videos": false})).await;
    let lesson = library(&h, &t)
        .await
        .lessons
        .into_iter()
        .find(|l| l.topic_id == topic.id)
        .unwrap();
    assert!(library(&h, &t).await.videos.is_empty());

    script(
        &h,
        &[("call_v", "make_video", json!({"lesson_id": lesson.id}))],
        "It's on its way.",
    );
    let chat = say(&h, &t, None, "Make a video of it").await;
    let d = wait_chat(&h, &t, &chat, idle).await;
    assert_eq!(
        last_turn(&d),
        vec![
            "user: Make a video of it",
            "call make_video",
            "result ok: Making the video of “Lesson 1”",
            "card result: Lesson 1",
            "assistant: It's on its way.",
        ]
    );
    assert_eq!(
        result_target(&d),
        ResultTarget::Video {
            lesson_id: lesson.id.clone()
        }
    );
    let out = output_for(&h, "call_v");
    assert_eq!(out["lesson_id"], lesson.id.as_str());
    eventually(|| async {
        let lib = library(&h, &t).await;
        lib.videos
            .iter()
            .any(|v| v.lesson_id == lesson.id && v.status == VideoStatus::Ready)
            .then_some(())
    })
    .await;

    // Only a ready lesson gets one.
    let now = chrono::Utc::now();
    let queued = json!({
        "id": "l-q", "topic_id": topic.id, "title": "Waiting", "summary": "",
        "position": 5, "level": "intro", "minutes": 5, "body_md": "",
        "status": {"state": "queued"}, "created_at": now, "updated_at": now, "citations": [],
    });
    h.state.store.put(db::LESSON, "l-q", &queued).await.unwrap();
    let registry = agent_tools::registry(&h.state);
    let tool = registry.get("make_video").unwrap();
    let out = tool
        .run(&tool_ctx(&chat), json!({"lesson_id": "l-q"}))
        .await;
    assert_eq!(
        out.summary,
        "The lesson is queued; make a video once it is ready."
    );
    let out = tool
        .run(&tool_ctx(&chat), json!({"lesson_id": "gone"}))
        .await;
    assert_eq!(out.summary, "That lesson doesn't exist.");
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_sources_files_the_chats_files_under_a_topic() {
    let (h, t) = signed_in().await;
    let topic = new_topic(&h, &t, None, "Optics").await;
    let elsewhere = new_topic(&h, &t, None, "Elsewhere").await;
    let upload = |name: &'static str, topic_id: Option<String>| {
        let h = &h;
        let t = &t;
        async move {
            let r = h
                .upload(t, name, topic_id.as_deref(), format!("{name} text"))
                .await;
            assert_eq!(r.status(), 200);
            r.json::<Source>().await.unwrap()
        }
    };
    let a = upload("a.md", None).await;
    let b = upload("b.md", None).await;
    let not_in_chat = upload("c.md", None).await;
    let other_topics = upload("d.md", Some(elsewhere.id.clone())).await;

    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Got them."])));
    let r = h
        .post(&t, "/api/chats")
        .json(&json!({"message": {"text": "Here are my files", "source_ids": [a.id, b.id, other_topics.id]}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let chat = r.json::<ChatDetail>().await.unwrap().chat.id;
    wait_chat(&h, &t, &chat, idle).await;

    let registry = agent_tools::registry(&h.state);
    let tool = registry.get("attach_sources").unwrap().clone();
    let run = |ids: Vec<&str>, topic_id: &str| {
        let tool = tool.clone();
        let args = json!({"source_ids": ids, "topic_id": topic_id});
        let ctx = tool_ctx(&chat);
        async move { tool.run(&ctx, args).await }
    };
    let topic_of = |id: String| {
        let h = &h;
        async move {
            let s: Source = h.state.store.get(db::SOURCE, &id).await.unwrap().unwrap();
            s.topic_id
        }
    };

    // Refused as a whole: nothing moves.
    let out = run(vec![&a.id, &not_in_chat.id], &topic.id).await;
    assert_eq!(
        out.summary,
        "“c.md” isn't attached to this chat; only the chat's files can be filed this way."
    );
    let out = run(vec![&a.id, &other_topics.id], &topic.id).await;
    assert_eq!(out.summary, "“d.md” already belongs to another topic.");
    assert_eq!(topic_of(a.id.clone()).await, None);
    let out = run(vec![], &topic.id).await;
    assert_eq!(out.summary, "Give the ids of the chat's files to file.");
    let out = run(vec![&a.id], "gone").await;
    assert_eq!(out.summary, "That topic doesn't exist.");
    let out = run(vec!["nope"], &topic.id).await;
    assert!(
        out.summary.starts_with("There is no source nope."),
        "{out:?}"
    );

    // Filed.
    let mut sock = h.socket(&t).await;
    let out = run(vec![&a.id, &b.id, &a.id], &topic.id).await;
    assert!(out.ok, "{out:?}");
    assert_eq!(out.summary, "Filed 2 files under “Optics”");
    let v: Value = serde_json::from_str(&out.output).unwrap();
    assert_eq!(v["filed"].as_array().unwrap().len(), 2);
    sock.until(|e| {
        matches!(e, Event::SourceChanged { source } if source.id == b.id
            && source.topic_id.as_deref() == Some(topic.id.as_str()))
    })
    .await;
    assert_eq!(topic_of(a.id.clone()).await, Some(topic.id.clone()));
    let out = run(vec![&a.id], &topic.id).await;
    assert_eq!(out.summary, "Already in “Optics”");

    // The topic's sources stay when the chat is deleted.
    let r = h
        .delete(&t, &format!("/api/chats/{chat}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    assert_eq!(topic_of(a.id.clone()).await, Some(topic.id.clone()));
    assert_eq!(topic_of(b.id.clone()).await, Some(topic.id.clone()));
}

// ---------------------------------------------------------------------------
// R1: lessons retrieve with the planner's questions, packed to the budget
// ---------------------------------------------------------------------------

mod lesson_retrieval {
    use super::*;
    use chrono::Utc;
    use margin_embed::{Embedder, Purpose, fake::FakeEmbedder};
    use margin_service::ask::{CoursePlan, PlannedLesson, StoredProposal};
    use margin_service::db::PassageRow;
    use margin_shared::{AskRequest, Outline, OutlineLesson};

    fn passage(id: &str, scope: &str, ord: i64, text: &str) -> PassageRow {
        PassageRow {
            id: id.into(),
            project_id: "my-library".into(),
            document_id: "notes".into(),
            origin: "agent".into(),
            scope_ids: vec![scope.into()],
            access: vec!["company".into()],
            unit_id: format!("notes:{id}"),
            modality: "text".into(),
            ord,
            page: None,
            start_line: Some(ord * 5 + 1),
            end_line: Some(ord * 5 + 4),
            start_ms: None,
            end_ms: None,
            section_path: "Signals".into(),
            text: text.into(),
            file: None,
            chunk_hash: format!("h-{id}"),
            // What the test embedder stamps (the dense leg compares only its vectors).
            embedder: margin_service::ingest::EMBEDDER_VERSION.into(),
            vector: FakeEmbedder
                .embed(&[text.to_string()], Purpose::DOCUMENT)
                .unwrap()
                .remove(0),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lessons_use_the_plans_questions_and_the_budget() {
        let (h, t) = signed_in().await;
        let topic = new_topic(&h, &t, None, "Signals").await;
        // Passages far apart (no merging), each about one thing.
        let texts = [
            "Gibbs ringing overshoots near a jump discontinuity by about nine percent.",
            "Aliasing folds frequencies above Nyquist back into the band.",
            "Convolution in time is multiplication in frequency.",
            "Window functions trade main lobe width against side lobes.",
        ];
        let rows: Vec<PassageRow> = texts
            .iter()
            .enumerate()
            .map(|(i, text)| passage(&format!("p{i}"), &topic.id, i as i64 * 10, text))
            .collect();
        h.state.store.insert_many(db::PASSAGE, &rows).await.unwrap();

        // A stored plan whose question is the only way to the Gibbs passage.
        let now = Utc::now();
        let request = AskRequest {
            prompt: "signals".into(),
            lesson_count: 1,
            level: Level::Intro,
            with_videos: false,
            with_quiz: false,
            parent_topic_id: None,
            source_ids: vec![],
        };
        let outline = Outline {
            topic_title: "Signals".into(),
            subject_title: "S".into(),
            lessons: vec![OutlineLesson {
                title: "Lesson one".into(),
                summary: "Overview".into(),
                minutes: 5,
            }],
        };
        let proposal = Proposal {
            id: "prop".into(),
            request,
            status: ProposalStatus::Accepted {
                topic_id: topic.id.clone(),
            },
            outline: Some(outline),
            created_at: now,
            chat_id: None,
            written_with: None,
        };
        let stored = StoredProposal {
            proposal,
            plan: Some(CoursePlan {
                topic_title: "Signals".into(),
                subject_title: "S".into(),
                lessons: vec![PlannedLesson {
                    title: "Lesson one".into(),
                    summary: "Overview".into(),
                    minutes: 5,
                    queries: vec!["Gibbs ringing overshoot near a jump".into()],
                    must_cite: vec![],
                }],
            }),
            pipeline_hash: None,
            workspace_ids: Vec::new(),
        };
        h.state
            .store
            .put(db::PROPOSAL, "prop", &stored)
            .await
            .unwrap();
        // The stored row still reads as a plain proposal.
        let plain: Proposal = h
            .get(&t, "/api/proposals/prop")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(plain.id, "prop");
        let lesson = StoredLesson {
            lesson: Lesson {
                citations: Vec::new(),
                id: "l1".into(),
                topic_id: topic.id.clone(),
                title: "Lesson one".into(),
                summary: "Overview".into(),
                position: 0,
                level: Level::Intro,
                minutes: 5,
                body_md: String::new(),
                status: LessonStatus::Ready,
                created_at: now,
                updated_at: now,
                written_with: None,
            },
            proposal_id: Some("prop".into()),
            previous_body: None,
            previous_citations: None,
            author: Author::Agent,
            prompt: None,
            with_quiz: false,
            workspace_ids: Vec::new(),
        };
        h.state.store.put(db::LESSON, "l1", &lesson).await.unwrap();
        *h.generator.lesson_body.lock().unwrap() = Some("Ringing.[^1]".into());

        margin_service::ask::regenerate_lesson(&h.state, "l1")
            .await
            .unwrap();
        let ctx =
            eventually(|| async { h.generator.lesson_requests.lock().unwrap().first().cloned() })
                .await;
        assert!(
            ctx.passages
                .iter()
                .any(|p| p.text.starts_with("Gibbs ringing")),
            "{:?}",
            ctx.passages
        );
        let ready = wait_lessons_ready(&h, &t, &topic.id).await;
        let cited = &ready[0].citations;
        assert_eq!(cited.len(), 1);
        assert_eq!(cited[0].document_id.as_deref(), Some("notes"));
        assert!(cited[0].start_line.is_some());

        // A window that fits about one passage: the budget, not a fixed k, decides.
        let one = texts[0].len().div_ceil(3) * 2 + 4;
        let r = h
            .patch(&t, "/api/settings")
            .json(
                &json!({"pipeline": {"packer": {"name": "mmr-v1", "params": {"window_tokens": one}},
                                        "budget": {"min_score_pct": 0}}}),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        regenerate(&h).await;
        let ctx = eventually(|| async {
            let r = h.generator.lesson_requests.lock().unwrap();
            (r.len() == 2).then(|| r[1].clone())
        })
        .await;
        assert_eq!(ctx.passages.len(), 1, "{:?}", ctx.passages);
        // With the default window every passage fits.
        let r = h
            .patch(&t, "/api/settings")
            .json(&json!({"pipeline": {"budget": {"min_score_pct": 0}}}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        regenerate(&h).await;
        let ctx = eventually(|| async {
            let r = h.generator.lesson_requests.lock().unwrap();
            (r.len() == 3).then(|| r[2].clone())
        })
        .await;
        assert!(ctx.passages.len() >= 2, "{:?}", ctx.passages);
    }

    /// Regenerates l1 once its last job has let go of it.
    async fn regenerate(h: &Harness) {
        eventually(|| async {
            margin_service::ask::regenerate_lesson(&h.state, "l1")
                .await
                .ok()
        })
        .await;
        eventually(|| async {
            let l: StoredLesson = h.state.store.get(db::LESSON, "l1").await.unwrap().unwrap();
            (l.lesson.status == LessonStatus::Ready).then_some(())
        })
        .await;
    }

    #[test]
    fn plans_are_checked_against_the_draft() {
        let lesson = |title: &str, queries: &[&str]| PlannedLesson {
            title: title.into(),
            summary: " s ".into(),
            minutes: 5,
            queries: queries.iter().map(|q| q.to_string()).collect(),
            must_cite: vec![],
        };
        let plan = CoursePlan {
            topic_title: "T".into(),
            subject_title: "S".into(),
            lessons: vec![
                lesson(" A ", &["q", " Q ", "", "r", "s", "t"]),
                lesson("B", &[]),
            ],
        };
        let req = |lesson_count| margin_ai::PlanRequest {
            prompt: "p".into(),
            level: Level::Intro,
            lesson_count,
            with_quiz: false,
            draft: None,
            contents: Vec::new(),
            survey: Vec::new(),
        };
        assert!(plan.clone().checked(&req(3)).is_err());
        let checked = plan.checked(&req(2)).unwrap();
        assert_eq!(checked.lessons[0].title, "A");
        assert_eq!(checked.lessons[0].queries, ["q", "r", "s"]);
        assert_eq!(checked.lessons[1].queries, ["B: s"]);
        assert_eq!(checked.outline().lessons[1].title, "B");
    }
}

// ---------------------------------------------------------------------------
// R2: the listwise rerank, query rewriting, and what wrote a lesson
// (`docs/r2-brief.md` D2, D3, D4.3)
// ---------------------------------------------------------------------------

mod r2 {
    use std::time::Duration;

    use super::*;
    use margin_ai::Ranked;
    use margin_service::pipeline::{Pipeline, Registry, StageDeps};
    use margin_service::retrieval::{self, QueryPlan, Request};
    use margin_service::small::StateModels;
    use margin_shared::context::{Scope, TraceKind, TraceRow};
    use margin_shared::pipeline::{PipelineConfig, RetrievalPurpose, Route, StageCfg};
    use margin_shared::{Provider, SourceStatus};

    const NOTES: &str = "Fourier series write a periodic signal as a sum of sines.\n\n\
Gibbs ringing overshoots near a jump discontinuity.\n\n\
The coefficients come from integrals over one period.\n\n\
Photosynthesis stores light energy as sugar.";

    /// Installs `cfg` as the service's pipeline, built from a registry whose
    /// model-backed stages call the service's small-tasks models (`StateModels`, bound
    /// to this service), as `server.rs` does once it passes them as `StageDeps.models`.
    fn install(h: &Harness, cfg: PipelineConfig) {
        let models = Arc::new(StateModels::default());
        models.bind(Arc::downgrade(&h.state));
        let mut deps = StageDeps::new(h.state.store.clone());
        deps.models = Some(models);
        let reg = Registry::standard(deps);
        let cfg = cfg.with_embedder(h.state.model_files.backend.name());
        h.state
            .pipeline
            .store(Arc::new(Pipeline::build(&cfg, &reg).unwrap()));
    }

    fn stage(name: &str, params: Value) -> StageCfg {
        StageCfg {
            name: name.into(),
            params: params.as_object().cloned().unwrap_or_default(),
        }
    }

    /// A topic with `NOTES` read into passages; their ids in order.
    async fn topic_with_notes(h: &Harness, t: &str) -> (Topic, Vec<String>) {
        let topic = new_topic(h, t, None, "Signals").await;
        let r = h
            .upload(t, "notes/signals.txt", Some(&topic.id), NOTES)
            .await;
        assert_eq!(r.status(), 200);
        let source: Source = r.json().await.unwrap();
        eventually(|| async {
            library(h, t)
                .await
                .sources
                .iter()
                .any(|s| s.id == source.id && matches!(s.status, SourceStatus::Ready { .. }))
                .then_some(())
        })
        .await;
        let mut rows = h.state.store.passages_of(&source.id).await.unwrap();
        rows.sort_by_key(|r| r.ord);
        assert_eq!(rows.len(), 4, "{rows:?}");
        (topic, rows.into_iter().map(|r| r.id).collect())
    }

    async fn request(
        h: &Harness,
        topic: &Topic,
        plan: QueryPlan,
        purpose: RetrievalPurpose,
        trace_id: &str,
    ) -> Request {
        let pipeline = h.state.pipeline.load_full();
        let window = retrieval::route_window(&h.state, Route::Write, None)
            .await
            .unwrap();
        Request {
            plan,
            scope: Scope::Topic {
                id: topic.id.clone(),
            },
            access: None,
            modalities: Vec::new(),
            k: retrieval::fused_k(&pipeline.config),
            budget: pipeline.config.budget,
            window_tokens: window.tokens,
            reserve_output: window.reserve_output,
            trace_id: trace_id.into(),
            caller: None,
            purpose,
            provider: None,
        }
    }

    async fn rows(h: &Harness, trace_id: &str) -> Vec<TraceRow> {
        assert!(h.state.tracer.flush(Duration::from_secs(5)).await);
        margin_service::trace::read_rows(
            &h.state.store,
            None,
            chrono::DateTime::UNIX_EPOCH,
            chrono::Utc::now() + chrono::Duration::minutes(1),
            Some(trace_id.into()),
        )
        .await
        .unwrap()
    }

    fn packed_ids(packed: &[margin_service::pipeline::Packed]) -> Vec<String> {
        packed
            .iter()
            .flat_map(|p| p.hits.iter().map(|h| h.passage.id.clone()))
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lesson_records_what_wrote_it_and_regenerating_overwrites_it() {
        let (h, t) = signed_in().await;
        let topic = course(
            &h,
            &t,
            "Fourier",
            1,
            json!({"with_videos": false, "with_quiz": false}),
        )
        .await;
        let lib = library(&h, &t).await;
        let lesson = lib
            .lessons
            .iter()
            .find(|l| l.topic_id == topic.id)
            .unwrap()
            .clone();
        let first = lesson.written_with.clone().expect("written_with");
        assert_eq!(first.prompt_version, "lesson-v2");
        assert_eq!(first.model, "test-model");
        assert_eq!(first.provider, Provider::OpenAi);
        assert_eq!(first.pipeline_hash, h.state.pipeline.load().hash);
        // The trace rows of its job carry the same pipeline hash.
        let calls: Vec<TraceRow> = rows(&h, &lesson.id)
            .await
            .into_iter()
            .filter(|r| r.kind == TraceKind::ModelCall)
            .collect();
        assert!(
            calls
                .iter()
                .all(|r| r.pipeline_hash.as_deref() == Some(first.pipeline_hash.as_str()))
        );

        let r = h
            .post(&t, &format!("/api/lessons/{}/regenerate", lesson.id))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let again = eventually(|| async {
            let l = stored_lesson(&h, &lesson.id).await.lesson;
            (l.status == LessonStatus::Ready
                && l.written_with.as_ref().is_some_and(|w| w.at > first.at))
            .then_some(l)
        })
        .await;
        assert_eq!(
            again.written_with.unwrap().prompt_version,
            first.prompt_version
        );

        // A failed rewrite keeps the body and what wrote it.
        h.generator
            .fail_lessons
            .lock()
            .unwrap()
            .insert(lesson.title.clone());
        let before = stored_lesson(&h, &lesson.id).await.lesson;
        let r = h
            .post(&t, &format!("/api/lessons/{}/regenerate", lesson.id))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let failed = eventually(|| async {
            let l = stored_lesson(&h, &lesson.id).await.lesson;
            matches!(l.status, LessonStatus::Failed { .. }).then_some(l)
        })
        .await;
        assert_eq!(failed.body_md, before.body_md);
        assert_eq!(failed.written_with, before.written_with);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lesson_retrieval_is_reranked_on_the_small_route_traced_and_cached() {
        let (h, t) = signed_in().await;
        let (topic, ids) = topic_with_notes(&h, &t).await;
        install(
            &h,
            PipelineConfig {
                reranker: StageCfg::named("listwise-small"),
                ..PipelineConfig::default()
            },
        );
        // The last paragraph first; the first one irrelevant.
        let grades = [(&ids[3], 3), (&ids[1], 2), (&ids[2], 1), (&ids[0], 0)];
        *h.generator.rerank_reply.lock().unwrap() = Some(
            grades
                .iter()
                .map(|(id, grade)| Ranked {
                    id: id.to_string(),
                    grade: *grade,
                })
                .collect(),
        );
        let plan = QueryPlan::lesson("Fourier series", "Sums of sines", &["gibbs".into()], None);
        let req = request(&h, &topic, plan, RetrievalPurpose::Lesson, "lesson-1").await;
        let packed = retrieval::retrieve(&h.state, &req).await.unwrap();
        let got = packed_ids(&packed);
        assert_eq!(got.first(), Some(&ids[3]), "{got:?}");
        assert!(!got.contains(&ids[0]), "grade 0 dropped: {got:?}");

        let calls = h.generator.rerank_requests.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].opts.route, Route::Small);
        assert_eq!(calls[0].opts.prompt_version.as_deref(), Some("rerank-v1"));
        assert_eq!(calls[0].choice.model, "test-model");
        assert!(
            calls[0]
                .query
                .starts_with("Fourier series Sums of sines\nAlso asked: "),
            "{}",
            calls[0].query
        );
        assert!(
            calls[0]
                .candidates
                .iter()
                .all(|c| c.label.contains("signals.txt")),
            "{:?}",
            calls[0].candidates
        );

        let trace = rows(&h, "lesson-1").await;
        let retrieval_row = trace
            .iter()
            .find(|r| r.kind == TraceKind::Retrieval)
            .unwrap();
        assert_eq!(retrieval_row.reranker.as_deref(), Some("listwise-small"));
        assert_eq!(retrieval_row.reranked, Some(4));
        assert_eq!(retrieval_row.error_code, None);
        assert!(retrieval_row.legs.as_ref().unwrap().rerank_ms > 0.0);
        let call = trace
            .iter()
            .find(|r| r.kind == TraceKind::ModelCall)
            .unwrap();
        assert_eq!(call.route, Some(Route::Small));
        assert_eq!(call.prompt_version.as_deref(), Some("rerank-v1"));
        assert_eq!(call.job_id.as_deref(), Some("lesson-1"));
        assert_eq!(call.parent_id.as_deref(), Some(retrieval_row.id.as_str()));
        assert!(call.ok);

        // The same retrieval again: a cache hit, no second rerank.
        let again = retrieval::retrieve(&h.state, &req).await.unwrap();
        assert_eq!(packed_ids(&again), got);
        assert_eq!(h.generator.rerank_requests.lock().unwrap().len(), 1);
        let trace = rows(&h, "lesson-1").await;
        assert!(trace.iter().any(|r| r.cache_hit == Some(true)));

        // A rerank that fails keeps the fused order, traced as rerank_failed.
        *h.generator.rerank_reply.lock().unwrap() = None;
        let req = request(
            &h,
            &topic,
            QueryPlan::single("coefficients integrals"),
            RetrievalPurpose::Lesson,
            "lesson-2",
        )
        .await;
        let packed = retrieval::retrieve(&h.state, &req).await.unwrap();
        assert!(!packed.is_empty());
        let trace = rows(&h, "lesson-2").await;
        let retrieval_row = trace
            .iter()
            .find(|r| r.kind == TraceKind::Retrieval)
            .unwrap();
        assert_eq!(retrieval_row.error_code.as_deref(), Some("rerank_failed"));
        let call = trace
            .iter()
            .find(|r| r.kind == TraceKind::ModelCall)
            .unwrap();
        assert!(!call.ok);

        // The agent's searches aren't reranked by default.
        let before = h.generator.rerank_requests.lock().unwrap().len();
        let req = request(
            &h,
            &topic,
            QueryPlan::single("gibbs"),
            RetrievalPurpose::Agent,
            "turn-1",
        )
        .await;
        retrieval::retrieve(&h.state, &req).await.unwrap();
        assert_eq!(h.generator.rerank_requests.lock().unwrap().len(), before);
        let trace = rows(&h, "turn-1").await;
        let retrieval_row = trace
            .iter()
            .find(|r| r.kind == TraceKind::Retrieval)
            .unwrap();
        assert_eq!(retrieval_row.reranked, None);
        assert!(trace.iter().all(|r| r.kind != TraceKind::ModelCall));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_short_query_is_rewritten_once_and_the_answer_reused() {
        let (h, t) = signed_in().await;
        let (topic, _ids) = topic_with_notes(&h, &t).await;
        install(
            &h,
            PipelineConfig {
                rewriter: stage("small-expand-v1", json!({})),
                ..PipelineConfig::default()
            },
        );
        *h.generator.rewrite_reply.lock().unwrap() = Some(vec![
            "overshoot near a jump".into(),
            "ringing artefacts".into(),
        ]);
        let req = request(
            &h,
            &topic,
            QueryPlan::single("gibbs"),
            RetrievalPurpose::Agent,
            "turn-1",
        )
        .await;
        retrieval::retrieve(&h.state, &req).await.unwrap();
        let calls = h.generator.rewrite_requests.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input.query, "gibbs");
        assert_eq!(calls[0].input.purpose, RetrievalPurpose::Agent);
        assert_eq!(calls[0].input.scope_names.len(), 1);
        assert!(
            calls[0].input.scope_names[0].contains("signals.txt"),
            "{:?}",
            calls[0].input.scope_names
        );
        assert_eq!(calls[0].opts.route, Route::Small);
        assert_eq!(calls[0].opts.prompt_version.as_deref(), Some("rewrite-v1"));
        let trace = rows(&h, "turn-1").await;
        let retrieval_row = trace
            .iter()
            .find(|r| r.kind == TraceKind::Retrieval)
            .unwrap();
        assert_eq!(retrieval_row.queries, Some(3));
        let call = trace
            .iter()
            .find(|r| r.kind == TraceKind::ModelCall)
            .unwrap();
        assert_eq!(call.prompt_version.as_deref(), Some("rewrite-v1"));
        assert_eq!(call.parent_id.as_deref(), Some(retrieval_row.id.as_str()));

        // Another retrieval of the same query (another k, so not the retrieval cache):
        // the rewrite is reused.
        let mut other = req.clone();
        other.k = 3;
        other.trace_id = "turn-2".into();
        retrieval::retrieve(&h.state, &other).await.unwrap();
        assert_eq!(h.generator.rewrite_requests.lock().unwrap().len(), 1);
        let trace = rows(&h, "turn-2").await;
        let retrieval_row = trace
            .iter()
            .find(|r| r.kind == TraceKind::Retrieval)
            .unwrap();
        assert_eq!(retrieval_row.queries, Some(3));

        // Five words or more: no call. A failed rewrite keeps the plan.
        let req = request(
            &h,
            &topic,
            QueryPlan::single("how do the fourier coefficients come"),
            RetrievalPurpose::Agent,
            "turn-3",
        )
        .await;
        retrieval::retrieve(&h.state, &req).await.unwrap();
        assert_eq!(h.generator.rewrite_requests.lock().unwrap().len(), 1);
        *h.generator.rewrite_reply.lock().unwrap() = None;
        let req = request(
            &h,
            &topic,
            QueryPlan::single("sugar"),
            RetrievalPurpose::Agent,
            "turn-4",
        )
        .await;
        let packed = retrieval::retrieve(&h.state, &req).await.unwrap();
        assert!(!packed.is_empty());
        let trace = rows(&h, "turn-4").await;
        let retrieval_row = trace
            .iter()
            .find(|r| r.kind == TraceKind::Retrieval)
            .unwrap();
        assert_eq!(retrieval_row.queries, Some(1));
    }
}
