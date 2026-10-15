//! Agents, ceilings and `mga_` tokens, and the context API on an agent token
//! (L1-groups, `docs/l1-brief.md` D6, D7; e2e 4). People are seeded straight into the
//! store with a device each, so these tests don't depend on sign-in admission.

mod common;

use chrono::Utc;
use common::*;
use margin_service::db;
use margin_service::groups;
use margin_service::people::PersonRow;
use margin_service::state::{DeviceRow, sha256_hex};
use margin_shared::context::{Document, Origin, TraceKind, TraceRow};
use margin_shared::{Agent, AgentToken, ApiError, OrgRole, Person, PersonSettings, PersonState};
use serde_json::{Value, json};

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

async fn error(r: reqwest::Response) -> (u16, String) {
    let status = r.status().as_u16();
    let e: ApiError = r.json().await.unwrap();
    (status, e.code)
}

async fn create_agent(h: &Harness, t: &str, body: Value) -> reqwest::Response {
    h.post(t, "/api/agents").json(&body).send().await.unwrap()
}

async fn mint(h: &Harness, t: &str, id: &str) -> AgentToken {
    let r = h
        .post(t, &format!("/api/agents/{id}/token"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

async fn search(h: &Harness, t: &str, extra: Value) -> reqwest::Response {
    let mut body = json!({"text": "carnot engine", "scope": {"scope": "library"}, "k": 3});
    if let (Value::Object(b), Value::Object(e)) = (&mut body, extra) {
        b.extend(e);
    }
    h.post(t, "/api/context/search")
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn patch(h: &Harness, t: &str, id: &str, body: Value) -> reqwest::Response {
    h.patch(t, &format!("/api/agents/{id}"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn list_ids(h: &Harness, t: String) -> Vec<String> {
    let agents: Vec<Agent> = h
        .get(&t, "/api/agents")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    agents.into_iter().map(|a| a.id).collect()
}

async fn traces(h: &Harness, t: &str, kind: &str) -> Vec<TraceRow> {
    let r = h
        .get(t, "/api/traces")
        .query(&[("kind", kind)])
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

#[tokio::test(flavor = "multi_thread")]
async fn agents_ceilings_and_who_may_change_them() {
    let h = start().await;
    let ada = seed_person(&h, "ada", OrgRole::Admin).await;
    let bob = seed_person(&h, "bob", OrgRole::Member).await;
    let carol = seed_person(&h, "carol", OrgRole::Member).await;
    for path in ["company/engineering", "company/sales"] {
        let r = h
            .post(&ada, "/api/groups")
            .json(&json!({"path": path, "name": path}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }
    groups::set_memberships(&h.state, "bob", &["company/engineering".into()])
        .await
        .unwrap();

    // Defaults by kind (owner, Q5).
    let r = create_agent(&h, &bob, json!({"name": "claude-code", "kind": "external"})).await;
    assert_eq!(r.status(), 200);
    let ext: Agent = r.json().await.unwrap();
    assert_eq!(ext.owner_id, "bob");
    assert_eq!(ext.ceiling, vec!["company", "company/engineering"]);
    assert!(ext.token.is_none() && !ext.disabled);
    let mine: Agent = create_agent(&h, &bob, json!({"name": "margin", "kind": "margin"}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        mine.ceiling,
        vec!["company", "company/engineering", "person/bob"]
    );
    let asked: Agent = create_agent(
        &h,
        &bob,
        json!({"name": "eng-bot", "kind": "voice", "ceiling": ["company/engineering"]}),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(asked.ceiling, vec!["company", "company/engineering"]);

    // Refusals at creation.
    assert_eq!(
        error(create_agent(&h, &bob, json!({"name": "claude-code", "kind": "margin"})).await).await,
        (409, "exists".into())
    );
    assert_eq!(
        error(create_agent(&h, &bob, json!({"name": "Claude Code", "kind": "margin"})).await).await,
        (400, "bad_name".into())
    );
    assert_eq!(
        error(
            create_agent(
                &h,
                &bob,
                json!({"name": "sales-bot", "kind": "external", "ceiling": ["company/sales"]})
            )
            .await
        )
        .await,
        (400, "outside_owner".into())
    );
    // The same name is fine for someone else.
    let carols: Agent = create_agent(
        &h,
        &carol,
        json!({"name": "claude-code", "kind": "external"}),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(carols.ceiling, vec!["company"]);

    // Listing: own agents; an admin sees everyone's.
    let list = |t: String| list_ids(&h, t);
    assert_eq!(
        list(bob.clone()).await,
        vec![ext.id.clone(), mine.id.clone(), asked.id.clone()]
    );
    assert_eq!(list(carol.clone()).await, vec![carols.id.clone()]);
    assert_eq!(list(ada.clone()).await.len(), 4);

    // Changing: the owner; someone else's is 404; an admin may only disable it.
    assert_eq!(
        error(patch(&h, &carol, &ext.id, json!({"disabled": true})).await).await,
        (404, "not_found".into())
    );
    assert_eq!(
        error(patch(&h, &ada, &ext.id, json!({"name": "renamed"})).await).await,
        (403, "not_owner".into())
    );
    assert_eq!(
        error(patch(&h, &ada, &ext.id, json!({"ceiling": ["company"]})).await).await,
        (403, "not_owner".into())
    );
    let off: Agent = patch(&h, &ada, &ext.id, json!({"disabled": true}))
        .await
        .json()
        .await
        .unwrap();
    assert!(off.disabled);
    assert_eq!(
        error(patch(&h, &bob, &ext.id, json!({"name": "margin"})).await).await,
        (409, "exists".into())
    );
    assert_eq!(
        error(patch(&h, &bob, &ext.id, json!({"ceiling": ["company/sales"]})).await).await,
        (400, "outside_owner".into())
    );
    let on: Agent = patch(
        &h,
        &bob,
        &ext.id,
        json!({"disabled": false, "ceiling": ["person/bob"], "name": "claude"}),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(
        (on.name.as_str(), on.ceiling.clone(), on.disabled),
        ("claude", vec!["person/bob".to_string()], false)
    );

    // Deleting: the owner only.
    assert_eq!(
        error(
            h.delete(&ada, &format!("/api/agents/{}", asked.id))
                .send()
                .await
                .unwrap()
        )
        .await,
        (403, "not_owner".into())
    );
    let r = h
        .delete(&bob, &format!("/api/agents/{}", asked.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    assert_eq!(
        list(bob.clone()).await,
        vec![ext.id.clone(), mine.id.clone()]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_token_reads_context_within_its_ceiling() {
    let h = start().await;
    let ada = seed_person(&h, "ada", OrgRole::Admin).await;
    let bob = seed_person(&h, "bob", OrgRole::Member).await;
    let carol = seed_person(&h, "carol", OrgRole::Member).await;
    let agent: Agent = create_agent(&h, &bob, json!({"name": "claude-code", "kind": "external"}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(agent.ceiling, vec!["company"]);

    // Minted once; listed by prefix only.
    let minted = mint(&h, &bob, &agent.id).await;
    let token = minted.token.clone();
    assert!(token.starts_with("mga_") && token.len() > 40);
    let info = minted.agent.token.clone().unwrap();
    assert_eq!(info.prefix, token[..8]);
    assert!(info.expires_at.is_none());
    let body = h
        .get(&bob, "/api/agents")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains(&info.prefix) && !body.contains(&token));
    assert_eq!(
        error(
            h.post(&ada, &format!("/api/agents/{}/token", agent.id))
                .send()
                .await
                .unwrap()
        )
        .await,
        (403, "not_owner".into())
    );

    // The context API works on it; the rest of /api doesn't.
    assert_eq!(search(&h, &token, json!({})).await.status(), 200);
    let r = h.get(&token, "/api/projects").send().await.unwrap();
    assert_eq!(r.status(), 200);
    for path in ["/api/library", "/api/events", "/api/agents", "/api/traces"] {
        let r = h.get(&token, path).send().await.unwrap();
        assert_eq!(error(r).await, (403, "agent_not_allowed".into()), "{path}");
    }
    for path in [
        "/api/providers/chatgpt/start",
        "/api/providers/chatgpt/complete",
    ] {
        let r = h
            .post(&token, path)
            .json(&json!({ "callback_url": "http://127.0.0.1:1455/auth/callback?code=c&state=s" }))
            .send()
            .await
            .unwrap();
        assert_eq!(error(r).await, (403, "agent_not_allowed".into()), "{path}");
    }

    // Its trace row says which agent, for whom; only admins read traces.
    let rows = traces(&h, &ada, "retrieval").await;
    let row = rows
        .iter()
        .find(|r| r.principal.as_deref() == Some(&format!("agent/{}", agent.id)))
        .unwrap_or_else(|| panic!("no row of the agent's in {rows:?}"));
    assert_eq!(row.kind, TraceKind::Retrieval);
    assert_eq!(row.actor, Some(format!("agent/{}", agent.id)));
    assert_eq!(row.via.as_deref(), Some("mcp"));
    assert_eq!(row.query.as_deref(), Some("carnot engine"));
    for path in ["/api/traces", "/api/traces/rollup?bucket=day"] {
        let r = h.get(&bob, path).send().await.unwrap();
        assert_eq!(error(r).await, (403, "admin_required".into()), "{path}");
    }
    // A device search names its person.
    assert_eq!(search(&h, &carol, json!({})).await.status(), 200);
    let rows = traces(&h, &ada, "retrieval").await;
    assert!(
        rows.iter()
            .any(|r| r.principal.as_deref() == Some("person/carol"))
    );

    // Access: only what the ceiling and the owner both allow.
    assert_eq!(
        error(search(&h, &token, json!({"access": ["person/bob"]})).await).await,
        (400, "bad_access".into())
    );
    assert_eq!(
        search(&h, &token, json!({"access": ["company"]}))
            .await
            .status(),
        200
    );
    assert_eq!(
        search(&h, &bob, json!({"access": ["person/bob"]}))
            .await
            .status(),
        200
    );
    assert_eq!(
        error(search(&h, &bob, json!({"access": ["person/carol"]})).await).await,
        (400, "bad_access".into())
    );
    patch(
        &h,
        &bob,
        &agent.id,
        json!({"ceiling": ["company", "person/bob"]}),
    )
    .await;
    assert_eq!(
        search(&h, &token, json!({"access": ["person/bob"]}))
            .await
            .status(),
        200
    );

    // Disabled by an admin: 401 until re-enabled.
    patch(&h, &ada, &agent.id, json!({"disabled": true})).await;
    assert_eq!(search(&h, &token, json!({})).await.status(), 401);
    patch(&h, &bob, &agent.id, json!({"disabled": false})).await;
    assert_eq!(search(&h, &token, json!({})).await.status(), 200);

    // Rotating kills the old token; revoking the new one.
    let rotated = mint(&h, &bob, &agent.id).await.token;
    assert_eq!(search(&h, &token, json!({})).await.status(), 401);
    assert_eq!(search(&h, &rotated, json!({})).await.status(), 200);
    let r = h
        .delete(&bob, &format!("/api/agents/{}/token", agent.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    assert_eq!(search(&h, &rotated, json!({})).await.status(), 401);
    assert_eq!(search(&h, "mga_made-up", json!({})).await.status(), 401);

    // A disabled owner's agent stops working.
    let again = mint(&h, &bob, &agent.id).await.token;
    assert_eq!(search(&h, &again, json!({})).await.status(), 200);
    let mut row: PersonRow = h.state.store.get(db::PERSON, "bob").await.unwrap().unwrap();
    row.person.state = PersonState::Disabled;
    h.state.store.put(db::PERSON, "bob", &row).await.unwrap();
    assert_eq!(search(&h, &again, json!({})).await.status(), 401);

    // A deleted agent's token dies with it.
    row.person.state = PersonState::Active;
    h.state.store.put(db::PERSON, "bob", &row).await.unwrap();
    assert_eq!(search(&h, &again, json!({})).await.status(), 200);
    let r = h
        .delete(&bob, &format!("/api/agents/{}", agent.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    assert_eq!(search(&h, &again, json!({})).await.status(), 401);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agents_documents_belong_to_its_owner() {
    let h = start().await;
    let bob = seed_person(&h, "bob", OrgRole::Member).await;
    let carol = seed_person(&h, "carol", OrgRole::Member).await;
    let agent: Agent = create_agent(&h, &bob, json!({"name": "claude-code", "kind": "external"}))
        .await
        .json()
        .await
        .unwrap();
    let token = mint(&h, &bob, &agent.id).await.token;

    let ingest = |t: String, agent_name: &'static str, text: &'static str| {
        let h = &h;
        async move {
            h.post(&t, "/api/context/ingest")
                .header("x-margin-agent", "someone-else")
                .json(&json!({
                    "name": "handoff.md",
                    "origin": {"origin": "agent", "name": agent_name},
                    "text": text
                }))
                .send()
                .await
                .unwrap()
        }
    };
    // On an agent token the origin is the agent's own name, whatever the body says.
    let r = ingest(token.clone(), "spoofed", "Session notes: the Carnot cycle.").await;
    assert_eq!(r.status(), 200);
    let doc: Document = r.json().await.unwrap();
    assert_eq!(doc.owner.as_deref(), Some("bob"));
    assert_eq!(doc.actor, format!("agent/{}", agent.id));
    assert_eq!(
        doc.origin,
        Origin::Agent {
            name: "claude-code".into()
        }
    );
    let stored: Document = h
        .state
        .store
        .get(db::DOCUMENT, &doc.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.owner.as_deref(), Some("bob"));

    // Carol's `claude-code` document of the same name is her own, beside Bob's: his is
    // neither replaced nor revealed.
    let r = ingest(carol.clone(), "claude-code", "Carol's own handoff.").await;
    assert_eq!(r.status(), 200);
    let carols_handoff: Document = r.json().await.unwrap();
    assert_ne!(carols_handoff.id, doc.id);
    assert_eq!(carols_handoff.owner.as_deref(), Some("carol"));
    let bobs: Document = h
        .state
        .store
        .get(db::DOCUMENT, &doc.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bobs.owner.as_deref(), Some("bob"));
    // Bob's document is private to him: to Carol it doesn't exist (404, not 403).
    let r = h
        .delete(&carol, &format!("/api/context/documents/{}", doc.id))
        .send()
        .await
        .unwrap();
    assert_eq!(error(r).await, (404, "not_found".into()));

    // Carol's own agent document is hers; Bob's device replaces Bob's.
    let r = ingest(carol.clone(), "carols-bot", "Carol's notes.").await;
    let carols: Document = r.json().await.unwrap();
    assert_eq!(carols.owner.as_deref(), Some("carol"));
    assert_eq!(carols.actor, "agent:carols-bot");
    let r = ingest(bob.clone(), "claude-code", "Session notes, revised.").await;
    assert_eq!(r.status(), 200);
    let replaced: Document = r.json().await.unwrap();
    assert_eq!(replaced.id, doc.id);
    assert_eq!(replaced.owner.as_deref(), Some("bob"));

    // The agent deletes its owner's document (once it is read).
    eventually(|| async {
        let d: Document = h
            .state
            .store
            .get(db::DOCUMENT, &doc.id)
            .await
            .unwrap()
            .unwrap();
        matches!(
            d.status,
            margin_shared::context::DocumentStatus::Ready { .. }
        )
        .then_some(())
    })
    .await;
    let r = h
        .delete(&token, &format!("/api/context/documents/{}", doc.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
}
