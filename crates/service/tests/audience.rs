//! Audiences (L2-write, `docs/l2-brief.md` D2, D3, D4) through the HTTP API and the
//! service's own functions: publishing a topic cascades to its rows, documents and
//! passages; refusals; agent documents; derived lesson audiences; the note refusal;
//! the `passages-access-v1` migration.

mod common;

use std::time::Instant;

use chrono::Utc;
use common::*;
use margin_embed::{Embedder, Purpose, fake::FakeEmbedder};
use margin_service::audience;
use margin_service::chats::StoredChatItem;
use margin_service::db::{self, PassageRow};
use margin_service::people::PersonRow;
use margin_service::state::{DeviceRow, StoredLesson, sha256_hex};
use margin_shared::context::{ContextIngest, Document, LibraryKind, Origin};
use margin_shared::{
    ApiError, Audience, AudienceUnit, AudienceView, Card, ChatItem, ChatItemKind, Citation, Event,
    ItemStatus, Lesson, LessonStatus, Level, Library, Note, OrgRole, PassagesCard, Person,
    PersonSettings, PersonState, Source, SourceKind, SourceStatus, Topic,
};
use serde_json::json;

const ENGINEERING: &str = "company/engineering";
const SALES: &str = "company/sales";

const HEAT: &str = "# Heat engines\n\nA Carnot engine runs between two reservoirs.\n\n\
                    Its efficiency is one minus the cold temperature over the hot.\n";
const SECRET: &str = "# Pricing\n\nThe discount ladder stays confidential.\n";

/// A person with `role` and a signed-in device; returns the device token.
async fn seed_person(h: &Harness, id: &str, role: OrgRole) -> String {
    let person = PersonRow {
        person: Person {
            id: id.into(),
            github_login: format!("{id}-gh"),
            github_id: None,
            handle: format!("{id}-gh"),
            name: id.into(),
            avatar_url: None,
            org_role: role,
            state: PersonState::Active,
            groups: vec![],
            settings: PersonSettings::default(),
            created_at: Utc::now(),
        },
        login_key: format!("{id}-gh"),
    };
    h.state.store.put(db::PERSON, id, &person).await.unwrap();
    let token = format!("device-token-{id}");
    let device = DeviceRow {
        id: format!("device-{id}"),
        name: format!("{id}'s Mac"),
        token_hash: sha256_hex(&token),
        github_login: format!("{id}-gh"),
        created_at: Utc::now(),
        last_seen_at: None,
        person_id: Some(id.into()),
    };
    h.state
        .store
        .put(db::DEVICE, &device.id, &device)
        .await
        .unwrap();
    token
}

/// The owner (alice), bob in `company/sales`, carol in `company/engineering`, and the
/// two groups.
struct Org {
    h: Harness,
    alice: String,
    alice_id: String,
    bob: String,
    carol: String,
}

