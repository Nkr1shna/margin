//! Groups, memberships and expansions (L1-groups, `docs/l1-brief.md` D1, D5), through
//! the HTTP API. People are seeded straight into the store with a device each, so
//! these tests don't depend on sign-in admission.

mod common;

use chrono::Utc;
use common::*;
use margin_service::db;
use margin_service::groups::{self, GroupRow};
use margin_service::people::PersonRow;
use margin_service::state::{DeviceRow, sha256_hex};
use margin_shared::{ApiError, Event, Group, OrgRole, Person, PersonSettings, PersonState};
use serde_json::json;

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

async fn create(h: &Harness, t: &str, path: &str, name: &str) -> reqwest::Response {
    h.post(t, "/api/groups")
        .json(&json!({"path": path, "name": name}))
        .send()
        .await
        .unwrap()
}

async fn list(h: &Harness, t: &str) -> Vec<Group> {
    let r = h.get(t, "/api/groups").send().await.unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn admins_make_the_tree_and_everyone_reads_it() {
    let h = start().await;
    let ada = seed_person(&h, "ada", OrgRole::Admin).await;
    let bob = seed_person(&h, "bob", OrgRole::Member).await;
    let mut socket = h.socket(&bob).await;

    // `company` is always there.
    let groups = list(&h, &bob).await;
    assert_eq!(groups.len(), 1);
    assert_eq!((groups[0].path.as_str(), groups[0].members), ("company", 0));

    let r = create(&h, &ada, "company/engineering", "Engineering").await;
    assert_eq!(r.status(), 200);
    let eng: Group = r.json().await.unwrap();
    assert_eq!((eng.source.as_str(), eng.members), ("manual", 0));
    // A directory event: the member's socket hears it.
    let seen = socket
        .until(|e| matches!(e, Event::GroupChanged { .. }))
        .await;
    assert!(
        matches!(seen.last(), Some(Event::GroupChanged { group }) if group.path == "company/engineering")
    );

    assert_eq!(
        create(&h, &ada, "company/engineering/web", "Web")
            .await
            .status(),
        200
    );
    assert_eq!(
        list(&h, &bob)
            .await
            .iter()
            .map(|g| g.path.as_str())
            .collect::<Vec<_>>(),
        vec!["company", "company/engineering", "company/engineering/web"]
    );

    // Refusals.
    assert_eq!(
        error(create(&h, &bob, "company/sales", "Sales").await).await,
        (403, "admin_required".into())
    );
    assert_eq!(
        error(create(&h, &ada, "company/engineering", "Again").await).await,
        (409, "exists".into())
    );
    assert_eq!(
        error(create(&h, &ada, "company", "Root").await).await,
        (409, "exists".into())
    );
    assert_eq!(
        error(create(&h, &ada, "company/nope/x", "Orphan").await).await,
        (400, "unknown_group".into())
    );
    for bad in ["Company/x", "sales", "company//x", "company/x/"] {
        assert_eq!(
            error(create(&h, &ada, bad, "Bad").await).await,
            (400, "bad_path".into()),
            "{bad}"
        );
    }
    assert_eq!(
        error(create(&h, &ada, "company/sales", "  ").await).await.0,
        400
    );

    // Rename (a path with slashes in the route).
    let r = h
        .patch(&ada, "/api/groups/company/engineering/web")
        .json(&json!({"name": "Web team"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let web: Group = r.json().await.unwrap();
    assert_eq!(web.name, "Web team");
    let r = h
        .patch(&bob, "/api/groups/company/engineering/web")
        .json(&json!({"name": "Mine"}))
        .send()
        .await
        .unwrap();
    assert_eq!(error(r).await, (403, "admin_required".into()));
    let r = h
        .patch(&ada, "/api/groups/company/nope")
        .json(&json!({"name": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn memberships_count_block_deletes_and_move_expansions() {
    let h = start().await;
    let ada = seed_person(&h, "ada", OrgRole::Admin).await;
    seed_person(&h, "bob", OrgRole::Member).await;
    for (path, name) in [
        ("company/engineering", "Engineering"),
        ("company/engineering/web", "Web"),
    ] {
        assert_eq!(create(&h, &ada, path, name).await.status(), 200);
    }

    let before = groups::expansion(&h.state, "bob").await.unwrap();
    assert_eq!(before.paths, vec!["company", "person/bob"]);
    assert_eq!(before.org_role, OrgRole::Member);
    let generation = h.state.expansions.generation();

    let now = groups::set_memberships(&h.state, "bob", &["company/engineering/web".into()])
        .await
        .unwrap();
    assert_eq!(now, vec!["company/engineering/web"]);
    assert!(h.state.expansions.generation() > generation);
    let after = groups::expansion(&h.state, "bob").await.unwrap();
    assert_eq!(
        after.paths,
        vec![
            "company",
            "company/engineering",
            "company/engineering/web",
            "person/bob"
        ]
    );
    // Cached until the next write.
    assert!(std::sync::Arc::ptr_eq(
        &after,
        &groups::expansion(&h.state, "bob").await.unwrap()
    ));
    let e = groups::set_memberships(&h.state, "bob", &["company/sales".into()])
        .await
        .unwrap_err();
    assert_eq!(e.code, "unknown_group");

    let counts: Vec<(String, u32)> = list(&h, &ada)
        .await
        .into_iter()
        .map(|g| (g.path, g.members))
        .collect();
    assert_eq!(
        counts,
        vec![
            ("company".to_string(), 0),
            ("company/engineering".to_string(), 0),
            ("company/engineering/web".to_string(), 1),
        ]
    );

    // Deleting: children first, members first, never `company`.
    let del = |path: &str| h.delete(&ada, &format!("/api/groups/{path}")).send();
    assert_eq!(
        error(del("company/engineering").await.unwrap()).await,
        (400, "has_children".into())
    );
    assert_eq!(
        error(del("company/engineering/web").await.unwrap()).await,
        (400, "has_members".into())
    );
    assert_eq!(error(del("company").await.unwrap()).await.0, 400);
    assert_eq!(del("company/nope").await.unwrap().status(), 404);

    groups::set_memberships(&h.state, "bob", &[]).await.unwrap();
    assert_eq!(
        groups::expansion(&h.state, "bob").await.unwrap().paths,
        vec!["company", "person/bob"]
    );
    assert_eq!(del("company/engineering/web").await.unwrap().status(), 204);
    assert_eq!(del("company/engineering").await.unwrap().status(), 204);
    assert_eq!(list(&h, &ada).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn groups_from_another_source_are_not_changed_here() {
    let h = start().await;
    let ada = seed_person(&h, "ada", OrgRole::Admin).await;
    let synced = GroupRow {
        path: "company/gh-team".into(),
        name: "GitHub team".into(),
        source: "github-org".into(),
        created_at: Utc::now(),
    };
    h.state
        .store
        .put(db::GROUP, &synced.path, &synced)
        .await
        .unwrap();
    let groups = list(&h, &ada).await;
    assert!(
        groups
            .iter()
            .any(|g| g.path == "company/gh-team" && g.source == "github-org")
    );
    let r = h
        .patch(&ada, "/api/groups/company/gh-team")
        .json(&json!({"name": "Mine now"}))
        .send()
        .await
        .unwrap();
    assert_eq!(error(r).await, (400, "managed_by_source".into()));
    let r = h
        .delete(&ada, "/api/groups/company/gh-team")
        .send()
        .await
        .unwrap();
    assert_eq!(error(r).await, (400, "managed_by_source".into()));

    // The manual source lists only what the API made.
    assert_eq!(create(&h, &ada, "company/ops", "Ops").await.status(), 200);
    let manual = &h.state.access_sources[0];
    assert_eq!(manual.name(), "manual");
    let paths: Vec<String> = manual
        .groups()
        .await
        .unwrap()
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    assert_eq!(paths, vec!["company", "company/ops"]);
}
