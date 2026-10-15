//! L1-auth: the `people-v1` migration, admission by person or invitation, `/api/me`,
//! devices per person, the people and invitation routes, disabling a person.

mod common;

use chrono::{Duration, Utc};
use common::*;
use margin_service::db::{self, Store};
use margin_service::groups::GroupRow;
use margin_service::invitations::InvitationRow;
use margin_service::people::{self, PersonRow};
use margin_service::state::{DeviceRow, SETTINGS_ID, SettingsRow, sha256_hex};
use margin_shared::{
    ApiError, Device, Invitation, LoginState, Me, Note, OrgRole, Person, PersonSettings,
    PersonState, Provider, ReasoningEffort, SOURCE_MANUAL, Topic, access_code,
};
use serde_json::json;

async fn api_error(r: reqwest::Response) -> (u16, ApiError) {
    let status = r.status().as_u16();
    (status, r.json().await.unwrap())
}

async fn me(h: &Harness, token: &str) -> Me {
    let r = h.get(token, "/api/me").send().await.unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

async fn add_group(h: &Harness, path: &str) {
    let g = GroupRow {
        path: path.into(),
        name: path.rsplit('/').next().unwrap().into(),
        source: SOURCE_MANUAL.into(),
        created_at: Utc::now(),
    };
    h.state.store.put(db::GROUP, path, &g).await.unwrap();
}

/// The callback's page text and the poll's outcome for `code`.
async fn try_sign_in(h: &Harness, code: &str) -> (u16, String, LoginState) {
    let s = h.start_login("Phone").await;
    let page = h.callback(code, &Harness::state_param(&s)).await;
    let status = page.status().as_u16();
    let text = page.text().await.unwrap();
    (status, text, h.poll(&s).await)
}

// ---------------------------------------------------------------------------
// people-v1
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_migration_makes_the_owner_and_hangs_every_row_on_them() {
    let store = Store::memory().await.unwrap();
    let now = Utc::now();
    for (id, name) in [("d1", "Mac"), ("d2", "iPhone")] {
        let d = DeviceRow {
            id: id.into(),
            name: name.into(),
            token_hash: sha256_hex(&format!("token-{id}")),
            github_login: "Nkr1shna".into(),
            created_at: now,
            last_seen_at: None,
            person_id: None,
        };
        store.put(db::DEVICE, id, &d).await.unwrap();
    }
    for i in 1..=3 {
        let t = Topic {
            id: format!("00000000-0000-4000-8000-00000000000{i}"),
            parent_id: None,
            title: format!("T{i}"),
            position: i,
            owner: None,
            access: Vec::new(),
            deny: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        store.put(db::TOPIC, &t.id, &t).await.unwrap();
        let n = Note {
            id: format!("00000000-0000-4000-8000-00000000010{i}"),
            topic_id: t.id.clone(),
            lesson_id: None,
            title: "N".into(),
            body_md: "body".into(),
            quote: None,
            owner: None,
            access: Vec::new(),
            deny: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        store.put(db::NOTE, &n.id, &n).await.unwrap();
    }

    let h = start_with(Setup {
        store: Some(store.clone()),
        ..Setup::default()
    })
    .await;
    let people_rows: Vec<PersonRow> = store.list(db::PERSON).await.unwrap();
    assert_eq!(people_rows.len(), 1);
    let owner = &people_rows[0].person;
    assert_eq!(owner.org_role, OrgRole::Owner);
    assert_eq!(owner.handle, "nkr1shna");
    assert_eq!(owner.state, PersonState::Active);
    assert_eq!(
        people::groups_of(&h.state, &owner.id).await.unwrap(),
        vec!["company"]
    );
    assert!(
        store
            .get::<GroupRow>(db::GROUP, "company")
            .await
            .unwrap()
            .is_some()
    );
    let devices: Vec<DeviceRow> = store.list(db::DEVICE).await.unwrap();
    assert!(
        devices
            .iter()
            .all(|d| d.person_id.as_deref() == Some(owner.id.as_str()))
    );
    let private = vec![format!("person/{}", owner.id)];
    let topics: Vec<Topic> = store.list(db::TOPIC).await.unwrap();
    let notes: Vec<Note> = store.list(db::NOTE).await.unwrap();
    assert_eq!((topics.len(), notes.len()), (3, 3));
    for t in &topics {
        assert_eq!(t.owner.as_deref(), Some(owner.id.as_str()));
        assert_eq!(t.access, private);
    }
    for n in &notes {
        assert_eq!(n.owner.as_deref(), Some(owner.id.as_str()));
        assert_eq!(n.access, private);
        assert_eq!(n.body_md, "body");
    }
    let settings: SettingsRow = store.get(db::SETTINGS, SETTINGS_ID).await.unwrap().unwrap();
    assert!(settings.migrations.iter().any(|m| m == people::MIGRATION));

    // The migrated device still works, as the owner.
    let m = me(&h, "token-d1").await;
    assert_eq!(m.person.as_ref().unwrap().id, owner.id);
    assert_eq!(m.person.unwrap().groups, vec!["company"]);
    drop(h);

    // A second start changes nothing.
    let h2 = start_with(Setup {
        store: Some(store.clone()),
        ..Setup::default()
    })
    .await;
    let again: Vec<PersonRow> = store.list(db::PERSON).await.unwrap();
    assert_eq!(again, people_rows);
    let memberships: Vec<serde_json::Value> = store.list(db::MEMBERSHIP).await.unwrap();
    assert_eq!(memberships.len(), 1);
    assert_eq!(store.list::<Topic>(db::TOPIC).await.unwrap().len(), 3);
    let settings: SettingsRow = store.get(db::SETTINGS, SETTINGS_ID).await.unwrap().unwrap();
    assert_eq!(
        settings
            .migrations
            .iter()
            .filter(|m| *m == people::MIGRATION)
            .count(),
        1
    );

    // After the migration a device without a person is refused.
    let stray = DeviceRow {
        id: "d3".into(),
        name: "Old".into(),
        token_hash: sha256_hex("token-d3"),
        github_login: "Nkr1shna".into(),
        created_at: now,
        last_seen_at: None,
        person_id: None,
    };
    store.put(db::DEVICE, "d3", &stray).await.unwrap();
    assert_eq!(
        h2.get("token-d3", "/api/me").send().await.unwrap().status(),
        401
    );
}

// ---------------------------------------------------------------------------
// Admission and /api/me
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_owner_signs_in_as_a_person() {
    let h = start().await;
    let token = h.sign_in("Mac").await;
    let m = me(&h, &token).await;
    let person = m.person.unwrap();
    assert_eq!(person.org_role, OrgRole::Owner);
    assert_eq!(person.handle, "nkr1shna");
    assert_eq!(person.github_id, Some(1));
    assert_eq!(person.groups, vec!["company"]);
    let device = m.device.unwrap();
    assert_eq!(device.person_id.as_deref(), Some(person.id.as_str()));
    assert!(m.agent.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invitation_admits_a_second_person() {
    let h = start().await;
    let owner = h.sign_in("Mac").await;
    let owner_id = me(&h, &owner).await.person.unwrap().id;

    // Not invited yet.
    let (status, page, state) = try_sign_in(&h, BOB_CODE).await;
    assert_eq!(status, 403);
    assert!(page.contains("@bob-dev isn"));
    assert!(matches!(state, LoginState::Denied { reason } if reason.contains("invite")));

    // An unknown group is refused; then the invitation.
    let r = h
        .post(&owner, "/api/invitations")
        .json(&json!({"github_login": "bob-dev", "role": "member", "groups": ["company/engineering"]}))
        .send()
        .await
        .unwrap();
    let (status, err) = api_error(r).await;
    assert_eq!(
        (status, err.code.as_str()),
        (400, access_code::UNKNOWN_GROUP)
    );
    add_group(&h, "company/engineering").await;
    let inv = h
        .invite(
            &owner,
            "@Bob-Dev",
            OrgRole::Member,
            &["company/engineering"],
        )
        .await;
    assert_eq!(inv.github_login, "Bob-Dev");
    assert_eq!(inv.invited_by, owner_id);
    assert!(inv.expires_at > Utc::now() + Duration::days(29));

    // A second one while it's pending.
    let r = h
        .post(&owner, "/api/invitations")
        .json(&json!({"github_login": "bob-dev", "role": "member"}))
        .send()
        .await
        .unwrap();
    let (status, err) = api_error(r).await;
    assert_eq!(
        (status, err.code.as_str()),
        (409, access_code::ALREADY_INVITED)
    );

    // Bob signs in: a person now, the invitation accepted.
    let s = h.start_login("Bob's laptop").await;
    let page = h.callback(BOB_CODE, &Harness::state_param(&s)).await;
    assert_eq!(page.status(), 200);
    assert!(page.text().await.unwrap().contains("as @bob-dev."));
    let bob = match h.poll(&s).await {
        LoginState::Approved {
            token,
            github_login,
        } => {
            assert_eq!(github_login, "bob-dev");
            token
        }
        other => panic!("{other:?}"),
    };
    let m = me(&h, &bob).await;
    let person = m.person.unwrap();
    assert_eq!(person.org_role, OrgRole::Member);
    assert_eq!(person.handle, "bob-dev");
    assert_eq!(person.github_id, Some(3));
    assert_eq!(person.groups, vec!["company", "company/engineering"]);
    assert_eq!(m.github_login, "bob-dev");

    let list: Vec<Invitation> = h
        .get(&owner, "/api/invitations")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].accepted_by.as_deref(), Some(person.id.as_str()));

    // Inviting a member again.
    let r = h
        .post(&owner, "/api/invitations")
        .json(&json!({"github_login": "BOB-DEV", "role": "member"}))
        .send()
        .await
        .unwrap();
    let (status, err) = api_error(r).await;
    assert_eq!(
        (status, err.code.as_str()),
        (409, access_code::ALREADY_MEMBER)
    );

    // Signing in again is the same person.
    let bob2 = h.sign_in_as(BOB_CODE, "Bob's phone").await;
    assert_eq!(me(&h, &bob2).await.person.unwrap().id, person.id);
    let people: Vec<PersonRow> = h.state.store.list(db::PERSON).await.unwrap();
    assert_eq!(people.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn revoked_expired_and_renamed_logins_are_refused() {
    let h = start().await;
    let owner = h.sign_in("Mac").await;

    // Revoked.
    let inv = h.invite(&owner, "bob-dev", OrgRole::Member, &[]).await;
    let r = h
        .delete(&owner, &format!("/api/invitations/{}", inv.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = h
        .delete(&owner, &format!("/api/invitations/{}", inv.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let (status, _, _) = try_sign_in(&h, BOB_CODE).await;
    assert_eq!(status, 403);

    // Expired.
    let inv = h.invite(&owner, "bob-dev", OrgRole::Member, &[]).await;
    let mut row: InvitationRow = h
        .state
        .store
        .get(db::INVITATION, &inv.id)
        .await
        .unwrap()
        .unwrap();
    row.invitation.expires_at = Utc::now() - Duration::minutes(1);
    h.state
        .store
        .put(db::INVITATION, &inv.id, &row)
        .await
        .unwrap();
    let (status, _, _) = try_sign_in(&h, BOB_CODE).await;
    assert_eq!(status, 403);

    // An expired one can be sent again; then a renamed login handed to someone else.
    h.invite(&owner, "bob-dev", OrgRole::Member, &[]).await;
    let bob = h.sign_in_as(BOB_CODE, "Laptop").await;
    let bob_id = me(&h, &bob).await.person.unwrap().id;
    let mut row: PersonRow = h
        .state
        .store
        .get(db::PERSON, &bob_id)
        .await
        .unwrap()
        .unwrap();
    row.person.github_id = Some(99);
    h.state.store.put(db::PERSON, &bob_id, &row).await.unwrap();
    let (status, page, _) = try_sign_in(&h, BOB_CODE).await;
    assert_eq!(status, 403);
    assert!(page.contains("isn&#39;t the one that was invited"));
}

// ---------------------------------------------------------------------------
// Roles, people routes, settings
// ---------------------------------------------------------------------------

async fn owner_and_bob(h: &Harness) -> (String, Person, String, Person) {
    let owner = h.sign_in("Mac").await;
    h.invite(&owner, "bob-dev", OrgRole::Member, &[]).await;
    let bob = h.sign_in_as(BOB_CODE, "Bob's laptop").await;
    let o = me(h, &owner).await.person.unwrap();
    let b = me(h, &bob).await.person.unwrap();
    (owner, o, bob, b)
}

#[tokio::test(flavor = "multi_thread")]
async fn members_and_admins_change_what_their_role_allows() {
    let h = start().await;
    let (owner, o, bob, b) = owner_and_bob(&h).await;

    // A member: no invitations, no roles, nobody else's name.
    for r in [
        h.get(&bob, "/api/invitations").send().await.unwrap(),
        h.post(&bob, "/api/invitations")
            .json(&json!({"github_login": "carol", "role": "member"}))
            .send()
            .await
            .unwrap(),
        h.patch(&bob, &format!("/api/people/{}", b.id))
            .json(&json!({"org_role": "admin"}))
            .send()
            .await
            .unwrap(),
        h.put(&bob, &format!("/api/people/{}/groups", b.id))
            .json(&json!([]))
            .send()
            .await
            .unwrap(),
    ] {
        let (status, err) = api_error(r).await;
        assert_eq!(
            (status, err.code.as_str()),
            (403, access_code::ADMIN_REQUIRED)
        );
    }
    let r = h
        .patch(&bob, &format!("/api/people/{}", o.id))
        .json(&json!({"name": "Not you"}))
        .send()
        .await
        .unwrap();
    let (status, err) = api_error(r).await;
    assert_eq!((status, err.code.as_str()), (403, access_code::NOT_OWNER));

    // Own name and settings.
    let p: Person = h
        .patch(&bob, &format!("/api/people/{}", b.id))
        .json(&json!({"name": "  Bob  "}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(p.name, "Bob");
    let s: PersonSettings = h
        .patch(&bob, "/api/me/settings")
        .json(&json!({"default_provider": "claude", "reasoning_effort": "high"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s.default_provider, Some(Provider::Claude));
    assert_eq!(s.reasoning_effort, Some(ReasoningEffort::High));
    let s: PersonSettings = h
        .get(&bob, "/api/me/settings")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s.default_provider, Some(Provider::Claude));

    // The directory shows settings only on your own row.
    let dir: Vec<Person> = h
        .get(&owner, "/api/people")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(dir.len(), 2);
    let bob_seen = dir.iter().find(|p| p.id == b.id).unwrap();
    assert_eq!(bob_seen.settings, PersonSettings::default());
    assert_eq!(bob_seen.name, "Bob");
    let mine: Person = h
        .get(&bob, &format!("/api/people/{}", b.id))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mine.settings.default_provider, Some(Provider::Claude));
    assert_eq!(
        h.get(&bob, "/api/people/nobody")
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    // The owner makes Bob an admin; Bob can invite members but not admins, and can't
    // touch the owner.
    let p: Person = h
        .patch(&owner, &format!("/api/people/{}", b.id))
        .json(&json!({"org_role": "admin"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(p.org_role, OrgRole::Admin);
    h.invite(&bob, "carol-dev", OrgRole::Member, &[]).await;
    let r = h
        .post(&bob, "/api/invitations")
        .json(&json!({"github_login": "dave-dev", "role": "admin"}))
        .send()
        .await
        .unwrap();
    let (status, err) = api_error(r).await;
    assert_eq!(
        (status, err.code.as_str()),
        (403, access_code::ADMIN_REQUIRED)
    );
    let r = h
        .patch(&bob, &format!("/api/people/{}", o.id))
        .json(&json!({"state": "disabled"}))
        .send()
        .await
        .unwrap();
    let (status, err) = api_error(r).await;
    assert_eq!((status, err.code.as_str()), (400, access_code::OWNER_FIXED));
    let r = h
        .patch(&owner, &format!("/api/people/{}", o.id))
        .json(&json!({"org_role": "member"}))
        .send()
        .await
        .unwrap();
    let (status, err) = api_error(r).await;
    assert_eq!((status, err.code.as_str()), (400, access_code::OWNER_FIXED));
    // Removing an admin is the owner's too.
    let r = h
        .patch(&bob, &format!("/api/people/{}", b.id))
        .json(&json!({"org_role": "member"}))
        .send()
        .await
        .unwrap();
    let (status, _) = api_error(r).await;
    assert_eq!(status, 403);

    // Memberships, by an admin.
    add_group(&h, "company/engineering").await;
    let r = h
        .put(&bob, &format!("/api/people/{}/groups", b.id))
        .json(&json!(["company/nope"]))
        .send()
        .await
        .unwrap();
    let (status, err) = api_error(r).await;
    assert_eq!(
        (status, err.code.as_str()),
        (400, access_code::UNKNOWN_GROUP)
    );
    let p: Person = h
        .put(&bob, &format!("/api/people/{}/groups", b.id))
        .json(&json!(["company/engineering", "company"]))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(p.groups, vec!["company", "company/engineering"]);
    let p: Person = h
        .put(&owner, &format!("/api/people/{}/groups", b.id))
        .json(&json!([]))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(p.groups.is_empty());
}

// ---------------------------------------------------------------------------
// Devices per person, disabling
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn devices_belong_to_their_person() {
    let h = start().await;
    let (owner, o, bob, b) = owner_and_bob(&h).await;
    let owner_device = me(&h, &owner).await.device.unwrap().id;
    let bob_device = me(&h, &bob).await.device.unwrap().id;

    let mine: Vec<Device> = h
        .get(&bob, "/api/devices")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].person_id.as_deref(), Some(b.id.as_str()));
    let everyone: Vec<Device> = h
        .get(&owner, "/api/devices")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(everyone.len(), 2);
    assert!(
        everyone
            .iter()
            .any(|d| d.person_id.as_deref() == Some(o.id.as_str()))
    );

    // A member can't revoke someone else's device (404: not even that it exists).
    let r = h
        .delete(&bob, &format!("/api/devices/{owner_device}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    assert_eq!(h.get(&owner, "/api/me").send().await.unwrap().status(), 200);

    // An admin can.
    let bob2 = h.sign_in_as(BOB_CODE, "Bob's phone").await;
    let r = h
        .delete(&owner, &format!("/api/devices/{bob_device}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    assert_eq!(h.get(&bob, "/api/me").send().await.unwrap().status(), 401);
    assert_eq!(h.get(&bob2, "/api/me").send().await.unwrap().status(), 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn disabling_a_person_revokes_their_devices_and_admission() {
    let h = start().await;
    let (owner, _o, bob, b) = owner_and_bob(&h).await;
    let mut socket = h.socket(&bob).await;

    let r = h
        .patch(&bob, &format!("/api/people/{}", b.id))
        .json(&json!({"state": "disabled"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let p: Person = h
        .patch(&owner, &format!("/api/people/{}", b.id))
        .json(&json!({"state": "disabled"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(p.state, PersonState::Disabled);
    assert_eq!(h.get(&bob, "/api/me").send().await.unwrap().status(), 401);
    let devices: Vec<DeviceRow> = h
        .state
        .store
        .list_where(db::DEVICE, "person_id", &b.id)
        .await
        .unwrap();
    assert!(devices.is_empty());
    // The socket closes.
    let closed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if socket.next_event().await.is_none() {
                break;
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the disabled person's socket stayed open");

    let (status, page, _) = try_sign_in(&h, BOB_CODE).await;
    assert_eq!(status, 403);
    assert!(page.contains("is disabled on this Margin"));

    // Enabling lets them sign in again; their devices don't come back.
    let p: Person = h
        .patch(&owner, &format!("/api/people/{}", b.id))
        .json(&json!({"state": "active"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(p.state, PersonState::Active);
    assert_eq!(h.get(&bob, "/api/me").send().await.unwrap().status(), 401);
    let bob2 = h.sign_in_as(BOB_CODE, "Laptop again").await;
    assert_eq!(me(&h, &bob2).await.person.unwrap().id, b.id);
}
