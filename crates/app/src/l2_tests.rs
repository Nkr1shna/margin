//! L2 end to end against the fake (`docs/l2-brief.md` D9): publishing a topic, a file
//! of no topic and an agent's document through the real client; what Alice (the owner)
//! and Bob (a member) read and get on their sockets; the refusals; the audit trail; the
//! change feed of agents' documents; and the probe the source viewer makes.

use std::time::Duration;

use futures_util::StreamExt;
use margin_shared::context::AuditKind;
use margin_shared::{Audience, AudienceUnit, Event, ServerFrame, access_code};

use crate::api::Api;
use crate::events::{Frames, connect, next_frame};
use crate::fake::{FakeService, TOKEN, TOKEN_BOB};
use crate::store::LibraryState;

fn aud(access: &[&str], deny: &[&str]) -> Audience {
    Audience {
        access: access.iter().map(|s| s.to_string()).collect(),
        deny: deny.iter().map(|s| s.to_string()).collect(),
    }
}

/// The next event on a socket within two seconds (`None`: nothing came).
async fn next_event(socket: &mut Frames) -> Option<Event> {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match next_frame(socket).await? {
                ServerFrame::Event(event) => return Some(event),
                ServerFrame::Directed(_) => continue,
            }
        }
    })
    .await
    .ok()
    .flatten()
}

/// A socket of `token`'s person, past the service's hello (keep the sender half: the
/// socket lives as long as both halves).
async fn socket(fake: &FakeService, token: &str) -> (crate::events::Sender, Frames) {
    let api = Api::with_token(&fake.url, token);
    let (sender, mut frames) = connect(&api.events_url(), token).await.unwrap().split();
    assert!(matches!(
        next_event(&mut frames).await,
        Some(Event::Hello { .. })
    ));
    (sender, frames)
}

fn ids<T>(rows: &[T], id: impl Fn(&T) -> &str) -> Vec<String> {
    let mut list: Vec<String> = rows.iter().map(|r| id(r).to_string()).collect();
    list.sort();
    list
}

#[tokio::test]
async fn publishing_reaches_new_readers_and_revoking_drops_it_live() {
    let fake = FakeService::start().await;
    let alice = Api::with_token(&fake.url, TOKEN);
    let bob = Api::with_token(&fake.url, TOKEN_BOB);

    // Bob reads what Alice shared with Engineering; not her private subject, not the
    // lesson narrowed by her private file, not that file.
    let lib = bob.library().await.unwrap();
    assert_eq!(
        ids(&lib.topics, |t| &t.id),
        ["complex", "fourier", "signals"]
    );
    let lessons = ids(&lib.lessons, |l| &l.id);
    assert!(lessons.contains(&"l1".to_string()) && !lessons.contains(&"l2".to_string()));
    assert!(!lib.sources.iter().any(|s| s.id == "s8"));
    let mut bob_store = LibraryState::from_library(lib);
    let (_bob_tx, mut bob_socket) = socket(&fake, TOKEN_BOB).await;
    let (_alice_tx, mut alice_socket) = socket(&fake, TOKEN).await;

    // Alice shares Linear Algebra with Engineering: the owner is always kept.
    let stored = alice
        .set_audience(
            AudienceUnit::Topic,
            "linalg",
            &aud(&["company/engineering"], &[]),
        )
        .await
        .unwrap();
    assert_eq!(stored.access, ["person/p-alice", "company/engineering"]);
    // Bob's socket gets the topic (he reads it now), not the audit row (admins only).
    match next_event(&mut bob_socket).await {
        Some(Event::TopicChanged { topic }) => {
            assert_eq!(topic.id, "linalg");
            assert_eq!(topic.access, stored.access);
            bob_store.apply(Event::TopicChanged { topic });
        }
        other => panic!("{other:?}"),
    }
    assert!(bob_store.topics.contains_key("linalg"));
    // Alice's gets the topic and the audit row, named for her.
    assert!(matches!(
        next_event(&mut alice_socket).await,
        Some(Event::TopicChanged { .. })
    ));
    match next_event(&mut alice_socket).await {
        Some(Event::AuditAppended { row }) => {
            assert_eq!(row.kind, AuditKind::AudienceChange);
            assert_eq!(row.actor, "person/p-alice");
            assert_eq!(row.subject.id, "linalg");
            assert_eq!(row.subject_name.as_deref(), Some("Linear Algebra"));
            assert_eq!(
                row.after["access"],
                serde_json::json!(["person/p-alice", "company/engineering"])
            );
        }
        other => panic!("{other:?}"),
    }
    // Bob reads the audience, and can't change it.
    let view = bob.audience(AudienceUnit::Topic, "linalg").await.unwrap();
    assert_eq!(view.audience, stored);
    let e = bob
        .set_audience(AudienceUnit::Topic, "linalg", &Audience::default())
        .await
        .unwrap_err();
    assert_eq!(
        (e.status, e.code.as_str()),
        (Some(403), access_code::NOT_OWNER)
    );

    // Alice denies Bob the subject: his socket drops it, with what follows it.
    alice
        .set_audience(
            AudienceUnit::Topic,
            "signals",
            &aud(&["company/engineering"], &["person/p-bob"]),
        )
        .await
        .unwrap();
    match next_event(&mut bob_socket).await {
        Some(event @ Event::AudienceRevoked { .. }) => {
            assert_eq!(
                event,
                Event::AudienceRevoked {
                    unit: AudienceUnit::Topic,
                    ids: vec!["signals".into()],
                }
            );
            bob_store.apply(event);
        }
        other => panic!("{other:?}"),
    }
    assert!(!bob_store.topics.contains_key("fourier"));
    assert!(!bob_store.lessons.contains_key("l1"));
    assert!(!bob_store.sources.contains_key("s1"));
    assert!(bob_store.topics.contains_key("linalg"));
    let lib = bob.library().await.unwrap();
    assert_eq!(ids(&lib.topics, |t| &t.id), ["linalg"]);
    // What he can't read is not there for him: a 404, so the viewer says so.
    let e = bob
        .audience(AudienceUnit::Topic, "signals")
        .await
        .unwrap_err();
    assert_eq!(e.status, Some(404));
    assert!(crate::ui::audience::source_unreadable(&bob, "s1").await);
    assert!(!crate::ui::audience::source_unreadable(&alice, "s1").await);

    // The audit log: both changes, for admins only.
    let rows = alice
        .audit(Some(AuditKind::AudienceChange), None, None)
        .await
        .unwrap();
    let subjects: Vec<&str> = rows.iter().map(|r| r.subject.id.as_str()).collect();
    assert!(subjects.ends_with(&["linalg", "signals"]), "{subjects:?}");
    assert!(rows.windows(2).all(|w| w[0].at <= w[1].at), "oldest first");
    // Bob's document, private to him: Alice reads only that it's a document of Bob's.
    let triage = rows.iter().find(|r| r.subject.id == "doc-triage").unwrap();
    assert_eq!(triage.subject_name.as_deref(), Some("a document of @bob"));
    let e = bob.audit(None, None, None).await.unwrap_err();
    assert_eq!(
        (e.status, e.code.as_str()),
        (Some(403), access_code::ADMIN_REQUIRED)
    );
    let later = chrono::Utc::now() + chrono::Duration::hours(1);
    let e = alice
        .audit(None, Some(later), Some(chrono::Utc::now()))
        .await
        .unwrap_err();
    assert_eq!((e.status, e.code.as_str()), (Some(400), "bad_range"));
}

