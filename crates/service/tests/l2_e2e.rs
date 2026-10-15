//! L2 end to end (`docs/l2-brief.md` §5): access control inside search, across the five
//! areas, through the real router with the harness's fakes (`FakeEmbedder` bag of words,
//! no model API, no running service).
//!
//! Covered elsewhere and not repeated here: the migration (§5.1,
//! `tests/audience.rs::the_migration_restamps_every_document_once`), derived lesson
//! audiences and the `create_note` refusal (§5.5, `tests/audience.rs`), the `Export`
//! row (§5.8, `tests/audit.rs`), the dense leg's `EXPLAIN` (§5.3, `db.rs` tests) and the
//! owner's ranked ids (§5.9, `margin-evals`' `access_gates::owner_rank_identical`).

mod common;

use std::collections::HashMap;

use chrono::Utc;
use common::*;
use margin_service::db;
use margin_service::people::PersonRow;
use margin_service::state::{DeviceRow, sha256_hex};
use margin_shared::context::{AuditKind, AuditRow, ContextChanges, Hit, TraceRow};
use margin_shared::{
    Agent, AgentToken, ApiError, Audience, AudienceUnit, Event, Note, OrgRole, Person,
    PersonSettings, PersonState, SearchHit, Source, SourceStatus, Topic,
};
use serde_json::{Value, json};

const ENGINEERING: &str = "company/engineering";
const PLATFORM: &str = "company/engineering/platform";
const SALES: &str = "company/sales";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

/// The owner (alice, an admin), bob in `company/sales`, carol in
/// `company/engineering/platform`, dave in no group (`company` only).
struct Org {
    h: Harness,
    alice: String,
    alice_id: String,
    bob: String,
    carol: String,
    dave: String,
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
    for (path, name) in [
        (ENGINEERING, "Engineering"),
        (PLATFORM, "Platform"),
        (SALES, "Sales"),
    ] {
        new_group(&h, &alice, path, name).await;
    }
    let bob = seed_person(&h, "bob", OrgRole::Member).await;
    let carol = seed_person(&h, "carol", OrgRole::Member).await;
    let dave = seed_person(&h, "dave", OrgRole::Member).await;
    for (id, group) in [("bob", SALES), ("carol", PLATFORM)] {
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
        dave,
    }
}

async fn new_group(h: &Harness, t: &str, path: &str, name: &str) {
    let r = h
        .post(t, "/api/groups")
        .json(&json!({"path": path, "name": name}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap_or_default());
}

fn aud(access: &[&str], deny: &[&str]) -> Audience {
    Audience {
        access: access.iter().map(|s| s.to_string()).collect(),
        deny: deny.iter().map(|s| s.to_string()).collect(),
    }
}

async fn publish(h: &Harness, t: &str, unit: AudienceUnit, id: &str, a: Audience) {
    let r = h.publish(t, unit, id, &a).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap_or_default());
}

async fn error(r: reqwest::Response) -> (u16, String) {
    let status = r.status().as_u16();
    let e: ApiError = r.json().await.unwrap();
    (status, e.code)
}

