//! Per-principal events and ownership (L1, `docs/l1-brief.md` D6, D8): a second person
//! never receives the first person's events, can't list, search, read or change the
//! first person's rows, and an open socket follows changes to its person's access
//! without reconnecting.
//!
//! The second person (`bob`) is written straight into the store with a device token,
//! the way an accepted invitation leaves them, so these tests don't depend on the
//! sign-in flow.

mod common;

use std::time::Duration;

use common::*;
use futures_util::StreamExt;
use margin_agent::Scripted;
use margin_agent::scripted::text_reply;
use margin_service::db;
use margin_service::people::PersonRow;
use margin_service::state::{Audience, DeviceRow, sha256_hex};
use margin_shared::{
    ChatDetail, ChatState, Event, Library, Note, OrgRole, Person, PersonSettings, PersonState,
    Topic,
};
use serde_json::json;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const BOB: &str = "bob";
const BOB_TOKEN: &str = "bob-device-token-0123456789";

async fn signed_in() -> (Harness, String) {
    let h = start().await;
    *h.state.turns.retry.lock().unwrap() = margin_agent::RetryPolicy::immediate();
    let t = h.sign_in("Mac").await;
    h.configure_ai(&t).await;
    (h, t)
}

/// Writes person `bob` (a member, or `role`) and a device of theirs; returns its token.
async fn add_bob(h: &Harness, role: OrgRole) -> String {
    put_bob(h, role).await;
    let device = DeviceRow {
        id: "bob-device".into(),
        name: "Bob's laptop".into(),
        token_hash: sha256_hex(BOB_TOKEN),
        github_login: "bob-dev".into(),
        created_at: chrono::Utc::now(),
        last_seen_at: None,
        person_id: Some(BOB.into()),
    };
    h.state
        .store
        .put(db::DEVICE, &device.id, &device)
        .await
        .unwrap();
    BOB_TOKEN.to_string()
}

async fn put_bob(h: &Harness, role: OrgRole) {
    put_bob_with(h, role, PersonSettings::default()).await;
}

async fn put_bob_with(h: &Harness, role: OrgRole, settings: PersonSettings) {
    let row = PersonRow {
        person: Person {
            id: BOB.into(),
            github_login: "bob-dev".into(),
            github_id: Some(3),
            handle: "bob-dev".into(),
            name: "Bob".into(),
            avatar_url: None,
            org_role: role,
            state: PersonState::Active,
            groups: Vec::new(),
            settings,
            created_at: chrono::Utc::now(),
        },
        login_key: "bob-dev".into(),
    };
    h.state.store.put(db::PERSON, BOB, &row).await.unwrap();
}

/// A marker only Bob receives: every event emitted before it has reached (or been
/// filtered from) his socket by the time it arrives.
fn mark_bob(h: &Harness) {
    h.state.emit_to(
        Audience::Person(BOB.into()),
        Event::AccessChanged {
            person_id: BOB.into(),
        },
    );
}

fn is_mark(e: &Event) -> bool {
    matches!(e, Event::AccessChanged { person_id } if person_id == BOB)
}

/// Whether `e` carries (or names) someone's library or chat content.
fn is_content(e: &Event) -> bool {
    matches!(
        e,
        Event::TopicChanged { .. }
            | Event::TopicDeleted { .. }
            | Event::LessonChanged { .. }
            | Event::LessonDeleted { .. }
            | Event::LessonDelta { .. }
            | Event::NoteChanged { .. }
            | Event::NoteDeleted { .. }
            | Event::SourceChanged { .. }
            | Event::SourceDeleted { .. }
            | Event::VideoChanged { .. }
            | Event::VideoDeleted { .. }
            | Event::ProposalChanged { .. }
            | Event::ChatChanged { .. }
            | Event::ChatItemChanged { .. }
            | Event::ChatItemDelta { .. }
            | Event::ChatDeleted { .. }
            | Event::WorkspaceChanged { .. }
            | Event::WorkspaceDeleted { .. }
            | Event::DocumentChanged { .. }
            | Event::Trace { .. }
    )
}

