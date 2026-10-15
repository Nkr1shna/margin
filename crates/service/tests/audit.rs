//! The audit trail and the owner-only export (L2-audit, `docs/l2-brief.md` D7): every
//! change to who may read or do what writes one row (ids and paths, never content or
//! names), admins read them through `GET /api/audit` and live as `AuditAppended`,
//! `margin-service export` on a stopped copy is logged and shows as a row at the next
//! start.

mod common;

use std::path::Path;

use chrono::{Duration, Utc};
use common::*;
use margin_service::audit::{self, ExportLine};
use margin_service::db::{self, Store};
use margin_service::export;
use margin_service::people::PersonRow;
use margin_service::state::{DeviceRow, sha256_hex};
use margin_shared::context::{AuditKind, AuditRow};
use margin_shared::{
    AgentToken, ApiError, Event, OrgRole, Person, PersonSettings, PersonState, Topic,
};
use serde_json::json;

/// A person with `role` and a signed-in device; returns the device token.
async fn seed_person(h: &Harness, id: &str, role: OrgRole) -> String {
    let person = PersonRow {
        person: Person {
            id: id.into(),
            github_login: format!("{id}-gh"),
            github_id: None,
            handle: format!("{id}-gh"),
            name: format!("Name of {id}"),
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

async fn audit_rows(h: &Harness, token: &str, query: &str) -> Vec<AuditRow> {
    let r = h
        .get(token, &format!("/api/audit{query}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap_or_default());
    assert_eq!(
        r.headers()["content-type"].to_str().unwrap(),
        "application/x-ndjson"
    );
    r.text()
        .await
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn find<'a>(rows: &'a [AuditRow], kind: AuditKind, subject: &str, event: &str) -> &'a AuditRow {
    rows.iter()
        .find(|r| {
            r.kind == kind
                && r.subject.id == subject
                && r.after.get("event").and_then(|e| e.as_str()) == Some(event)
        })
        .unwrap_or_else(|| panic!("no {kind:?} {event} row about {subject} in {rows:#?}"))
}

async fn ok(r: reqwest::Response) -> reqwest::Response {
    assert!(
        r.status().is_success(),
        "{}",
        r.text().await.unwrap_or_default()
    );
    r
}

#[tokio::test(flavor = "multi_thread")]
async fn every_change_to_who_may_do_what_is_journaled_without_names() {
    let h = start().await;
    let ada = seed_person(&h, "ada", OrgRole::Admin).await;
    let bob = seed_person(&h, "bob", OrgRole::Member).await;
    let mut ada_socket = h.socket(&ada).await;
    let mut bob_socket = h.socket(&bob).await;

    // Groups: create, rename, membership, delete.
    ok(h.post(&ada, "/api/groups")
        .json(&json!({"path": "company/sales", "name": "Secret Sales Team"}))
        .send()
        .await
        .unwrap())
    .await;
    ok(h.patch(&ada, "/api/groups/company/sales")
        .json(&json!({"name": "Renamed Sales Team"}))
        .send()
        .await
        .unwrap())
    .await;
    ok(h.put(&ada, "/api/people/bob/groups")
        .json(&json!(["company/sales"]))
        .send()
        .await
        .unwrap())
    .await;
    ok(h.put(&ada, "/api/people/bob/groups")
        .json(&json!([]))
        .send()
        .await
        .unwrap())
    .await;
    ok(h.delete(&ada, "/api/groups/company/sales")
        .send()
        .await
        .unwrap())
    .await;

    // Agents: create, ceiling, disable (by an admin), token mint and revoke, delete.
    let agent: margin_shared::Agent = ok(h
        .post(&bob, "/api/agents")
        .json(&json!({"name": "bob-bot", "kind": "external"}))
        .send()
        .await
        .unwrap())
    .await
    .json()
    .await
    .unwrap();
    ok(h.patch(&bob, &format!("/api/agents/{}", agent.id))
        .json(&json!({"ceiling": ["company", "person/bob"]}))
        .send()
        .await
        .unwrap())
    .await;
    ok(h.patch(&ada, &format!("/api/agents/{}", agent.id))
        .json(&json!({"disabled": true}))
        .send()
        .await
        .unwrap())
    .await;
    let minted: AgentToken = ok(h
        .post(&bob, &format!("/api/agents/{}/token", agent.id))
        .send()
        .await
        .unwrap())
    .await
    .json()
    .await
    .unwrap();
    ok(h.delete(&bob, &format!("/api/agents/{}/token", agent.id))
        .send()
        .await
        .unwrap())
    .await;
    ok(h.delete(&bob, &format!("/api/agents/{}", agent.id))
        .send()
        .await
        .unwrap())
    .await;

    // Invitations: create and revoke.
    let inv = h.invite(&ada, "carol-gh", OrgRole::Member, &[]).await;
    ok(h.delete(&ada, &format!("/api/invitations/{}", inv.id))
        .send()
        .await
        .unwrap())
    .await;

    // A person: role, then disabled (their device goes) and enabled again.
    let owner = h.sign_in("owner's Mac").await;
    ok(h.patch(&owner, "/api/people/bob")
        .json(&json!({"org_role": "admin"}))
        .send()
        .await
        .unwrap())
    .await;
    ok(h.patch(&ada, "/api/people/bob")
        .json(&json!({"state": "disabled"}))
        .send()
        .await
        .unwrap())
    .await;
    ok(h.patch(&ada, "/api/people/bob")
        .json(&json!({"state": "active"}))
        .send()
        .await
        .unwrap())
    .await;

    let rows = audit_rows(&h, &ada, "").await;
    // Oldest first.
    assert!(rows.windows(2).all(|w| w[0].at <= w[1].at));

    let created = find(&rows, AuditKind::GroupChange, "company/sales", "created");
    assert_eq!(created.actor, "person/ada");
    assert_eq!(created.device_id.as_deref(), Some("device-ada"));
    assert_eq!(created.after["parent"], "company");
    find(&rows, AuditKind::GroupChange, "company/sales", "renamed");
    find(&rows, AuditKind::GroupChange, "company/sales", "deleted");
    let joins: Vec<&AuditRow> = rows
        .iter()
        .filter(|r| r.kind == AuditKind::MembershipChange && r.subject.id == "bob")
        .collect();
    assert_eq!(joins.len(), 2, "{joins:#?}");
    // Seeded straight into the store, Bob starts in no group.
    assert_eq!(joins[0].before["groups"], json!([]));
    assert_eq!(joins[0].after["groups"], json!(["company/sales"]));
    assert_eq!(joins[1].after["groups"], json!([]));

    let made = find(&rows, AuditKind::AgentChange, &agent.id, "created");
    assert_eq!(made.actor, "person/bob");
    assert_eq!(made.after["owner"], "person/bob");
    let ceilings: Vec<&AuditRow> = rows
        .iter()
        .filter(|r| {
            r.kind == AuditKind::AgentChange
                && r.subject.id == agent.id
                && r.after["event"] == "changed"
        })
        .collect();
    assert_eq!(ceilings.len(), 2);
    assert_eq!(
        ceilings[0].after["ceiling"],
        json!(["company", "person/bob"])
    );
    assert_eq!(ceilings[1].actor, "person/ada");
    assert_eq!(ceilings[1].after["disabled"], true);
    let mint = find(&rows, AuditKind::AgentToken, &agent.id, "minted");
    let prefix = minted.agent.token.as_ref().unwrap().prefix.clone();
    assert_eq!(mint.after["token_prefix"], json!(prefix));
    let revoked = find(&rows, AuditKind::AgentToken, &agent.id, "revoked");
    assert_eq!(revoked.before["token_prefix"], json!(prefix));
    find(&rows, AuditKind::AgentChange, &agent.id, "deleted");

    let invited = find(&rows, AuditKind::PersonChange, &inv.id, "invited");
    assert_eq!(invited.subject.kind, "invitation");
    assert_eq!(invited.after["role"], "member");
    find(
        &rows,
        AuditKind::PersonChange,
        &inv.id,
        "invitation_revoked",
    );

    let role = find(&rows, AuditKind::PersonChange, "bob", "role_changed");
    assert_eq!(role.before["role"], "member");
    assert_eq!(role.after["role"], "admin");
    let disabled = find(&rows, AuditKind::PersonChange, "bob", "disabled");
    assert_eq!(disabled.after["state"], "disabled");
    let device = find(
        &rows,
        AuditKind::PersonChange,
        "device-bob",
        "device_revoked",
    );
    assert_eq!(device.subject.kind, "device");
    assert_eq!(device.after["reason"], "person_disabled");
    find(&rows, AuditKind::PersonChange, "bob", "enabled");
    // The owner's own sign-in is there too.
    assert!(rows.iter().any(|r| r.after["event"] == "sign_in"));

    // No content and no names anywhere in what is stored or sent.
    let text = serde_json::to_string(
        &rows
            .iter()
            .map(|r| (&r.before, &r.after, &r.actor, &r.subject))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    for name in [
        "Secret Sales Team",
        "Renamed Sales Team",
        "bob-bot",
        "carol-gh",
        "Name of bob",
        "bob's Mac",
        &minted.token,
    ] {
        assert!(!text.contains(name), "{name} is in the audit log: {text}");
    }

    // Live: the admin's socket gets every row; the member's (now admin bob) was closed
    // when disabled, so a fresh member socket is checked below.
    let appended = ada_socket
        .until(|e| matches!(e, Event::AuditAppended { row } if row.after["event"] == "enabled"))
        .await;
    assert!(
        appended
            .iter()
            .filter(|e| matches!(e, Event::AuditAppended { .. }))
            .count()
            >= 15
    );
    // Bob was a member until the role change: no audit row reached him before it.
    loop {
        match tokio::time::timeout(
            std::time::Duration::from_millis(300),
            bob_socket.next_event(),
        )
        .await
        {
            Ok(Some(Event::AuditAppended { row })) => {
                panic!("a member's socket got an audit row: {row:?}")
            }
            Ok(Some(Event::PersonChanged { person }))
                if person.id == "bob" && person.org_role == OrgRole::Admin =>
            {
                break;
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn only_admins_read_the_log_and_names_need_read_access() {
    let h = start().await;
    let ada = seed_person(&h, "ada", OrgRole::Admin).await;
    let bob = seed_person(&h, "bob", OrgRole::Member).await;

    let r = h.get(&bob, "/api/audit").send().await.unwrap();
    assert_eq!(r.status(), 403);
    let e: ApiError = r.json().await.unwrap();
    assert_eq!(e.code, "admin_required");

    // Bob's private topic and Ada's own: an audit row about each.
    let topic = |title: &str| json!({"parent_id": null, "title": title});
    let bobs: Topic = ok(h
        .post(&bob, "/api/topics")
        .json(&topic("Bob's secret plans"))
        .send()
        .await
        .unwrap())
    .await
    .json()
    .await
    .unwrap();
    let adas: Topic = ok(h
        .post(&ada, "/api/topics")
        .json(&topic("Ada's notes"))
        .send()
        .await
        .unwrap())
    .await
    .json()
    .await
    .unwrap();
    for (owner, t) in [("bob", &bobs), ("ada", &adas)] {
        audit::record(
            &h.state,
            audit::row(
                AuditKind::AudienceChange,
                format!("person/{owner}"),
                "topic",
                &t.id,
                json!({"access": [format!("person/{owner}")], "deny": []}),
                json!({"access": [format!("person/{owner}"), "company"], "deny": []}),
            ),
        );
    }
    ok(h.post(&ada, "/api/groups")
        .json(&json!({"path": "company/eng", "name": "Engineering"}))
        .send()
        .await
        .unwrap())
    .await;
    let rows = audit_rows(&h, &ada, "?kind=audience_change").await;
    assert_eq!(rows.len(), 2);
    let name_of = |id: &str| {
        rows.iter()
            .find(|r| r.subject.id == id)
            .unwrap()
            .subject_name
            .clone()
    };
    // An admin reads no content: Bob's topic is named by its owner only.
    assert_eq!(name_of(&bobs.id).as_deref(), Some("a topic of @bob-gh"));
    assert_eq!(name_of(&adas.id).as_deref(), Some("Ada's notes"));

    // Filters: subject, kind, window, and the newest page.
    let one = audit_rows(&h, &ada, &format!("?subject={}", bobs.id)).await;
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].subject.id, bobs.id);
    let future = (Utc::now() + Duration::hours(1)).to_rfc3339();
    let later = (Utc::now() + Duration::hours(2)).to_rfc3339();
    let none = audit_rows(
        &h,
        &ada,
        &format!("?from={}&to={}", urlencode(&future), urlencode(&later)),
    )
    .await;
    assert!(none.is_empty());
    let r = h
        .get(
            &ada,
            &format!(
                "/api/audit?from={}&to={}",
                urlencode(&future),
                urlencode(&Utc::now().to_rfc3339())
            ),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let all = audit_rows(&h, &ada, "").await;
    assert_eq!(all.len(), 3, "{all:#?}");
    let page = audit_rows(&h, &ada, "?limit=2").await;
    assert_eq!(page.len(), 2);
    assert_eq!(page, all[all.len() - 2..].to_vec());
    let older = audit_rows(
        &h,
        &ada,
        &format!(
            "?limit=2&to={}",
            urlencode(
                &(page[0].at - Duration::nanoseconds(1))
                    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            )
        ),
    )
    .await;
    assert_eq!(older.last().unwrap(), &all[all.len() - 3]);
}

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B").replace(':', "%3A")
}

#[tokio::test(flavor = "multi_thread")]
async fn sign_ins_are_journaled_approved_and_denied() {
    let h = start().await;
    let owner = h.sign_in("owner's Mac").await;
    let _bob_inv = h.invite(&owner, "bob-dev", OrgRole::Member, &[]).await;
    let _bob = h.sign_in_as(BOB_CODE, "Bob's phone").await;
    // A login nobody invited.
    let s = h.start_login("stranger").await;
    let r = h.callback(OTHER_CODE, &Harness::state_param(&s)).await;
    assert_eq!(r.status(), 403);

    let rows = audit_rows(&h, &owner, "?kind=person_change").await;
    let owner_row: PersonRow = h
        .state
        .store
        .list::<PersonRow>(db::PERSON)
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.person.org_role == OrgRole::Owner)
        .unwrap();
    let signed = find(
        &rows,
        AuditKind::PersonChange,
        &owner_row.person.id,
        "sign_in",
    );
    assert_eq!(signed.actor, format!("person/{}", owner_row.person.id));
    let bob_row = h
        .state
        .store
        .list::<PersonRow>(db::PERSON)
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.person.github_login == "bob-dev")
        .unwrap();
    find(&rows, AuditKind::PersonChange, &bob_row.person.id, "joined");
    find(
        &rows,
        AuditKind::PersonChange,
        &bob_row.person.id,
        "sign_in",
    );
    let denied = find(&rows, AuditKind::PersonChange, "2", "sign_in_denied");
    assert_eq!(denied.actor, "github/2");
    assert_eq!(denied.subject.kind, "github_account");
    assert_eq!(denied.after["reason"], "not_invited");
    let joined = audit_rows(&h, &owner, "?kind=membership_change").await;
    let m = joined
        .iter()
        .find(|r| r.subject.id == bob_row.person.id)
        .expect("the invitation's acceptance is a membership change");
    assert_eq!(m.after["groups"], json!(["company"]));
    assert!(
        !serde_json::to_string(&rows)
            .unwrap()
            .contains("someone-else")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn exports_log_lines_become_rows_once() {
    let line = |person: &str, at: &str| {
        serde_json::to_string(&ExportLine {
            at: at.parse().unwrap(),
            person: person.into(),
            by: "root".into(),
            host: "mini".into(),
            out: "/srv/exports/p".into(),
            ok: true,
        })
        .unwrap()
    };
    let log = format!(
        "{}\nnot json\n{}\n",
        line("p1", "2026-10-01T09:00:00Z"),
        line("p2", "2026-10-02T09:00:00Z")
    );
    let store = Store::memory().await.unwrap();
    let h = start_with(Setup {
        store: Some(store.clone()),
        files: vec![(export::EXPORTS_LOG.into(), log.clone().into_bytes())],
        ..Setup::default()
    })
    .await;
    let owner = h.sign_in("owner's Mac").await;
    let rows = audit_rows(&h, &owner, "?kind=export").await;
    assert_eq!(rows.len(), 2, "{rows:#?}");
    assert_eq!(rows[0].actor, "host/root");
    assert_eq!(rows[0].subject.id, "p1");
    assert_eq!(rows[0].after["host"], "mini");
    drop(h);

    // The next start reads the log again: nothing new.
    let h = start_with(Setup {
        store: Some(store),
        files: vec![(export::EXPORTS_LOG.into(), log.into_bytes())],
        ..Setup::default()
    })
    .await;
    let owner = h.sign_in("owner's Mac").await;
    assert_eq!(audit_rows(&h, &owner, "?kind=export").await.len(), 2);
}

/// Puts a raw row (as the service stores it) into an on-disk store.
async fn put(store: &Store, table: &'static str, id: &str, row: serde_json::Value) {
    store.put(table, id, &row).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn export_reads_a_stopped_copy_and_logs_itself() {
    let root = std::env::temp_dir().join(format!("margin-export-test-{}", uuid::Uuid::new_v4()));
    let data = root.join("data");
    let db_dir = data.join("db");
    let store = Store::open(&db_dir).await.unwrap();
    let now = Utc::now();
    for (id, login) in [("alice", "alice-gh"), ("bob", "bob-gh")] {
        let row = PersonRow::new(Person {
            id: id.into(),
            github_login: login.into(),
            github_id: None,
            handle: login.into(),
            name: id.into(),
            avatar_url: None,
            org_role: OrgRole::Member,
            state: PersonState::Active,
            groups: vec![],
            settings: PersonSettings::default(),
            created_at: now,
        });
        store.put(db::PERSON, id, &row).await.unwrap();
    }
    let owned = |id: &str, owner: &str, title: &str| json!({"id": id, "owner": owner, "title": title, "access": [format!("person/{owner}")], "deny": []});
    put(
        &store,
        db::TOPIC,
        "t-a",
        owned("t-a", "alice", "Alice's topic"),
    )
    .await;
    put(&store, db::TOPIC, "t-b", owned("t-b", "bob", "Bob's topic")).await;
    put(&store, db::LESSON, "l-a", owned("l-a", "alice", "A lesson")).await;
    put(&store, db::NOTE, "n-a", owned("n-a", "alice", "A note")).await;
    put(&store, db::CHAT, "c-a", owned("c-a", "alice", "A chat")).await;
    put(&store, db::CHAT, "c-b", owned("c-b", "bob", "Bob's chat")).await;
    put(
        &store,
        db::CHAT_ITEM,
        "i-a",
        json!({"id": "i-a", "chat_id": "c-a", "text": "hi"}),
    )
    .await;
    put(
        &store,
        db::CHAT_ITEM,
        "i-b",
        json!({"id": "i-b", "chat_id": "c-b", "text": "no"}),
    )
    .await;
    put(
        &store,
        db::SOURCE,
        "s-a",
        json!({"id": "s-a", "owner": "alice", "name": "paper.pdf"}),
    )
    .await;
    put(
        &store,
        db::DOCUMENT,
        "d-a",
        json!({"id": "d-a", "owner": "alice", "name": "agent doc"}),
    )
    .await;
    put(
        &store,
        db::DOCUMENT,
        "d-b",
        json!({"id": "d-b", "owner": "bob", "name": "bob doc"}),
    )
    .await;
    std::fs::create_dir_all(data.join("sources/s-a")).unwrap();
    std::fs::write(data.join("sources/s-a/original"), b"pdf bytes").unwrap();
    std::fs::create_dir_all(data.join("documents/d-a")).unwrap();
    std::fs::write(data.join("documents/d-a/original"), b"agent text").unwrap();
    std::fs::create_dir_all(data.join("documents/d-b")).unwrap();
    std::fs::write(data.join("documents/d-b/original"), b"bob's text").unwrap();

    // The database is open (as when the service runs): refused, nothing written.
    let out = root.join("out");
    let e = export::export(&data, "alice", &out, "root", "mini")
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("is open"), "{e:#}");
    assert!(!out.exists());
    assert!(!data.join(export::EXPORTS_LOG).exists());

    drop(store);
    margin_service::restore::wait_released(&db_dir)
        .await
        .unwrap();

    // An unknown person: refused, nothing logged.
    assert!(
        export::export(&data, "nobody", &out, "root", "mini")
            .await
            .is_err()
    );
    assert!(!data.join(export::EXPORTS_LOG).exists());

    // By handle.
    let done = export::export(&data, "@alice-gh", &out, "root", "mini")
        .await
        .unwrap();
    assert_eq!(done.person, "alice");
    let dir = out.join("alice");
    let lines = |f: &str| -> Vec<serde_json::Value> {
        std::fs::read_to_string(dir.join(f))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };
    let ids = |f: &str| -> Vec<String> {
        lines(f)
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(ids("topics.jsonl"), ["t-a"]);
    assert_eq!(ids("lessons.jsonl"), ["l-a"]);
    assert_eq!(ids("notes.jsonl"), ["n-a"]);
    assert_eq!(ids("chats.jsonl"), ["c-a"]);
    assert_eq!(ids("chat_items.jsonl"), ["i-a"]);
    assert_eq!(ids("sources.jsonl"), ["s-a"]);
    assert_eq!(ids("documents.jsonl"), ["d-a"]);
    assert_eq!(
        std::fs::read(dir.join("files/sources/s-a/original")).unwrap(),
        b"pdf bytes"
    );
    assert_eq!(
        std::fs::read(dir.join("files/documents/d-a/original")).unwrap(),
        b"agent text"
    );
    assert!(!dir.join("files/documents/d-b").exists());

    // Logged, one line.
    let log = std::fs::read_to_string(data.join(export::EXPORTS_LOG)).unwrap();
    let logged: Vec<ExportLine> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(logged.len(), 1);
    assert_eq!(logged[0].person, "alice");
    assert_eq!(
        (logged[0].by.as_str(), logged[0].host.as_str()),
        ("root", "mini")
    );
    assert!(logged[0].ok);
    assert!(Path::new(&logged[0].out).is_absolute());

    // A second export into the same folder is refused (and not logged).
    assert!(
        export::export(&data, "alice", &out, "root", "mini")
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(data.join(export::EXPORTS_LOG))
            .unwrap()
            .lines()
            .count(),
        1
    );

    // The live data was never opened for writing: the store opens as before.
    let store = Store::open_existing(&db_dir).await.unwrap().unwrap();
    assert!(
        store
            .get::<serde_json::Value>(db::TOPIC, "t-a")
            .await
            .unwrap()
            .is_some()
    );
    drop(store);
    let _ = margin_service::restore::wait_released(&db_dir).await;
    let _ = std::fs::remove_dir_all(&root);
}