async fn new_topic(h: &Harness, t: &str, title: &str) -> Topic {
    let r = h
        .post(t, "/api/topics")
        .json(&json!({"parent_id": null, "title": title}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

async fn new_note(h: &Harness, t: &str, topic: &str, title: &str, body: &str) -> Note {
    let r = h
        .post(t, "/api/notes")
        .json(&json!({"topic_id": topic, "lesson_id": null, "title": title, "body_md": body, "quote": null}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap_or_default());
    r.json().await.unwrap()
}

async fn upload_ready(h: &Harness, t: &str, name: &str, topic: &str, text: &str) -> Source {
    let r = h.upload(t, name, Some(topic), text.to_string()).await;
    assert_eq!(r.status(), 200);
    let s: Source = r.json().await.unwrap();
    eventually(|| async {
        let row: Option<Source> = h.state.store.get(db::SOURCE, &s.id).await.unwrap();
        row.filter(|s| matches!(s.status, SourceStatus::Ready { .. }))
            .map(|_| ())
    })
    .await;
    s
}

fn mentions(hits: &[Hit], word: &str) -> bool {
    hits.iter().any(|h| h.text.contains(word))
}

/// The document holding `word`, once `t` finds it (indexing is in the background).
async fn indexed(h: &Harness, t: &str, word: &str) -> String {
    eventually(|| async {
        h.reader_search(t, word)
            .await
            .into_iter()
            .find(|hit| hit.text.contains(word))
            .map(|hit| hit.document_id)
    })
    .await
}

async fn cmd_k(h: &Harness, t: &str, q: &str) -> Vec<SearchHit> {
    let r = h
        .get(t, "/api/search")
        .query(&[("q", q)])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

/// `GET /api/context/documents/{id}/outline` and `…/passages`' statuses for `t`.
async fn by_id(h: &Harness, t: &str, doc: &str) -> (u16, u16) {
    let outline = h
        .get(t, &format!("/api/context/documents/{doc}/outline"))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    let passages = h
        .get(t, &format!("/api/context/documents/{doc}/passages"))
        .query(&[("from", "0"), ("to", "3")])
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    (outline, passages)
}

async fn changes(h: &Harness, t: &str, since: u64) -> ContextChanges {
    let r = h
        .get(t, "/api/context/changes")
        .query(&[("since", since.to_string())])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap_or_default());
    r.json().await.unwrap()
}

async fn traces(h: &Harness, admin: &str) -> Vec<TraceRow> {
    let r = h
        .get(admin, "/api/traces")
        .query(&[("kind", "retrieval")])
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

async fn audit_rows(h: &Harness, t: &str, query: &str) -> Vec<AuditRow> {
    let r = h
        .get(t, &format!("/api/audit{query}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap_or_default());
    r.text()
        .await
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Every event `socket` got until the `GroupChanged` of `marker`, which goes to
/// everyone: what it didn't get by then, it never got.
async fn events_until_marker(
    h: &Harness,
    admin: &str,
    socket: &mut Socket,
    marker: &str,
) -> Vec<Event> {
    new_group(h, admin, &format!("company/{marker}"), marker).await;
    socket
        .until(|e| matches!(e, Event::GroupChanged { group } if group.path.ends_with(marker)))
        .await
}

async fn agent_with_token(h: &Harness, t: &str, name: &str) -> (Agent, String) {
    let r = h
        .post(t, "/api/agents")
        .json(&json!({"name": name, "kind": "external"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap_or_default());
    let agent: Agent = r.json().await.unwrap();
    let r = h
        .post(t, &format!("/api/agents/{}/token", agent.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let minted: AgentToken = r.json().await.unwrap();
    (agent, minted.token)
}

async fn set_ceiling(h: &Harness, t: &str, agent: &str, ceiling: &[&str]) {
    let r = h
        .patch(t, &format!("/api/agents/{agent}"))
        .json(&json!({ "ceiling": ceiling }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap_or_default());
}

// ---------------------------------------------------------------------------
// §5.2 The leak tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_private_note_leaks_nowhere() {
    let Org { h, alice, bob, .. } = org().await;
    let mut bobs = h.socket(&bob).await;
    let t = new_topic(&h, &alice, "Pricing").await;
    let note = new_note(
        &h,
        &alice,
        &t.id,
        "Ladder",
        "The zygomorphic discount ladder.",
    )
    .await;
    let doc = indexed(&h, &alice, "zygomorphic").await;
    let source = upload_ready(
        &h,
        &alice,
        "ladder.md",
        &t.id,
        "# Ladder\n\nQuixotic margins.\n",
    )
    .await;
    indexed(&h, &alice, "Quixotic").await;

    // Search, ⌘K, by-id reads and the feed: nothing.
    assert!(h.reader_search(&bob, "zygomorphic").await.is_empty());
    assert!(!mentions(
        &h.reader_search(&bob, "Quixotic margins").await,
        "Quixotic"
    ));
    assert!(cmd_k(&h, &bob, "zygomorphic").await.is_empty());
    assert!(!cmd_k(&h, &alice, "zygomorphic").await.is_empty());
    assert_eq!(by_id(&h, &bob, &doc).await, (404, 404));
    assert_eq!(by_id(&h, &alice, &doc).await, (200, 200));
    let feed = changes(&h, &bob, 0).await;
    assert!(feed.changed.iter().all(|d| d.id != doc), "{feed:?}");
    // The audience view of a source under a private topic is a 404, as the source is.
    for (unit, id) in [
        (AudienceUnit::Source, source.id.as_str()),
        (AudienceUnit::Topic, t.id.as_str()),
        (AudienceUnit::Document, doc.as_str()),
    ] {
        let path = margin_shared::routes::audience(unit, id);
        let r = h.get(&bob, &path).send().await.unwrap();
        assert_eq!(r.status(), 404, "{path}");
        let r = h.get(&alice, &path).send().await.unwrap();
        assert_eq!(r.status(), 200, "{path}");
    }

    // Bob's retrieval trace rows name him and hold no hit.
    let rows = traces(&h, &alice).await;
    let his: Vec<&TraceRow> = rows
        .iter()
        .filter(|r| r.principal.as_deref() == Some("person/bob"))
        .collect();
    assert!(!his.is_empty());
    assert!(his.iter().all(|r| r.hit_ids.is_empty()), "{his:?}");

    // Bob's socket heard nothing about the note, its topic or its documents.
    let events = events_until_marker(&h, &alice, &mut bobs, "marker-leak").await;
    for e in &events {
        match e {
            Event::DocumentChanged { document } => {
                assert_eq!(document.owner.as_deref(), Some("bob"), "{e:?}")
            }
            Event::NoteChanged { note: n } => assert_ne!(n.id, note.id),
            Event::TopicChanged { topic } => assert_ne!(topic.id, t.id),
            Event::SourceChanged { source: s } => assert_ne!(s.id, source.id),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// §5.3 Hierarchy the right way round, §5.4 deny
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_group_reads_down_the_tree_never_up_and_a_deny_wins() {
    let Org {
        h,
        alice,
        bob,
        carol,
        dave,
        ..
    } = org().await;
    let t = new_topic(&h, &alice, "Systems").await;
    new_note(
        &h,
        &alice,
        &t.id,
        "Kernels",
        "Idempotent xylophone schedulers.",
    )
    .await;
    let doc = indexed(&h, &alice, "xylophone").await;

    publish(
        &h,
        &alice,
        AudienceUnit::Topic,
        &t.id,
        aud(&[ENGINEERING], &[]),
    )
    .await;
    // Carol (platform, under engineering) reads it, with the lexical highlight.
    let carols = h.reader_search(&carol, "xylophone").await;
    let hit = carols
        .iter()
        .find(|h| h.text.contains("xylophone"))
        .expect("carol finds the engineering topic");
    assert!(
        hit.highlight
            .as_deref()
            .is_some_and(|s| s.contains("**xylophone**")),
        "{hit:?}"
    );
    assert_eq!(by_id(&h, &carol, &doc).await, (200, 200));
    // Dave (company only) and Bob (sales) don't: a group never reads its children's.
    for t in [&dave, &bob] {
        assert!(!mentions(
            &h.reader_search(t, "xylophone").await,
            "xylophone"
        ));
        assert_eq!(by_id(&h, t, &doc).await, (404, 404));
    }
    assert!(mentions(
        &h.reader_search(&alice, "xylophone").await,
        "xylophone"
    ));

    // To the company, but not Bob: Bob loses it live, Carol keeps it.
    publish(
        &h,
        &alice,
        AudienceUnit::Topic,
        &t.id,
        aud(&["company"], &[]),
    )
    .await;
    assert!(mentions(
        &h.reader_search(&bob, "xylophone").await,
        "xylophone"
    ));
    assert!(!cmd_k(&h, &bob, "xylophone").await.is_empty());
    let mut bobs = h.socket(&bob).await;
    let mut carols = h.socket(&carol).await;
    publish(
        &h,
        &alice,
        AudienceUnit::Topic,
        &t.id,
        aud(&["company"], &["person/bob"]),
    )
    .await;
    assert!(!mentions(
        &h.reader_search(&bob, "xylophone").await,
        "xylophone"
    ));
    assert_eq!(by_id(&h, &bob, &doc).await, (404, 404));
    assert!(cmd_k(&h, &bob, "xylophone").await.is_empty());
    assert!(mentions(
        &h.reader_search(&carol, "xylophone").await,
        "xylophone"
    ));
    assert!(mentions(
        &h.reader_search(&dave, "xylophone").await,
        "xylophone"
    ));

    let events = events_until_marker(&h, &alice, &mut bobs, "marker-deny-b").await;
    assert!(
        events.iter().any(|e| matches!(e,
            Event::AudienceRevoked { unit: AudienceUnit::Topic, ids } if ids.contains(&t.id))),
        "{events:?}"
    );
    assert!(!events.iter().any(|e| matches!(e,
        Event::TopicChanged { topic } if topic.id == t.id)));
    let events = events_until_marker(&h, &alice, &mut carols, "marker-deny-c").await;
    assert!(events.iter().any(|e| matches!(e,
        Event::TopicChanged { topic } if topic.id == t.id)));
    assert!(events.iter().any(|e| matches!(e,
        Event::DocumentChanged { document } if document.id == doc)));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::AudienceRevoked { .. }))
    );

    // Bob's agent reads no more than Bob.
    let (bob_agent, bob_token) = agent_with_token(&h, &bob, "bob-bot").await;
    set_ceiling(&h, &bob, &bob_agent.id, &["company"]).await;
    assert!(!mentions(
        &h.reader_search(&bob_token, "xylophone").await,
        "xylophone"
    ));
}

// ---------------------------------------------------------------------------
// §5.6 Agents
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_reads_within_its_owner_and_its_ceiling() {
    let Org {
        h,
        alice,
        alice_id,
        carol,
        ..
    } = org().await;
    let private = new_topic(&h, &alice, "Diary").await;
    new_note(&h, &alice, &private.id, "Mine", "Quokka thoughts.").await;
    indexed(&h, &alice, "Quokka").await;
    let eng = new_topic(&h, &alice, "Platform").await;
    new_note(&h, &alice, &eng.id, "Runbook", "Restart the wombat queue.").await;
    let eng_doc = indexed(&h, &alice, "wombat").await;
    publish(
        &h,
        &alice,
        AudienceUnit::Topic,
        &eng.id,
        aud(&[ENGINEERING], &[]),
    )
    .await;

    // Alice is in engineering herself: an agent never reads more than its owner.
    let r = h
        .put(&alice, &format!("/api/people/{alice_id}/groups"))
        .json(&json!([ENGINEERING]))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let (agent, token) = agent_with_token(&h, &alice, "claude-code").await;
    // The default ceiling is the owner's groups; narrowed to the company first.
    assert_eq!(agent.ceiling, ["company", ENGINEERING]);
    set_ceiling(&h, &alice, &agent.id, &["company"]).await;
    let me = format!("agent/{}", agent.id);
    // `company` only: none of her private rows, nothing in engineering.
    assert!(!mentions(
        &h.reader_search(&token, "Quokka").await,
        "Quokka"
    ));
    assert!(!mentions(
        &h.reader_search(&token, "wombat").await,
        "wombat"
    ));
    assert_eq!(by_id(&h, &token, &eng_doc).await, (404, 404));
    // Widened to engineering: what she published there.
    set_ceiling(&h, &alice, &agent.id, &["company", ENGINEERING]).await;
    assert!(mentions(&h.reader_search(&token, "wombat").await, "wombat"));
    assert!(!mentions(
        &h.reader_search(&token, "Quokka").await,
        "Quokka"
    ));
    // Denied the agent by name: the agent loses it, Alice and Carol keep it.
    publish(
        &h,
        &alice,
        AudienceUnit::Topic,
        &eng.id,
        aud(&[ENGINEERING], &[&me]),
    )
    .await;
    assert!(!mentions(
        &h.reader_search(&token, "wombat").await,
        "wombat"
    ));
    assert!(mentions(&h.reader_search(&alice, "wombat").await, "wombat"));
    assert!(mentions(&h.reader_search(&carol, "wombat").await, "wombat"));
    // Shared with the agent only: the agent and Alice, not Carol.
    publish(
        &h,
        &alice,
        AudienceUnit::Topic,
        &private.id,
        aud(&[&me], &[]),
    )
    .await;
    assert!(mentions(&h.reader_search(&token, "Quokka").await, "Quokka"));
    assert!(mentions(&h.reader_search(&alice, "Quokka").await, "Quokka"));
    assert!(!mentions(
        &h.reader_search(&carol, "Quokka").await,
        "Quokka"
    ));

    // Ingest: an audience outside the agent's paths is refused; inside it, it holds.
    let ingest = |audience: Value| {
        h.post(&token, "/api/context/ingest")
            .json(&json!({
                "name": "shared.md", "origin": {"origin": "agent", "name": "claude-code"},
                "text": "Narwhal deploys on Fridays.", "audience": audience,
            }))
            .send()
    };
    let r = ingest(json!({"access": [SALES], "deny": []}))
        .await
        .unwrap();
    assert_eq!(error(r).await, (400, "bad_access".into()));
    let r = ingest(json!({"access": [ENGINEERING], "deny": []}))
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap_or_default());
    let shared = indexed(&h, &carol, "Narwhal").await;
    // The feed names the document's owner; her device token deletes it.
    let feed = changes(&h, &alice, 0).await;
    let listed = feed
        .changed
        .iter()
        .find(|d| d.id == shared)
        .expect("alice's feed lists the agent's document");
    assert_eq!(listed.owner.as_deref(), Some(alice_id.as_str()));
    let r = h
        .delete(&carol, &format!("/api/context/documents/{shared}"))
        .send()
        .await
        .unwrap();
    assert_eq!(error(r).await, (403, "not_owner".into()));
    let r = h
        .delete(&alice, &format!("/api/context/documents/{shared}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    assert!(!mentions(
        &h.reader_search(&carol, "Narwhal").await,
        "Narwhal"
    ));
    assert!(
        changes(&h, &carol, feed.generation)
            .await
            .deleted
            .contains(&shared)
    );
}

// ---------------------------------------------------------------------------
// §5.7 Publishing and the feed, §5.8 audit
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn publishing_moves_the_feed_the_cache_and_the_audit_log() {
    let Org {
        h,
        alice,
        alice_id,
        bob,
        carol,
        ..
    } = org().await;
    let mut alices = h.socket(&alice).await;
    let mut carols = h.socket(&carol).await;
    let t = new_topic(&h, &alice, "Release").await;
    new_note(&h, &alice, &t.id, "Train", "Ocelot release train.").await;
    let doc = indexed(&h, &alice, "Ocelot").await;
    let before = changes(&h, &carol, 0).await;
    assert!(before.changed.iter().all(|d| d.id != doc));
    let generation = before.generation;

    publish(
        &h,
        &alice,
        AudienceUnit::Topic,
        &t.id,
        aud(&[ENGINEERING], &[]),
    )
    .await;
    let after = changes(&h, &carol, generation).await;
    assert!(after.generation > generation);
    let listed = after
        .changed
        .iter()
        .find(|d| d.id == doc)
        .expect("carol's feed lists the published document");
    assert_eq!(
        listed.access,
        [format!("person/{alice_id}"), ENGINEERING.to_string()]
    );

    // The cache: Carol's first search misses, the same search again hits.
    let count = |rows: &[TraceRow]| {
        rows.iter()
            .filter(|r| r.principal.as_deref() == Some("person/carol"))
            .count()
    };
    let seen = count(&traces(&h, &alice).await);
    assert!(mentions(
        &h.reader_search(&carol, "Ocelot release").await,
        "Ocelot"
    ));
    assert!(mentions(
        &h.reader_search(&carol, "Ocelot release").await,
        "Ocelot"
    ));
    let rows = eventually(|| async {
        let rows = traces(&h, &alice).await;
        (count(&rows) >= seen + 2).then_some(rows)
    })
    .await;
    let hers: Vec<Option<bool>> = rows
        .iter()
        .filter(|r| r.principal.as_deref() == Some("person/carol"))
        .skip(seen)
        .map(|r| r.cache_hit)
        .collect();
    assert_eq!(hers[..2], [Some(false), Some(true)], "{hers:?}");

    // "Only me": Carol's feed drops it, her by-id read is a 404.
    let g = changes(&h, &carol, 0).await.generation;
    publish(&h, &alice, AudienceUnit::Topic, &t.id, aud(&[], &[])).await;
    assert!(
        changes(&h, &carol, g)
            .await
            .changed
            .iter()
            .all(|d| d.id != doc)
    );
    assert_eq!(by_id(&h, &carol, &doc).await, (404, 404));
    assert!(!mentions(
        &h.reader_search(&carol, "Ocelot release").await,
        "Ocelot"
    ));

    // Bob revokes his own device.
    let r = h
        .delete(&bob, "/api/devices/device-bob")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    // One row per change, ids and paths only; admins read them, members don't.
    let rows = audit_rows(&h, &alice, "?kind=audience_change").await;
    let mine: Vec<&AuditRow> = rows.iter().filter(|r| r.subject.id == t.id).collect();
    assert_eq!(mine.len(), 2, "{mine:#?}");
    assert_eq!(mine[0].actor, format!("person/{alice_id}"));
    assert_eq!(mine[0].subject.kind, "topic");
    assert_eq!(
        mine[0].after["access"],
        json!([format!("person/{alice_id}"), ENGINEERING])
    );
    assert_eq!(mine[1].before["access"], mine[0].after["access"]);
    assert_eq!(
        mine[1].after["access"],
        json!([format!("person/{alice_id}")])
    );
    // (Names are joined at read time, for subjects the admin reads; stored rows hold none.)
    let mut all = audit_rows(&h, &alice, "").await;
    all.iter_mut().for_each(|r| r.subject_name = None);
    let stored = serde_json::to_string(&all).unwrap();
    for content in ["Ocelot", "Release", "Train"] {
        assert!(!stored.contains(content), "{content} in {stored}");
    }
    let revoked = audit_rows(&h, &alice, "?kind=device_revoked").await;
    let row = revoked
        .iter()
        .find(|r| r.subject.id == "device-bob")
        .expect("a device_revoked row");
    assert_eq!(row.actor, "person/bob");
    assert_eq!(row.device_id.as_deref(), Some("device-bob"));
    let r = h.get(&carol, "/api/audit").send().await.unwrap();
    assert_eq!(error(r).await, (403, "admin_required".into()));

    // `AuditAppended` reached the admin, never a member; it carries no name.
    let events = events_until_marker(&h, &alice, &mut alices, "marker-audit-a").await;
    let appended: Vec<&AuditRow> = events
        .iter()
        .filter_map(|e| match e {
            Event::AuditAppended { row } => Some(row),
            _ => None,
        })
        .collect();
    assert!(
        appended
            .iter()
            .any(|r| r.kind == AuditKind::AudienceChange && r.subject.id == t.id),
        "{appended:#?}"
    );
    assert!(appended.iter().all(|r| r.subject_name.is_none()));
    let events = events_until_marker(&h, &alice, &mut carols, "marker-audit-c").await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::AuditAppended { .. }))
    );
}

// ---------------------------------------------------------------------------
// §5.10 The grep test
// ---------------------------------------------------------------------------

/// Statements over `passage` or `document` that read by document id (or count) for a
/// writer or the service itself, never for a reader: the allow-list, one by one.
const WRITERS: &[(&str, &str)] = &[
    // A count of documents still indexing, for the alert.
    ("alerts.rs", "ingest_backlog"),
    // The context API's ingest: whether the caller's agent document exists already,
    // to refuse `not_owner` (a reader-less lookup by origin and name; nothing is
    // returned to the caller but the refusal).
    ("context_api.rs", "existing_agent_document"),
    // The generation counter.
    ("db.rs", "next_generation"),
    // Ingest's diff and the re-stamp: a document's own passages, by id.
    ("db.rs", "passages_of"),
    ("db.rs", "has_passages"),
    // Re-embedding: documents whose vectors came from another embedder.
    ("db.rs", "documents_with_other_embedder"),
    // Ingest's upsert: the document for an origin.
    ("ingest.rs", "find_by_origin"),
    // The cache key's generation: counts and sums, no rows.
    ("retrieval.rs", "index_generation"),
    // Whether pre-`passages-v1` rows remain: a count.
    ("retrieval.rs", "legacy_sources"),
];

/// A string literal and the function it is in.
struct Literal {
    file: String,
    function: String,
    text: String,
}

/// The string literals of a Rust file outside its `#[cfg(test)] mod tests`, each with
/// its enclosing function, and each function's text.
fn literals(file: &str, src: &str) -> (Vec<Literal>, HashMap<String, String>) {
    let src = match src.find("#[cfg(test)]\nmod tests") {
        Some(i) => &src[..i],
        None => src,
    };
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut fns: Vec<(usize, String)> = Vec::new();
    let mut i = 0;
    let current = |fns: &Vec<(usize, String)>| {
        fns.last()
            .map(|(_, n)| n.clone())
            .unwrap_or_else(|| "?".into())
    };
    while i < b.len() {
        if b[i..].starts_with(b"//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i..].starts_with(b"r\"") || b[i..].starts_with(b"r#\"") {
            let hashes = if b[i + 1] == b'#' { 1 } else { 0 };
            let start = i + 2 + hashes;
            let close: &[u8] = if hashes == 1 { b"\"#" } else { b"\"" };
            let mut j = start;
            while j < b.len() && !b[j..].starts_with(close) {
                j += 1;
            }
            out.push(Literal {
                file: file.into(),
                function: current(&fns),
                text: src[start..j].into(),
            });
            i = j + close.len();
        } else if b[i] == b'\'' {
            // A char literal ('"', '\'', 'x') or a lifetime.
            if b.get(i + 1) == Some(&b'\\') {
                let mut j = i + 2;
                while j < b.len() && b[j] != b'\'' {
                    j += 1;
                }
                i = j + 1;
            } else if b.get(i + 2) == Some(&b'\'') {
                i += 3;
            } else {
                i += 1;
            }
        } else if b[i] == b'"' {
            let start = i + 1;
            let mut j = start;
            while j < b.len() && b[j] != b'"' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            out.push(Literal {
                file: file.into(),
                function: current(&fns),
                text: src[start..j].into(),
            });
            i = j + 1;
        } else if b[i..].starts_with(b"fn ")
            && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_'))
        {
            let name: String = src[i + 3..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            fns.push((i, name));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut bodies = HashMap::new();
    for (k, (at, name)) in fns.iter().enumerate() {
        let end = fns.get(k + 1).map_or(src.len(), |(e, _)| *e);
        bodies
            .entry(name.clone())
            .or_insert_with(String::new)
            .push_str(&src[*at..end]);
    }
    (out, bodies)
}

/// Whether `s` reads rows of `passage` or `document` (not `document_deleted`).
fn reads_rows(s: &str) -> bool {
    ["FROM passage", "FROM document"].iter().any(|from| {
        s.match_indices(from).any(|(at, _)| {
            s[at + from.len()..]
                .chars()
                .next()
                .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'))
        })
    })
}

#[test]
fn no_statement_without_a_reader() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut checked = 0;
    let mut allowed_seen = Vec::new();
    let mut failures = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let file = path.file_name().unwrap().to_string_lossy().to_string();
        let src = std::fs::read_to_string(&path).unwrap();
        let (lits, bodies) = literals(&file, &src);
        for lit in lits.iter().filter(|l| reads_rows(&l.text)) {
            checked += 1;
            let body = bodies.get(&lit.function).map_or("", String::as_str);
            let both = lit.text.contains("$paths") && lit.text.contains("$deny");
            // The reader's conjuncts spliced in: the Filter's (which ends in
            // `Reader::sql()`), `Reader::sql()` itself, or `DOC_READER_SQL`.
            let spliced = (lit.text.contains("{cond}") && body.contains("filter."))
                || (lit.text.contains("{}") && body.contains("Reader::sql()"))
                || lit.text.contains("{DOC_READER_SQL}")
                || (lit.text.contains("{dense}")
                    && lit.text.contains("{lex}")
                    && body.contains("self.dense_sql(")
                    && body.contains("self.lex_sql("));
            let allowed = WRITERS
                .iter()
                .any(|(f, n)| *f == lit.file && *n == lit.function);
            if allowed {
                allowed_seen.push((lit.file.clone(), lit.function.clone()));
            }
            if !(both || spliced || allowed) {
                failures.push(format!(
                    "{}::{}: {}",
                    lit.file,
                    lit.function,
                    lit.text.split_whitespace().collect::<Vec<_>>().join(" ")
                ));
            }
        }
    }
    assert!(checked > 10, "only {checked} statements found");
    assert!(
        failures.is_empty(),
        "statements over passage/document without the reader's two conjuncts \
         (add `Reader::sql()`/`DOC_READER_SQL`, or name a writer in WRITERS):\n{}",
        failures.join("\n")
    );
    // The allow-list stays exact: every entry still names a statement.
    for (f, n) in WRITERS {
        assert!(
            allowed_seen.iter().any(|(sf, sn)| sf == f && sn == n),
            "WRITERS names {f}::{n}, which no longer reads passage or document"
        );
    }
    // The Filter's statement fragment ends in the reader's conjuncts, and they hold
    // both parameters.
    let retrieval = std::fs::read_to_string(dir.join("retrieval.rs")).unwrap();
    let (_, bodies) = literals("retrieval.rs", &retrieval);
    assert!(bodies["sql"].contains("sql.push_str(Reader::sql());"));
    let pipeline = std::fs::read_to_string(dir.join("pipeline.rs")).unwrap();
    let (lits, _) = literals("pipeline.rs", &pipeline);
    assert!(lits.iter().any(|l| l.function == "sql"
        && l.text.contains("access CONTAINSANY $paths")
        && l.text.contains("deny CONTAINSNONE $deny")));
    // No prefix rule anywhere.
    for name in ["may_see", "visible_paths"] {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "rs") {
                let src = std::fs::read_to_string(&path).unwrap();
                assert!(!src.contains(name), "{name} in {}", path.display());
            }
        }
    }
}