#[tokio::test]
async fn the_service_refuses_what_cant_be_named_and_says_what_to_do() {
    let fake = FakeService::start().await;
    let alice = Api::with_token(&fake.url, TOKEN);
    let refused = |access: &[&str]| {
        let alice = alice.clone();
        let body = aud(access, &[]);
        async move {
            alice
                .set_audience(AudienceUnit::Topic, "linalg", &body)
                .await
                .unwrap_err()
        }
    };
    let e = refused(&["company/sales"]).await;
    assert_eq!(
        (e.status, e.code.as_str()),
        (Some(400), access_code::UNKNOWN_GROUP)
    );
    assert!(e.message.contains("Pick one"), "{}", e.message);
    let e = refused(&["person/nobody"]).await;
    assert_eq!(e.code, access_code::UNKNOWN_PERSON);
    // Bob's agent isn't Alice's to name.
    let e = refused(&["agent/a-triage"]).await;
    assert_eq!(e.code, access_code::UNKNOWN_AGENT);
    let e = refused(&["engineering"]).await;
    assert_eq!(e.code, access_code::BAD_ACCESS);
    let many: Vec<String> = (0..=margin_shared::AUDIENCE_MAX)
        .map(|i| format!("person/p{i}"))
        .collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    assert_eq!(refused(&many).await.code, access_code::AUDIENCE_TOO_LARGE);
    // Lessons and notes follow their topic; a topic's file too.
    let http = reqwest::Client::new();
    let response = http
        .put(format!("{}/api/audience/lesson/l1", fake.url))
        .bearer_auth(TOKEN)
        .json(&Audience::default())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let body: margin_shared::ApiError = response.json().await.unwrap();
    assert_eq!(body.code, access_code::NOT_A_UNIT);
    assert!(body.message.contains("change it on the topic"));
    let e = alice
        .set_audience(AudienceUnit::Source, "s1", &Audience::default())
        .await
        .unwrap_err();
    assert_eq!(e.code, access_code::NOT_A_UNIT);
    // Her own agent can be named, in either list.
    let stored = alice
        .set_audience(
            AudienceUnit::Topic,
            "linalg",
            &aud(&["agent/a-claude"], &["person/p-bob"]),
        )
        .await
        .unwrap();
    assert_eq!(stored.access, ["person/p-alice", "agent/a-claude"]);
    assert_eq!(stored.deny, ["person/p-bob"]);
}