async fn new_topic(h: &Harness, token: &str, title: &str) -> Topic {
    let r = h
        .post(token, "/api/topics")
        .json(&json!({"title": title}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

async fn new_note(h: &Harness, token: &str, topic_id: &str, title: &str) -> reqwest::Response {
    h.post(token, "/api/notes")
        .json(&json!({"topic_id": topic_id, "title": title, "body_md": "Fourier sines"}))
        .send()
        .await
        .unwrap()
}

async fn library(h: &Harness, token: &str) -> Library {
    let r = h.get(token, "/api/library").send().await.unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_person_never_receives_the_first_persons_events() {
    let (h, owner) = signed_in().await;
    let bob = add_bob(&h, OrgRole::Member).await;
    let mut owner_sock = h.socket(&owner).await;
    let mut bob_sock = h.socket(&bob).await;

    // The owner writes: a topic, a note, an edit, a chat with a turn, then deletes.
    let topic = new_topic(&h, &owner, "Signals").await;
    let r = new_note(&h, &owner, &topic.id, "Fourier").await;
    assert_eq!(r.status(), 200);
    let note: Note = r.json().await.unwrap();
    let r = h
        .patch(&owner, &format!("/api/notes/{}", note.id))
        .json(&json!({"body_md": "Fourier series, edited"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Sines ", "and cosines."])));
    let r = h
        .post(&owner, "/api/chats")
        .json(&json!({"message": {"text": "Teach me Fourier series"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let chat: ChatDetail = r.json().await.unwrap();

    // The owner's socket gets all of it, the streamed answer included.
    let seen = owner_sock
        .until(|e| matches!(e, Event::ChatChanged { chat: c } if c.id == chat.chat.id && c.state == ChatState::Idle && c.usage.model_calls == 1))
        .await;
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::TopicChanged { topic: t } if t.id == topic.id))
    );
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::NoteChanged { note: n } if n.id == note.id))
    );
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::ChatItemChanged { .. }))
    );
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::ChatItemDelta { .. }))
    );

    let r = h
        .delete(&owner, &format!("/api/notes/{}", note.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    owner_sock
        .until(|e| matches!(e, Event::NoteDeleted { id } if *id == note.id))
        .await;
    // Let the background indexing of the note and the chat land too.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Bob's socket, open throughout, got none of it.
    mark_bob(&h);
    let bob_saw = bob_sock.until(is_mark).await;
    let leaked: Vec<&Event> = bob_saw.iter().filter(|e| is_content(e)).collect();
    assert!(
        leaked.is_empty(),
        "Bob received the owner's events: {leaked:?}"
    );
    // The owner never sees Bob's marker.
    h.state.emit_to(
        Audience::Person(BOB.into()),
        Event::AccessChanged {
            person_id: "second".into(),
        },
    );
    let r = new_topic(&h, &owner, "After").await;
    let owner_next = owner_sock
        .until(|e| matches!(e, Event::TopicChanged { topic: t } if t.id == r.id))
        .await;
    assert!(
        !owner_next
            .iter()
            .any(|e| matches!(e, Event::AccessChanged { .. })),
        "{owner_next:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_person_sees_none_of_the_first_persons_rows() {
    let (h, owner) = signed_in().await;
    let bob = add_bob(&h, OrgRole::Member).await;
    let topic = new_topic(&h, &owner, "Signals").await;
    let note: Note = new_note(&h, &owner, &topic.id, "Fourier")
        .await
        .json()
        .await
        .unwrap();
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Hello."])));
    let chat: ChatDetail = h
        .post(&owner, "/api/chats")
        .json(&json!({"message": {"text": "Hello there"}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // Lists: the owner's slice, Bob's (empty) slice.
    let lib = library(&h, &owner).await;
    assert_eq!(lib.topics.len(), 1);
    assert_eq!(lib.notes.len(), 1);
    assert_eq!(
        lib.topics[0].access,
        [format!("person/{}", topic.owner.clone().unwrap())]
    );
    let lib = library(&h, &bob).await;
    assert!(lib.topics.is_empty() && lib.notes.is_empty() && lib.lessons.is_empty());
    let chats: Vec<margin_shared::Chat> = h
        .get(&bob, "/api/chats")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(chats.is_empty());
    let chats: Vec<margin_shared::Chat> = h
        .get(&owner, "/api/chats")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(chats.len(), 1);

    // ⌘K: nothing of the owner's for Bob.
    let hits: Vec<margin_shared::SearchHit> = h
        .get(&bob, "/api/search?q=fourier")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(hits.is_empty(), "{hits:?}");
    let hits: Vec<margin_shared::SearchHit> = h
        .get(&owner, "/api/search?q=fourier")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(hits.iter().any(|h| h.id == note.id));

    // Item routes: 404 outside Bob's access, for reads and writes alike.
    for (method, path) in [
        ("PATCH", format!("/api/notes/{}", note.id)),
        ("DELETE", format!("/api/notes/{}", note.id)),
        ("PATCH", format!("/api/topics/{}", topic.id)),
        ("DELETE", format!("/api/topics/{}", topic.id)),
        ("GET", format!("/api/chats/{}", chat.chat.id)),
        ("PATCH", format!("/api/chats/{}", chat.chat.id)),
        ("DELETE", format!("/api/chats/{}", chat.chat.id)),
        ("POST", format!("/api/chats/{}/stop", chat.chat.id)),
        ("POST", format!("/api/chats/{}/continue", chat.chat.id)),
        ("POST", format!("/api/chats/{}/messages", chat.chat.id)),
    ] {
        let req = match method {
            "GET" => h.get(&bob, &path),
            "PATCH" => h.patch(&bob, &path).json(&json!({"title": "Mine now"})),
            "DELETE" => h.delete(&bob, &path),
            _ => h.post(&bob, &path).json(&json!({"text": "hi"})),
        };
        let r = req.send().await.unwrap();
        assert_eq!(r.status(), 404, "{method} {path}");
    }
    // Filing a note under the owner's topic: the topic doesn't exist for Bob.
    let r = new_note(&h, &bob, &topic.id, "Sneaky").await;
    assert_eq!(r.status(), 404);

    // Bob's own rows are his, and the owner can't see or change them.
    let bobs = new_topic(&h, &bob, "Bob's topic").await;
    assert_eq!(bobs.owner.as_deref(), Some(BOB));
    assert_eq!(bobs.access, ["person/bob"]);
    let r = new_note(&h, &bob, &bobs.id, "Bob's note").await;
    assert_eq!(r.status(), 200);
    let bob_note: Note = r.json().await.unwrap();
    assert_eq!(bob_note.access, ["person/bob"]);
    let lib = library(&h, &owner).await;
    assert!(lib.topics.iter().all(|t| t.id != bobs.id));
    let r = h
        .delete(&owner, &format!("/api/topics/{}", bobs.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_events_and_settings_need_an_admin() {
    let (h, owner) = signed_in().await;
    let bob = add_bob(&h, OrgRole::Member).await;
    let mut owner_sock = h.socket(&owner).await;
    let mut bob_sock = h.socket(&bob).await;

    // Settings: everyone reads them; only an admin changes them.
    let r = h.get(&bob, "/api/settings").send().await.unwrap();
    assert_eq!(r.status(), 200);
    let r = h
        .patch(&bob, "/api/settings")
        .json(&json!({"render_fps": 24}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let err: margin_shared::ApiError = r.json().await.unwrap();
    assert_eq!(err.code, "admin_required");

    // The organisation's models and its ChatGPT sign-in are admin-written too (a
    // member reads the model's state).
    let r = h.get(&bob, "/api/embedding").send().await.unwrap();
    assert_eq!(r.status(), 200);
    for (method, path) in [
        ("POST", "/api/embedding/download"),
        ("DELETE", "/api/embedding"),
        ("POST", "/api/embedding/speech/download"),
        ("DELETE", "/api/embedding/speech"),
        ("GET", "/api/providers/chatgpt"),
        ("POST", "/api/providers/chatgpt/start"),
        ("POST", "/api/providers/chatgpt/complete"),
        ("DELETE", "/api/providers/chatgpt"),
    ] {
        let req = match method {
            "POST" => h.post(&bob, path),
            "DELETE" => h.delete(&bob, path),
            _ => h.get(&bob, path),
        };
        let r = req.send().await.unwrap();
        assert_eq!(r.status(), 403, "{method} {path}");
        let err: margin_shared::ApiError = r.json().await.unwrap();
        assert_eq!(err.code, "admin_required", "{method} {path}");
    }

    // An admins-only event reaches the owner, not the member.
    h.state.emit_to(
        Audience::Admins,
        Event::InvitationDeleted { id: "inv-1".into() },
    );
    owner_sock
        .until(|e| matches!(e, Event::InvitationDeleted { id } if id == "inv-1"))
        .await;
    mark_bob(&h);
    let saw = bob_sock.until(is_mark).await;
    assert!(
        !saw.iter()
            .any(|e| matches!(e, Event::InvitationDeleted { .. })),
        "{saw:?}"
    );

    // Bob becomes an admin: his open socket follows without reconnecting.
    put_bob(&h, OrgRole::Admin).await;
    h.state.expansions.invalidate_all();
    // The socket rebuilds its principal on the change; give it a moment.
    tokio::time::sleep(Duration::from_millis(200)).await;
    h.state.emit_to(
        Audience::Admins,
        Event::InvitationDeleted { id: "inv-2".into() },
    );
    bob_sock
        .until(|e| matches!(e, Event::InvitationDeleted { id } if id == "inv-2"))
        .await;

    // And back to a member: they stop.
    put_bob(&h, OrgRole::Member).await;
    h.state.expansions.invalidate_all();
    tokio::time::sleep(Duration::from_millis(200)).await;
    h.state.emit_to(
        Audience::Admins,
        Event::InvitationDeleted { id: "inv-3".into() },
    );
    mark_bob(&h);
    let saw = bob_sock.until(is_mark).await;
    assert!(
        !saw.iter()
            .any(|e| matches!(e, Event::InvitationDeleted { id } if id == "inv-3")),
        "{saw:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_members_trace_stream_gets_no_traces() {
    let (h, owner) = signed_in().await;
    let bob = add_bob(&h, OrgRole::Member).await;
    let mut req = format!("{}/api/events?traces=1", h.url.replace("http://", "ws://"))
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {bob}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let mut owner_sock = h.socket(&owner).await;

    // The owner's turn writes model_call trace rows.
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Traced."])));
    let r = h
        .post(&owner, "/api/chats")
        .json(&json!({"message": {"text": "Trace me"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    owner_sock
        .until(|e| matches!(e, Event::ChatChanged { chat } if chat.state == ChatState::Idle && chat.usage.model_calls == 1))
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    mark_bob(&h);
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .expect("socket timed out")
            .expect("socket closed")
            .unwrap();
        let WsMessage::Text(text) = msg else { continue };
        let v: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
        assert_ne!(v["type"], "trace", "a member's socket got a trace: {v}");
        assert_ne!(v["type"], "chat_item_changed", "{v}");
        if let Ok(e) = serde_json::from_value::<Event>(v)
            && is_mark(&e)
        {
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_persons_own_provider_and_effort_drive_their_chats() {
    let (h, owner) = signed_in().await;
    let bob = add_bob(&h, OrgRole::Member).await;
    put_bob_with(
        &h,
        OrgRole::Member,
        PersonSettings {
            default_provider: Some(margin_shared::Provider::ChatGpt),
            reasoning_effort: Some(margin_shared::ReasoningEffort::High),
        },
    )
    .await;

    // Bob's chat without a provider takes his default, not the organisation's.
    let r = h
        .post(&bob, "/api/chats")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let chat: ChatDetail = r.json().await.unwrap();
    assert_eq!(chat.chat.provider, margin_shared::Provider::ChatGpt);
    assert_eq!(chat.chat.owner.as_deref(), Some(BOB));

    // Bob's effort goes with his turns; the owner keeps the organisation's.
    put_bob_with(
        &h,
        OrgRole::Member,
        PersonSettings {
            default_provider: None,
            reasoning_effort: Some(margin_shared::ReasoningEffort::High),
        },
    )
    .await;
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Bob's answer."])));
    let r = h
        .post(&bob, "/api/chats")
        .json(&json!({"message": {"text": "Hi from Bob"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let bob_chat: ChatDetail = r.json().await.unwrap();
    assert_eq!(bob_chat.chat.provider, margin_shared::Provider::OpenAi);
    eventually(|| async { (h.generator.agent.requests().len() == 1).then_some(()) }).await;
    let req = &h.generator.agent.requests()[0];
    assert_eq!(
        req.reasoning.map(|r| r.effort),
        Some(margin_shared::ReasoningEffort::High)
    );

    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Owner's answer."])));
    let r = h
        .post(&owner, "/api/chats")
        .json(&json!({"message": {"text": "Hi from the owner"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    eventually(|| async { (h.generator.agent.requests().len() == 2).then_some(()) }).await;
    let req = &h.generator.agent.requests()[1];
    assert_eq!(
        req.reasoning.map(|r| r.effort),
        Some(margin_shared::ReasoningEffort::Medium)
    );
}

#[test]
fn audiences_allow_whom_they_name() {
    use margin_service::principal::{Principal, PrincipalKind};
    let principal = |id: &str, role: OrgRole, paths: &[&str]| Principal {
        kind: PrincipalKind::Person,
        person_id: id.into(),
        id: format!("person/{id}"),
        paths: paths.iter().map(|p| p.to_string()).collect(),
        deny_ids: Vec::new(),
        org_role: role,
        device: None,
        person: Person {
            id: id.into(),
            github_login: id.into(),
            github_id: None,
            handle: id.into(),
            name: id.into(),
            avatar_url: None,
            org_role: role,
            state: PersonState::Active,
            groups: Vec::new(),
            settings: PersonSettings::default(),
            created_at: chrono::Utc::now(),
        },
    };
    let alice = principal("alice", OrgRole::Owner, &["company", "person/alice"]);
    let bob = principal(
        "bob",
        OrgRole::Member,
        &["company", "company/eng", "person/bob"],
    );
    let carol = principal("carol", OrgRole::Admin, &["company", "person/carol"]);

    assert!(Audience::Everyone.allows(&bob));
    assert!(Audience::Admins.allows(&alice) && Audience::Admins.allows(&carol));
    assert!(!Audience::Admins.allows(&bob));
    assert!(Audience::Person("bob".into()).allows(&bob));
    assert!(!Audience::Person("bob".into()).allows(&alice));
    let poa = Audience::PersonOrAdmins("bob".into());
    assert!(poa.allows(&bob) && poa.allows(&carol) && poa.allows(&alice));
    assert!(!Audience::PersonOrAdmins("alice".into()).allows(&bob));
    // Alice's private row: only Alice; not her admins (owner Q3).
    let private = Audience::Paths(vec!["person/alice".into()]);
    assert!(private.allows(&alice));
    assert!(!private.allows(&bob) && !private.allows(&carol));
    // A group's row: its members.
    let eng = Audience::Paths(vec!["company/eng".into()]);
    assert!(eng.allows(&bob) && !eng.allows(&alice));
    // No paths: nobody.
    assert!(!Audience::Paths(Vec::new()).allows(&alice));
}