async fn org() -> Org {
    let h = start().await;
    let alice = h.sign_in("Mac").await;
    let me: margin_shared::Me = h
        .get(&alice, margin_shared::routes::ME)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let alice_id = me.person.unwrap().id;
    for (path, name) in [(ENGINEERING, "Engineering"), (SALES, "Sales")] {
        let r = h
            .post(&alice, "/api/groups")
            .json(&json!({"path": path, "name": name}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }
    let bob = seed_person(&h, "bob", OrgRole::Member).await;
    let carol = seed_person(&h, "carol", OrgRole::Member).await;
    for (id, group) in [("bob", SALES), ("carol", ENGINEERING)] {
        let r = h
            .put(&alice, &format!("/api/people/{id}/groups"))
            .json(&json!([group]))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }
    Org {
        h,
        alice,
        alice_id,
        bob,
        carol,
    }
}

fn aud(access: &[&str], deny: &[&str]) -> Audience {
    Audience {
        access: access.iter().map(|s| s.to_string()).collect(),
        deny: deny.iter().map(|s| s.to_string()).collect(),
    }
}

async fn error(r: reqwest::Response) -> (u16, String) {
    let status = r.status().as_u16();
    let e: ApiError = r.json().await.unwrap();
    (status, e.code)
}

async fn library(h: &Harness, t: &str) -> Library {
    let r = h.get(t, "/api/library").send().await.unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

async fn new_topic(h: &Harness, t: &str, parent: Option<&str>, title: &str) -> Topic {
    let r = h
        .post(t, "/api/topics")
        .json(&json!({"parent_id": parent, "title": title}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

async fn upload(h: &Harness, t: &str, name: &str, topic: Option<&str>, text: &str) -> Source {
    let r = h.upload(t, name, topic, text.to_string()).await;
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

async fn wait_ready(h: &Harness, id: &str) {
    eventually(|| async {
        let s: Option<Source> = h.state.store.get(db::SOURCE, id).await.unwrap();
        s.filter(|s| matches!(s.status, SourceStatus::Ready { .. }))
            .map(|_| ())
    })
    .await;
}

async fn document(h: &Harness, id: &str) -> Document {
    h.state.store.get(db::DOCUMENT, id).await.unwrap().unwrap()
}

/// Every passage of `doc` carries `access` and `deny`, and there is at least one.
async fn passages_carry(h: &Harness, doc: &str, access: &[&str], deny: &[&str]) {
    let rows = h.state.store.passages_of(doc).await.unwrap();
    assert!(!rows.is_empty(), "{doc} has no passages");
    for r in rows {
        assert_eq!(r.access, access, "{doc}");
        assert_eq!(r.deny, deny, "{doc}");
    }
}

fn mentions(hits: &[margin_shared::context::Hit], word: &str) -> bool {
    hits.iter().any(|h| h.text.contains(word))
}

#[tokio::test(flavor = "multi_thread")]
async fn publishing_a_topic_cascades_to_its_rows_documents_and_passages() {
    let Org {
        h,
        alice,
        alice_id,
        bob,
        carol,
    } = org().await;
    let me = format!("person/{alice_id}");
    let physics = new_topic(&h, &alice, None, "Physics").await;
    let heat = new_topic(&h, &alice, Some(&physics.id), "Heat").await;
    let s = upload(&h, &alice, "heat.md", Some(&heat.id), HEAT).await;
    wait_ready(&h, &s.id).await;
    let r = h
        .post(&alice, "/api/notes")
        .json(&json!({"topic_id": heat.id, "lesson_id": null, "title": "Carnot", "body_md": "Reservoirs.", "quote": null}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let note: Note = r.json().await.unwrap();

    // Private by default: in the index too.
    passages_carry(&h, &s.id, &[&me], &[]).await;
    assert!(!mentions(
        &h.reader_search(&carol, "Carnot reservoirs").await,
        "Carnot"
    ));
    assert!(mentions(
        &h.reader_search(&alice, "Carnot reservoirs").await,
        "Carnot"
    ));

    let mut carol_socket = h.socket(&carol).await;
    let mut bob_socket = h.socket(&bob).await;
    let r = h
        .publish(
            &alice,
            AudienceUnit::Topic,
            &physics.id,
            &aud(&[ENGINEERING], &[]),
        )
        .await;
    assert_eq!(r.status(), 200);
    let stored: Audience = r.json().await.unwrap();
    assert_eq!(stored, aud(&[&me, ENGINEERING], &[]));

    // The subtree, its note and source, the source's document and every passage.
    let lib = library(&h, &carol).await;
    let ids: Vec<&str> = lib.topics.iter().map(|t| t.id.as_str()).collect();
    assert!(ids.contains(&physics.id.as_str()) && ids.contains(&heat.id.as_str()));
    assert!(lib.notes.iter().any(|n| n.id == note.id));
    assert!(lib.sources.iter().any(|x| x.id == s.id));
    assert!(library(&h, &bob).await.topics.is_empty());
    let doc = document(&h, &s.id).await;
    assert_eq!(doc.access, [me.as_str(), ENGINEERING]);
    passages_carry(&h, &s.id, &[&me, ENGINEERING], &[]).await;
    // The note's document follows once it was indexed.
    let note_doc = eventually(|| async {
        let found: Vec<Document> = h.state.store.list(db::DOCUMENT).await.unwrap();
        found.into_iter().find(|d| {
            d.origin
                == Origin::Library {
                    kind: LibraryKind::Note,
                    id: note.id.clone(),
                }
                && d.access.contains(&ENGINEERING.to_string())
        })
    })
    .await;
    assert_eq!(note_doc.owner.as_deref(), Some(alice_id.as_str()));

    // Search follows the index: carol (engineering) finds it, bob (sales) doesn't.
    assert!(mentions(
        &h.reader_search(&carol, "Carnot reservoirs").await,
        "Carnot"
    ));
    assert!(!mentions(
        &h.reader_search(&bob, "Carnot reservoirs").await,
        "Carnot"
    ));

    // Carol's socket got the rows; bob's got none of them.
    let seen = carol_socket
        .until(|e| matches!(e, Event::DocumentChanged { document } if document.id == s.id))
        .await;
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::TopicChanged { topic } if topic.id == heat.id))
    );
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::SourceChanged { source } if source.id == s.id))
    );

    // Deny carol by name: she loses it all and hears `AudienceRevoked`.
    let r = h
        .publish(
            &alice,
            AudienceUnit::Topic,
            &physics.id,
            &aud(&[ENGINEERING], &["person/carol"]),
        )
        .await;
    assert_eq!(r.status(), 200);
    let seen = carol_socket
        .until(|e| matches!(e, Event::AudienceRevoked { ids, .. } if ids.contains(&physics.id)))
        .await;
    assert!(matches!(
        seen.last(),
        Some(Event::AudienceRevoked {
            unit: AudienceUnit::Topic,
            ..
        })
    ));
    assert!(library(&h, &carol).await.topics.is_empty());
    assert!(!mentions(
        &h.reader_search(&carol, "Carnot reservoirs").await,
        "Carnot"
    ));
    passages_carry(&h, &s.id, &[&me, ENGINEERING], &["person/carol"]).await;

    // Bob never heard a thing about it.
    bob_socket.send_ping().await;
    while let Some(e) = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        bob_socket.next_event(),
    )
    .await
    .ok()
    .flatten()
    {
        assert!(
            !matches!(
                e,
                Event::TopicChanged { .. }
                    | Event::SourceChanged { .. }
                    | Event::NoteChanged { .. }
                    | Event::DocumentChanged { .. }
                    | Event::AudienceRevoked { .. }
            ),
            "{e:?}"
        );
    }

    // The view.
    let r = h
        .get(
            &alice,
            &margin_shared::routes::audience(AudienceUnit::Source, &s.id),
        )
        .send()
        .await
        .unwrap();
    let view: AudienceView = r.json().await.unwrap();
    assert_eq!(
        view.inherited_from,
        Some((AudienceUnit::Topic, heat.id.clone()))
    );
    assert_eq!(view.audience.deny, ["person/carol"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn refusals_say_what_to_do() {
    let Org {
        h,
        alice,
        bob,
        carol,
        ..
    } = org().await;
    let t = new_topic(&h, &alice, None, "Physics").await;
    let s = upload(&h, &alice, "heat.md", Some(&t.id), HEAT).await;
    let p = |a: Audience| {
        let h = &h;
        let alice = alice.clone();
        let id = t.id.clone();
        async move { error(h.publish(&alice, AudienceUnit::Topic, &id, &a).await).await }
    };
    assert_eq!(
        p(aud(&["company/nope"], &[])).await,
        (400, "unknown_group".into())
    );
    assert_eq!(
        p(aud(&["person/nobody"], &[])).await,
        (400, "unknown_person".into())
    );
    assert_eq!(
        p(aud(&["agent/x"], &[])).await,
        (400, "unknown_agent".into())
    );
    assert_eq!(
        p(aud(&["not a path"], &[])).await,
        (400, "bad_access".into())
    );
    // The owner is in `company`: denying it would lock them out.
    assert_eq!(
        p(aud(&["company"], &["company"])).await,
        (400, "bad_access".into())
    );
    // Outside the caller's access: 404; readable but not theirs: 403.
    assert_eq!(
        error(
            h.publish(&bob, AudienceUnit::Topic, &t.id, &aud(&["company"], &[]))
                .await
        )
        .await,
        (404, "not_found".into())
    );
    assert_eq!(
        h.publish(&alice, AudienceUnit::Topic, &t.id, &aud(&["company"], &[]))
            .await
            .status(),
        200
    );
    assert_eq!(
        error(
            h.publish(&carol, AudienceUnit::Topic, &t.id, &aud(&[], &[]))
                .await
        )
        .await,
        (403, "not_owner".into())
    );
    // Lessons, notes and a topic's source follow their topic.
    let r = h
        .put(&alice, "/api/audience/lesson/x")
        .json(&aud(&[], &[]))
        .send()
        .await
        .unwrap();
    assert_eq!(error(r).await, (400, "not_a_unit".into()));
    assert_eq!(
        error(
            h.publish(&alice, AudienceUnit::Source, &s.id, &aud(&["company"], &[]))
                .await
        )
        .await,
        (400, "not_a_unit".into())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_standalone_source_and_an_agents_document_are_units() {
    let Org {
        h,
        alice,
        alice_id,
        carol,
        ..
    } = org().await;
    let me = format!("person/{alice_id}");
    let s = upload(&h, &alice, "pricing.md", None, SECRET).await;
    wait_ready(&h, &s.id).await;
    assert!(!mentions(
        &h.reader_search(&carol, "discount ladder").await,
        "discount"
    ));
    let r = h
        .publish(&alice, AudienceUnit::Source, &s.id, &aud(&["company"], &[]))
        .await;
    assert_eq!(r.status(), 200);
    passages_carry(&h, &s.id, &[&me, "company"], &[]).await;
    assert!(mentions(
        &h.reader_search(&carol, "discount ladder").await,
        "discount"
    ));

    // An agent's document is private to its owner, then published by hand.
    let req = ContextIngest {
        name: "runbook".into(),
        origin: Origin::Agent {
            name: "claude-code".into(),
        },
        kind: SourceKind::Text,
        text: "Restart the queue worker, then drain the dead letters.".into(),
        audience: None,
    };
    let doc = margin_service::ingest::ingest_text_as(&h.state, &req, Some(&alice_id), None)
        .await
        .unwrap();
    assert_eq!(doc.access, [me.as_str()]);
    eventually(|| async {
        let rows = h.state.store.passages_of(&doc.id).await.unwrap();
        (!rows.is_empty()).then_some(())
    })
    .await;
    passages_carry(&h, &doc.id, &[&me], &[]).await;
    assert!(!mentions(
        &h.reader_search(&carol, "dead letters").await,
        "dead letters"
    ));
    let r = h
        .publish(
            &alice,
            AudienceUnit::Document,
            &doc.id,
            &aud(&[ENGINEERING], &[]),
        )
        .await;
    assert_eq!(r.status(), 200);
    passages_carry(&h, &doc.id, &[&me, ENGINEERING], &[]).await;
    assert!(mentions(
        &h.reader_search(&carol, "dead letters").await,
        "dead letters"
    ));
    // A source's document is changed on its source.
    assert_eq!(
        error(
            h.publish(&alice, AudienceUnit::Document, &s.id, &aud(&[], &[]))
                .await
        )
        .await,
        (400, "not_a_unit".into())
    );

    // Ingested again with an audience, it takes it (the owner stays in it).
    let req = ContextIngest {
        audience: Some(aud(&[SALES], &[])),
        ..req
    };
    let again = margin_service::ingest::ingest_text_as(&h.state, &req, Some(&alice_id), None)
        .await
        .unwrap();
    assert_eq!(again.id, doc.id);
    assert_eq!(again.access, [me.as_str(), SALES]);
    passages_carry(&h, &doc.id, &[&me, SALES], &[]).await;
}

fn lesson_row(id: &str, topic: &Topic, drew_on: &[&str]) -> StoredLesson {
    let now = Utc::now();
    StoredLesson {
        lesson: Lesson {
            access: topic.access.clone(),
            deny: topic.deny.clone(),
            owner: topic.owner.clone(),
            citations: Vec::new(),
            id: id.into(),
            topic_id: topic.id.clone(),
            title: id.into(),
            summary: String::new(),
            position: 0,
            level: Level::Intro,
            minutes: 3,
            body_md: "Body.".into(),
            status: LessonStatus::Ready,
            created_at: now,
            updated_at: now,
            written_with: None,
            audience_narrowed: false,
        },
        proposal_id: None,
        previous_body: None,
        previous_citations: None,
        author: Default::default(),
        prompt: None,
        with_quiz: false,
        workspace_ids: Vec::new(),
        drew_on: drew_on.iter().map(|s| s.to_string()).collect(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn lessons_derive_their_audience_and_publishing_derives_it_again() {
    let Org {
        h,
        alice,
        alice_id,
        carol,
        ..
    } = org().await;
    let me = format!("person/{alice_id}");
    let t = new_topic(&h, &alice, None, "Physics").await;
    let own = upload(&h, &alice, "heat.md", Some(&t.id), HEAT).await;
    let sales = upload(&h, &alice, "pricing.md", None, SECRET).await;
    wait_ready(&h, &own.id).await;
    wait_ready(&h, &sales.id).await;
    assert_eq!(
        h.publish(&alice, AudienceUnit::Source, &sales.id, &aud(&[SALES], &[]))
            .await
            .status(),
        200
    );
    // Two lessons: one from the topic's own source, one also from the sales source.
    for row in [
        lesson_row("from-own", &t, &[&own.id]),
        lesson_row("from-sales", &t, &[&own.id, &sales.id]),
    ] {
        h.state
            .store
            .put(db::LESSON, &row.lesson.id, &row)
            .await
            .unwrap();
    }
    // Under a private topic nothing is narrowed: it is private anyway.
    let row: StoredLesson = h
        .state
        .store
        .get(db::LESSON, "from-sales")
        .await
        .unwrap()
        .unwrap();
    let d = audience::derive_lesson(&h.state, &row, &audience::audience_of(&t))
        .await
        .unwrap();
    assert_eq!(d.audience.access, [me.as_str()]);
    assert!(!d.narrowed);

    // Publishing the topic to engineering: the first follows, the second falls to
    // private (no reader the alphabet can name reads both engineering and sales),
    // narrowed.
    let r = h
        .publish(
            &alice,
            AudienceUnit::Topic,
            &t.id,
            &aud(&[ENGINEERING], &[]),
        )
        .await;
    assert_eq!(r.status(), 200);
    let first: StoredLesson = h
        .state
        .store
        .get(db::LESSON, "from-own")
        .await
        .unwrap()
        .unwrap();
    let second: StoredLesson = h
        .state
        .store
        .get(db::LESSON, "from-sales")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.lesson.access, [me.as_str(), ENGINEERING]);
    assert!(!first.lesson.audience_narrowed);
    assert_eq!(second.lesson.access, [me.as_str()]);
    assert!(second.lesson.audience_narrowed);
    let lib = library(&h, &carol).await;
    assert!(lib.lessons.iter().any(|l| l.id == "from-own"));
    assert!(!lib.lessons.iter().any(|l| l.id == "from-sales"));
    // The lessons' own documents follow them once indexed.
    let doc_of = |id: &'static str| {
        let h = &h;
        async move {
            eventually(|| async {
                let docs: Vec<Document> = h.state.store.list(db::DOCUMENT).await.unwrap();
                docs.into_iter().find(|d| {
                    d.origin
                        == Origin::Library {
                            kind: LibraryKind::Lesson,
                            id: id.into(),
                        }
                })
            })
            .await
        }
    };
    margin_service::ingest::index_library(&h.state, LibraryKind::Lesson, "from-sales")
        .await
        .unwrap();
    assert_eq!(doc_of("from-sales").await.access, [me.as_str()]);

    // The sales source goes to the company: publishing the topic again derives the
    // second lesson anew, now un-narrowed, and its document follows.
    assert_eq!(
        h.publish(
            &alice,
            AudienceUnit::Source,
            &sales.id,
            &aud(&["company"], &[])
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        h.publish(
            &alice,
            AudienceUnit::Topic,
            &t.id,
            &aud(&[ENGINEERING], &[])
        )
        .await
        .status(),
        200
    );
    let second: StoredLesson = h
        .state
        .store
        .get(db::LESSON, "from-sales")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.lesson.access, [me.as_str(), ENGINEERING]);
    assert!(!second.lesson.audience_narrowed);
    assert_eq!(
        doc_of("from-sales").await.access,
        [me.as_str(), ENGINEERING]
    );

    // Narrower than the topic without being private: the company's topic, a sales
    // source → sales.
    assert_eq!(
        h.publish(&alice, AudienceUnit::Source, &sales.id, &aud(&[SALES], &[]))
            .await
            .status(),
        200
    );
    assert_eq!(
        h.publish(&alice, AudienceUnit::Topic, &t.id, &aud(&["company"], &[]))
            .await
            .status(),
        200
    );
    let second: StoredLesson = h
        .state
        .store
        .get(db::LESSON, "from-sales")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.lesson.access, [me.as_str(), SALES]);
    assert!(!second.lesson.audience_narrowed);
    // A denial on a source denies the lesson.
    assert_eq!(
        h.publish(
            &alice,
            AudienceUnit::Source,
            &sales.id,
            &aud(&["company"], &["person/bob"])
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        h.publish(&alice, AudienceUnit::Topic, &t.id, &aud(&["company"], &[]))
            .await
            .status(),
        200
    );
    let second: StoredLesson = h
        .state
        .store
        .get(db::LESSON, "from-sales")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.lesson.access, [me.as_str(), "company"]);
    assert_eq!(second.lesson.deny, ["person/bob"]);
}

fn passages_card(chat_id: &str, doc: &str) -> StoredChatItem {
    let now = Utc::now();
    StoredChatItem {
        item: ChatItem {
            id: format!("card-{doc}"),
            chat_id: chat_id.into(),
            turn_id: "turn".into(),
            seq: 1,
            status: ItemStatus::Done,
            created_at: now,
            updated_at: now,
            kind: ChatItemKind::Card {
                call_id: "call".into(),
                card: Card::Passages(PassagesCard {
                    query: "q".into(),
                    passages: vec![Citation {
                        n: 1,
                        source_id: doc.into(),
                        source_name: "x".into(),
                        kind: SourceKind::Text,
                        page: None,
                        start_ms: None,
                        end_ms: None,
                        excerpt: "x".into(),
                        image_url: None,
                        document_id: Some(doc.into()),
                        start_line: None,
                        end_line: None,
                        file: None,
                        section_path: None,
                    }],
                }),
            },
        },
        model_items: Vec::new(),
        full_output: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_note_quoting_narrower_passages_is_refused() {
    let Org { h, alice, .. } = org().await;
    let open = new_topic(&h, &alice, None, "Open").await;
    let mine = new_topic(&h, &alice, None, "Mine").await;
    assert_eq!(
        h.publish(
            &alice,
            AudienceUnit::Topic,
            &open.id,
            &aud(&["company"], &[])
        )
        .await
        .status(),
        200
    );
    let private = upload(&h, &alice, "pricing.md", None, SECRET).await;
    wait_ready(&h, &private.id).await;
    let chat_id = "chat-1";
    let args = |topic: &str| json!({"topic_id": topic, "title": "T", "body_md": "B"});
    // Nothing quoted yet: no refusal.
    assert_eq!(
        audience::note_refusal(&h.state, chat_id, "create_note", &args(&open.id))
            .await
            .unwrap(),
        None
    );
    let card = passages_card(chat_id, &private.id);
    h.state
        .store
        .put(db::CHAT_ITEM, &card.item.id, &card)
        .await
        .unwrap();
    assert_eq!(
        audience::note_refusal(&h.state, chat_id, "create_note", &args(&open.id))
            .await
            .unwrap()
            .as_deref(),
        Some(audience::NOTE_REFUSAL)
    );
    // Into a private topic it is written; other tools aren't checked.
    assert_eq!(
        audience::note_refusal(&h.state, chat_id, "create_note", &args(&mine.id))
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        audience::note_refusal(&h.state, chat_id, "search", &args(&open.id))
            .await
            .unwrap(),
        None
    );
    // Appending to a note of the open topic is the same leak.
    let r = h
        .post(&alice, "/api/notes")
        .json(&json!({"topic_id": open.id, "lesson_id": null, "title": "N", "body_md": "", "quote": null}))
        .send()
        .await
        .unwrap();
    let note: Note = r.json().await.unwrap();
    assert!(
        audience::note_refusal(
            &h.state,
            chat_id,
            "append_to_note",
            &json!({"note_id": note.id, "text": "x"})
        )
        .await
        .unwrap()
        .is_some()
    );
    // Once the quoted document is the company's, the note may be written.
    assert_eq!(
        h.publish(
            &alice,
            AudienceUnit::Source,
            &private.id,
            &aud(&["company"], &[])
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        audience::note_refusal(&h.state, chat_id, "create_note", &args(&open.id))
            .await
            .unwrap(),
        None
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_migration_restamps_every_document_once() {
    let Org {
        h, alice, alice_id, ..
    } = org().await;
    let me = format!("person/{alice_id}");
    let t = new_topic(&h, &alice, None, "Physics").await;
    let s = upload(&h, &alice, "heat.md", Some(&t.id), HEAT).await;
    wait_ready(&h, &s.id).await;
    let req = ContextIngest {
        name: "runbook".into(),
        origin: Origin::Agent {
            name: "claude-code".into(),
        },
        kind: SourceKind::Text,
        text: "Restart the queue worker.".into(),
        audience: None,
    };
    let agent_doc = margin_service::ingest::ingest_text_as(&h.state, &req, Some(&alice_id), None)
        .await
        .unwrap();
    eventually(|| async {
        let rows = h.state.store.passages_of(&agent_doc.id).await.unwrap();
        (!rows.is_empty()).then_some(())
    })
    .await;
    // As a store from before L2: everything `company`, the marker gone.
    {
        let _w = h.state.write_lock.lock().await;
        for id in [&s.id, &agent_doc.id] {
            h.state
                .store
                .set_document_audience(id, &["company".into()], &[])
                .await
                .unwrap();
        }
        let mut settings = h.state.settings_row().await.unwrap();
        settings
            .migrations
            .retain(|m| m != audience::ACCESS_MIGRATION);
        h.state
            .store
            .put(db::SETTINGS, margin_service::state::SETTINGS_ID, &settings)
            .await
            .unwrap();
    }
    passages_carry(&h, &s.id, &["company"], &[]).await;

    audience::migrate(&h.state).await.unwrap();
    passages_carry(&h, &s.id, &[&me], &[]).await;
    passages_carry(&h, &agent_doc.id, &[&me], &[]).await;
    assert_eq!(document(&h, &agent_doc.id).await.access, [me.as_str()]);
    assert!(
        h.state
            .settings_row()
            .await
            .unwrap()
            .migrations
            .iter()
            .any(|m| m == audience::ACCESS_MIGRATION)
    );
    // Once: a second run changes nothing.
    {
        let _w = h.state.write_lock.lock().await;
        h.state
            .store
            .set_document_audience(&s.id, &["company".into()], &[])
            .await
            .unwrap();
    }
    audience::migrate(&h.state).await.unwrap();
    passages_carry(&h, &s.id, &["company"], &[]).await;
}

fn bulk_passage(doc: &str, ord: usize, vector: &[f32]) -> PassageRow {
    PassageRow {
        id: format!("{doc}:{ord}"),
        project_id: "p".into(),
        document_id: doc.into(),
        origin: "agent".into(),
        scope_ids: Vec::new(),
        access: vec!["person/alice".into()],
        deny: Vec::new(),
        unit_id: format!("{doc}:{ord}"),
        modality: "text".into(),
        ord: ord as i64,
        page: None,
        start_line: None,
        end_line: None,
        start_ms: None,
        end_ms: None,
        section_path: String::new(),
        text: format!("passage {ord} of the big document"),
        file: None,
        chunk_hash: format!("h{ord}"),
        embedder: margin_service::ingest::EMBEDDER_VERSION.into(),
        vector: vector.to_vec(),
    }
}

/// Passages in `restamping_a_big_document_is_quick`.
const BIG: usize = 1000;

/// Risk §8.5: re-stamping a document is one indexed statement under the write lock.
/// 1 000 passages within 2 s on kv-mem in a release build (a debug build is about ten
/// times slower). Measured on the M-series Mac for 5 000 passages: 3.3 s release,
/// 31 s debug — SurrealDB re-indexes every passage row it rewrites.
#[tokio::test(flavor = "multi_thread")]
async fn restamping_a_big_document_is_quick() {
    let h = start().await;
    let vector = FakeEmbedder
        .embed(&["the big document".to_string()], Purpose::DOCUMENT)
        .unwrap()
        .remove(0);
    let rows: Vec<PassageRow> = (0..BIG).map(|i| bulk_passage("big", i, &vector)).collect();
    for chunk in rows.chunks(db::INSERT_MANY_MAX) {
        h.state.store.insert_many(db::PASSAGE, chunk).await.unwrap();
    }
    let started = Instant::now();
    {
        let _w = h.state.write_lock.lock().await;
        h.state
            .store
            .set_document_audience(
                "big",
                &["person/alice".into(), "company".into()],
                &["person/bob".into()],
            )
            .await
            .unwrap();
    }
    let took = started.elapsed();
    let rows = h.state.store.passages_of("big").await.unwrap();
    assert_eq!(rows.len(), BIG);
    assert!(
        rows.iter()
            .all(|r| r.access == ["person/alice", "company"] && r.deny == ["person/bob"])
    );
    let budget = if cfg!(debug_assertions) { 20.0 } else { 2.0 };
    assert!(took.as_secs_f64() <= budget, "re-stamping took {took:?}");
}