#[tokio::test]
async fn a_narrowed_lesson_follows_its_topic_once_its_sources_are_shared_too() {
    let fake = FakeService::start().await;
    let alice = Api::with_token(&fake.url, TOKEN);
    let bob = Api::with_token(&fake.url, TOKEN_BOB);
    // "Computing coefficients" cites Alice's private file: publishing its topic again
    // leaves it hers alone.
    alice
        .set_audience(
            AudienceUnit::Topic,
            "fourier",
            &aud(&["company/engineering"], &[]),
        )
        .await
        .unwrap();
    let l2 = |lib: margin_shared::Library| lib.lessons.into_iter().find(|l| l.id == "l2");
    let lesson = l2(alice.library().await.unwrap()).unwrap();
    assert!(lesson.audience_narrowed);
    assert_eq!(lesson.access, ["person/p-alice"]);
    assert!(l2(bob.library().await.unwrap()).is_none());
    // Shared with Engineering too, the file no longer narrows it.
    alice
        .set_audience(
            AudienceUnit::Source,
            "s8",
            &aud(&["company/engineering"], &[]),
        )
        .await
        .unwrap();
    alice
        .set_audience(
            AudienceUnit::Topic,
            "fourier",
            &aud(&["company/engineering"], &[]),
        )
        .await
        .unwrap();
    let lesson = l2(bob.library().await.unwrap()).expect("Bob reads it now");
    assert!(!lesson.audience_narrowed);
    assert_eq!(lesson.access, ["person/p-alice", "company/engineering"]);
}

#[tokio::test]
async fn agents_documents_are_listed_published_and_deleted_by_their_owner() {
    let fake = FakeService::start().await;
    let alice = Api::with_token(&fake.url, TOKEN);
    let bob = Api::with_token(&fake.url, TOKEN_BOB);
    let names = |docs: Vec<margin_shared::context::Document>| {
        let mut n: Vec<String> = docs.into_iter().map(|d| d.name).collect();
        n.sort();
        n
    };
    assert_eq!(
        names(alice.agent_documents().await.unwrap()),
        ["deploy-runbook.md", "margin-architecture.md"]
    );
    assert_eq!(
        names(bob.agent_documents().await.unwrap()),
        ["deploy-runbook.md", "triage-notes.md"]
    );
    // Alice shares her private one with Engineering: Bob's feed lists it, live too.
    let (_bob_tx, mut bob_socket) = socket(&fake, TOKEN_BOB).await;
    alice
        .set_audience(
            AudienceUnit::Document,
            "doc-arch",
            &aud(&["company/engineering"], &[]),
        )
        .await
        .unwrap();
    match next_event(&mut bob_socket).await {
        Some(Event::DocumentChanged { document }) => assert_eq!(document.id, "doc-arch"),
        other => panic!("{other:?}"),
    }
    assert_eq!(bob.agent_documents().await.unwrap().len(), 3);
    // Only its owner deletes one; then it's gone from the feed with a tombstone.
    let e = bob.delete_document("doc-arch").await.unwrap_err();
    assert_eq!(e.status, Some(403));
    let since = bob.context_changes(0, 500).await.unwrap().generation;
    alice.delete_document("doc-arch").await.unwrap();
    let page = bob.context_changes(since, 500).await.unwrap();
    assert_eq!(page.deleted, ["doc-arch"]);
    assert_eq!(names(bob.agent_documents().await.unwrap()).len(), 2);
    let e = alice.delete_document("doc-triage").await.unwrap_err();
    assert_eq!(
        e.status,
        Some(404),
        "Bob's private one isn't there for Alice"
    );
}

#[tokio::test]
async fn changes_to_access_are_journaled_for_admins() {
    let fake = FakeService::start().await;
    let alice = Api::with_token(&fake.url, TOKEN);
    let (_alice_tx, mut alice_socket) = socket(&fake, TOKEN).await;
    let (_bob_tx, mut bob_socket) = socket(&fake, TOKEN_BOB).await;
    alice
        .create_group(&margin_shared::NewGroup {
            path: "company/design".into(),
            name: "Design".into(),
        })
        .await
        .unwrap();
    alice
        .put_person_groups(
            crate::fake::BOB,
            &["company".into(), "company/design".into()],
        )
        .await
        .unwrap();
    alice.mint_agent_token("a-claude").await.unwrap();
    let kinds: Vec<AuditKind> = alice
        .audit(None, None, None)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.kind)
        .collect();
    assert!(
        kinds.ends_with(&[
            AuditKind::GroupChange,
            AuditKind::MembershipChange,
            AuditKind::AgentToken
        ]),
        "{kinds:?}"
    );
    // Live to admins; never to a member.
    let mut appended = 0;
    while let Some(event) = next_event(&mut alice_socket).await {
        if let Event::AuditAppended { row } = event {
            appended += 1;
            assert!(row.after.to_string().len() < 200, "ids and paths only");
        }
    }
    assert_eq!(appended, 3);
    while let Some(event) = next_event(&mut bob_socket).await {
        assert!(!matches!(event, Event::AuditAppended { .. }), "{event:?}");
    }
}
