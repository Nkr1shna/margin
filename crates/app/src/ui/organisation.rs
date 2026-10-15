//! Settings → Organisation (L1, `docs/l1-brief.md` D10): people, invitations by GitHub
//! login, the groups tree and agents with their tokens; Settings → Account's own
//! defaults ("My settings") and devices per person; and the organisation's settings as a
//! member sees them (read-only, with who changes them).

use chrono::Utc;
use dioxus::prelude::*;
use margin_shared::pipeline::Route;
use margin_shared::{
    Agent, AgentKind, AgentPatch, Device, Event, Group, GroupPatch, Id, Invitation, NewAgent,
    NewGroup, NewInvitation, OrgRole, Person, PersonPatch, PersonSettings, PersonState, Provider,
    ROOT_GROUP, ReasoningEffort, Settings, person_id,
};

use crate::api::Api;
use crate::org;
use crate::state::{AppCtx, use_app};
use crate::ui::common::{
    ConfirmDialog, ErrorNotice, Icon, ago, copy_to_clipboard, date, focus_first,
};
use crate::ui::settings::{effort_label, presence_text, shown_effort, slot_label};

/// What a member reads under each organisation setting they can't change.
pub const ADMIN_CHANGES_THESE: &str =
    "Only an organisation admin changes these; your own defaults are under Account.";

/// Who is signed in (`GET /api/me`, which may have changed: a role, groups) and the
/// organisation: people, groups, agents, and for an admin the invitations. A service
/// before L1 has none of these routes; the lists stay empty then. `Err` with the first
/// failure's message.
pub async fn load_org(ctx: AppCtx, api: &Api) -> Result<(), String> {
    match api.me().await {
        Ok(me) => {
            let mut session = ctx.session;
            if let Some(s) = session.write().as_mut() {
                s.me = Some(me);
            }
        }
        Err(e) => return Err(ctx.failure(&e)),
    }
    let (people, groups, agents) = tokio::join!(api.people(), api.groups(), api.agents());
    let mut lib = ctx.lib;
    let mut first_error = None;
    match people {
        Ok(list) => lib.write().replace_people(list),
        Err(e) => first_error = first_error.or(Some(ctx.failure(&e))),
    }
    match groups {
        Ok(list) => lib.write().replace_groups(list),
        Err(e) => first_error = first_error.or(Some(ctx.failure(&e))),
    }
    match agents {
        Ok(list) => lib.write().replace_agents(list),
        Err(e) => first_error = first_error.or(Some(ctx.failure(&e))),
    }
    let admin = ctx
        .session
        .peek()
        .as_ref()
        .and_then(|s| s.me.as_ref())
        .and_then(|m| m.person.as_ref())
        .is_some_and(|p| p.org_role.is_admin());
    if admin {
        match api.invitations().await {
            Ok(list) => lib.write().replace_invitations(list),
            Err(e) => first_error = first_error.or(Some(ctx.failure(&e))),
        }
    } else {
        lib.write().invitations.clear();
    }
    lib.write().org_loaded = first_error.is_none();
    match first_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// A person's row as the app holds it: in the store, and in the session when it is the
/// signed-in person (their own row carries their settings).
fn store_person(app: AppCtx, person: Person) {
    let mine = app
        .session
        .peek()
        .as_ref()
        .and_then(|s| s.me.as_ref())
        .and_then(|m| m.person.as_ref())
        .is_some_and(|p| p.id == person.id);
    let mut lib = app.lib;
    lib.write().upsert_person(person.clone());
    if mine {
        follow_own_person(app, person);
    }
}

/// The signed-in person's row after an event or an answer about them: their settings
/// aren't in every answer and never in an event (`org::merge_own_person`), so they're
/// fetched again when the row can't say: a reset on another device shows here too.
pub fn follow_own_person(app: AppCtx, person: Person) {
    let mut fetch = false;
    app.update_person(|me| {
        let (merged, unknown) = org::merge_own_person(me, person);
        *me = merged;
        fetch = unknown;
    });
    if fetch {
        spawn(refresh_my_settings(app));
    }
}

/// `GET /api/me/settings` into the session and the store.
pub async fn refresh_my_settings(app: AppCtx) {
    let Some(api) = app.api() else { return };
    match api.my_settings().await {
        Ok(settings) => set_my_settings(app, settings),
        Err(e) => {
            app.failure(&e);
        }
    }
}

/// The signed-in person's own settings, as the service holds them now.
pub fn set_my_settings(app: AppCtx, settings: PersonSettings) {
    let s2 = settings.clone();
    app.update_person(move |p| p.settings = s2);
    let me = app
        .session
        .peek()
        .as_ref()
        .and_then(|s| s.me.as_ref())
        .and_then(|m| m.person.as_ref())
        .map(|p| p.id.clone());
    let mut lib = app.lib;
    if let Some(id) = me
        && let Some(p) = lib.write().people.get_mut(&id)
    {
        p.settings = settings;
    }
}

/// "@bob", or the id when the person isn't known.
fn handle_of(people: &[Person], id: &str) -> String {
    people
        .iter()
        .find(|p| p.id == id)
        .map(|p| format!("@{}", p.handle))
        .unwrap_or_else(|| id.to_string())
}

/// A group's name, else its path.
fn group_name(groups: &[Group], path: &str) -> String {
    groups
        .iter()
        .find(|g| g.path == path)
        .map(|g| g.name.clone())
        .unwrap_or_else(|| path.to_string())
}

/// "Engineering, Design", or "No groups" (everyone is in Company).
pub fn groups_text(paths: &[String], groups: &[Group]) -> String {
    let named: Vec<String> = paths
        .iter()
        .filter(|p| p.as_str() != ROOT_GROUP)
        .map(|p| group_name(groups, p))
        .collect();
    if named.is_empty() {
        "Company only".into()
    } else {
        named.join(", ")
    }
}

/// A row of a checkbox tree.
#[derive(Clone, PartialEq, Debug)]
pub struct CheckRow {
    pub key: String,
    pub label: String,
    pub depth: usize,
    pub checked: bool,
    /// Something the owner no longer has: shown struck through, can only be unchecked.
    pub struck: bool,
    /// A short note after the label ("through Engineering").
    pub note: Option<String>,
}

/// The tree of groups with the `checked` ones ticked; groups above a ticked one say
/// they're included through it.
pub fn group_rows(groups: &[Group], checked: &[String]) -> Vec<CheckRow> {
    org::tree_order(groups)
        .into_iter()
        .map(|g| {
            let on = checked.contains(&g.path);
            let under = format!("{}/", g.path);
            let through = checked.iter().find(|c| c.starts_with(&under));
            CheckRow {
                key: g.path.clone(),
                label: format!("{} ({})", g.name, g.path),
                depth: org::depth(&g.path),
                checked: on,
                struck: false,
                note: (!on && through.is_some()).then(|| "included through a group below".into()),
            }
        })
        .collect()
}

/// The rows of an agent's ceiling over `me`'s own access: their private notes and chats,
/// then their groups as a tree; entries of `ceiling` they no longer have, struck through.
pub fn ceiling_rows(me: &Person, groups: &[Group], ceiling: &[String]) -> Vec<CheckRow> {
    let mine = org::expansion(me);
    let private = person_id(&me.id);
    let mut rows = vec![CheckRow {
        key: private.clone(),
        label: "My private notes and chats".into(),
        depth: 0,
        checked: ceiling.contains(&private),
        struck: false,
        note: None,
    }];
    let mut paths: Vec<Group> = mine
        .iter()
        .filter(|p| **p != private)
        .map(|p| {
            groups
                .iter()
                .find(|g| g.path == *p)
                .cloned()
                .unwrap_or(Group {
                    path: p.clone(),
                    name: p.rsplit('/').next().unwrap_or(p).to_string(),
                    source: margin_shared::SOURCE_MANUAL.into(),
                    members: 0,
                    created_at: Utc::now(),
                })
        })
        .collect();
    paths = org::tree_order(&paths);
    for g in paths {
        rows.push(CheckRow {
            key: g.path.clone(),
            label: format!("{} ({})", g.name, g.path),
            depth: org::depth(&g.path),
            checked: ceiling.contains(&g.path),
            struck: false,
            note: None,
        });
    }
    for stale in ceiling.iter().filter(|c| !mine.contains(c)) {
        rows.push(CheckRow {
            key: stale.clone(),
            label: stale.clone(),
            depth: 0,
            checked: true,
            struck: true,
            note: Some("you no longer have this".into()),
        });
    }
    rows
}

/// A tree of checkboxes. Toggling one calls `on_toggle(key, checked)`.
#[component]
pub fn CheckTree(
    id: String,
    label: String,
    rows: Vec<CheckRow>,
    on_toggle: EventHandler<(String, bool)>,
) -> Element {
    rsx! {
        fieldset { class: "check-tree", id: "{id}",
            legend { class: "label", "{label}" }
            for row in rows {
                label {
                    key: "{row.key}",
                    class: if row.struck { "check-row struck" } else { "check-row" },
                    style: "padding-left: {row.depth * 20}px",
                    input {
                        r#type: "checkbox",
                        checked: row.checked,
                        // A struck entry can only go.
                        disabled: row.struck && !row.checked,
                        onchange: {
                            let key = row.key.clone();
                            move |e: FormEvent| on_toggle.call((key.clone(), e.checked()))
                        },
                    }
                    span { class: "check-label", "{row.label}" }
                    if let Some(note) = row.note.clone() {
                        small { class: "faint", "{note}" }
                    }
                }
            }
        }
    }
}

/// A form dialog over Settings: Esc or a click outside closes it (not while busy).
#[component]
pub fn FormDialog(
    title: String,
    #[props(default)] busy: bool,
    on_close: EventHandler<()>,
    children: Element,
) -> Element {
    let close = move || {
        if !busy {
            on_close.call(());
        }
    };
    rsx! {
        div {
            class: "modal-scrim",
            onclick: move |_| close(),
            onkeydown: move |e: KeyboardEvent| {
                if e.key() == Key::Escape {
                    e.stop_propagation();
                    close();
                }
            },
            div {
                class: "modal org-dialog",
                role: "dialog",
                "aria-modal": "true",
                "aria-label": "{title}",
                "aria-busy": busy.to_string(),
                onclick: move |e| e.stop_propagation(),
                h2 { "{title}" }
                {children}
            }
        }
    }
}

/// Settings → Organisation.
#[component]
pub fn OrganisationPane() -> Element {
    let app = use_app();
    // Fetched again each time the pane opens; events keep it current from then on.
    let mut load = use_resource(move || async move {
        let api = app.api().ok_or_else(|| "Not signed in.".to_string())?;
        load_org(app, &api).await
    });
    let failed = match &*load.read() {
        Some(Err(e)) => Some(e.clone()),
        _ => None,
    };
    // "Loading…" only while the load runs: once it failed, the notice above says so.
    let loading = load.read().is_none();
    rsx! {
        if let Some(err) = failed {
            section { class: "set-sec",
                ErrorNotice { message: err, on_retry: move |_| load.restart() }
            }
        }
        PeopleSection { loading }
        InvitationsSection {}
        GroupsSection { loading }
        AgentsSection {}
        crate::ui::audit::AuditSection {}
    }
}

// ---------------------------------------------------------------------------
// People
// ---------------------------------------------------------------------------

#[component]
fn PeopleSection(loading: bool) -> Element {
    let app = use_app();
    let me = app.person();
    let my_role = me.as_ref().map(|p| p.org_role).unwrap_or(OrgRole::Member);
    let admin = my_role.is_admin();
    let lib = app.lib.read();
    let people: Vec<Person> = lib.people_in_order().into_iter().cloned().collect();
    let groups: Vec<Group> = lib.groups.values().cloned().collect();
    drop(lib);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| None::<Id>);
    let mut confirm_disable = use_signal(|| None::<Person>);
    let mut confirm_role = use_signal(|| None::<(Person, OrgRole)>);
    let mut editing_groups = use_signal(|| None::<Person>);

    let patch = move |id: Id, patch: PersonPatch| async move {
        let Some(api) = app.api() else { return false };
        busy.set(Some(id.clone()));
        let ok = match api.patch_person(&id, &patch).await {
            Ok(person) => {
                error.set(None);
                store_person(app, person);
                true
            }
            Err(e) => {
                error.set(Some(app.failure(&e)));
                false
            }
        };
        busy.set(None);
        ok
    };

    rsx! {
        section { class: "set-sec",
            h2 { "People" }
            p { class: "help",
                if admin {
                    "Everyone who can sign in to this Margin. Each person's notes and chats are their own; admins manage who belongs, not what they read."
                } else {
                    "Everyone who can sign in to this Margin. Each person's notes and chats are their own."
                }
            }
            if let Some(err) = error.read().clone() {
                ErrorNotice { message: err }
            }
            if loading && people.is_empty() {
                p { class: "faint", "Loading people…" }
            }
            div { class: "org-list", role: "list",
                for p in people {
                    div { key: "{p.id}", class: "org-row", role: "listitem",
                        span { class: "avatar", "aria-hidden": "true",
                            {p.handle.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default()}
                        }
                        div { class: "grow",
                            div { class: "org-title",
                                b { "@{p.handle}" }
                                span { class: "faint", "{p.name}" }
                                span { class: "badge", {org::role_label(p.org_role)} }
                                if p.state != PersonState::Active {
                                    span { class: "badge bad", {org::state_label(p.state)} }
                                }
                                if me.as_ref().is_some_and(|m| m.id == p.id) {
                                    span { class: "badge", "You" }
                                }
                            }
                            small { "{groups_text(&p.groups, &groups)}" }
                        }
                        if admin {
                            div { class: "row-actions org-actions",
                                // Only the owner makes or removes admins; the owner's own
                                // role and state are fixed.
                                if my_role == OrgRole::Owner && p.org_role == OrgRole::Member && p.state == PersonState::Active {
                                    button {
                                        class: "btn btn-secondary btn-sm",
                                        disabled: busy.read().is_some(),
                                        onclick: {
                                            let p = p.clone();
                                            move |_| confirm_role.set(Some((p.clone(), OrgRole::Admin)))
                                        },
                                        "Make admin…"
                                    }
                                }
                                if my_role == OrgRole::Owner && p.org_role == OrgRole::Admin {
                                    button {
                                        class: "btn btn-secondary btn-sm",
                                        disabled: busy.read().is_some(),
                                        onclick: {
                                            let p = p.clone();
                                            move |_| confirm_role.set(Some((p.clone(), OrgRole::Member)))
                                        },
                                        "Remove admin…"
                                    }
                                }
                                button {
                                    class: "btn btn-secondary btn-sm",
                                    onclick: {
                                        let p = p.clone();
                                        move |_| editing_groups.set(Some(p.clone()))
                                    },
                                    "Edit groups…"
                                }
                                if p.org_role != OrgRole::Owner && me.as_ref().is_none_or(|m| m.id != p.id) {
                                    if p.state == PersonState::Disabled {
                                        button {
                                            class: "btn btn-secondary btn-sm",
                                            disabled: busy.read().is_some(),
                                            onclick: {
                                                let id = p.id.clone();
                                                move |_| {
                                                    let id = id.clone();
                                                    async move {
                                                        patch(id, PersonPatch { state: Some(PersonState::Active), ..Default::default() }).await;
                                                    }
                                                }
                                            },
                                            if busy.read().as_deref() == Some(p.id.as_str()) { span { class: "spinner" } }
                                            "Enable"
                                        }
                                    } else {
                                        button {
                                            class: "btn btn-secondary btn-sm",
                                            disabled: busy.read().is_some(),
                                            onclick: {
                                                let p = p.clone();
                                                move |_| confirm_disable.set(Some(p.clone()))
                                            },
                                            "Disable…"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(p) = confirm_disable.read().clone() {
            ConfirmDialog {
                title: "Disable @{p.handle}?",
                message: "Their devices are signed out now and their agents' tokens stop working. Their notes and chats stay theirs. Enable them to let them sign in again.",
                confirm_label: "Disable",
                busy: busy.read().is_some(),
                error: error.read().clone(),
                on_cancel: move |_| confirm_disable.set(None),
                on_confirm: {
                    let id = p.id.clone();
                    move |_| {
                        let id = id.clone();
                        async move {
                            if patch(id, PersonPatch { state: Some(PersonState::Disabled), ..Default::default() }).await {
                                confirm_disable.set(None);
                            }
                        }
                    }
                },
            }
        }
        if let Some((p, role)) = confirm_role.read().clone() {
            ConfirmDialog {
                title: if role == OrgRole::Admin { format!("Make @{} an admin?", p.handle) } else { format!("Remove @{} as admin?", p.handle) },
                message: if role == OrgRole::Admin {
                    "Admins invite and disable people, manage groups and change the organisation's settings. They can't read anyone's private notes or chats."
                } else {
                    "They stay a member, with the same groups."
                },
                confirm_label: if role == OrgRole::Admin { "Make admin" } else { "Remove admin" },
                danger: false,
                busy: busy.read().is_some(),
                error: error.read().clone(),
                on_cancel: move |_| confirm_role.set(None),
                on_confirm: {
                    let id = p.id.clone();
                    move |_| {
                        let id = id.clone();
                        async move {
                            if patch(id, PersonPatch { org_role: Some(role), ..Default::default() }).await {
                                confirm_role.set(None);
                            }
                        }
                    }
                },
            }
        }
        if let Some(p) = editing_groups.read().clone() {
            EditGroupsDialog { person: p, on_close: move |_| editing_groups.set(None) }
        }
    }
}

/// A person's direct groups, as a checkbox tree (admins).
#[component]
fn EditGroupsDialog(person: Person, on_close: EventHandler<()>) -> Element {
    let app = use_app();
    let mut checked = use_signal(|| person.groups.clone());
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    // Everyone is in Company (the service keeps the person in it): not a choice here.
    let groups: Vec<Group> = app
        .lib
        .read()
        .groups
        .values()
        .filter(|g| g.path != margin_shared::ROOT_GROUP)
        .cloned()
        .collect();
    let rows = group_rows(&groups, &checked.read());
    let id = person.id.clone();
    rsx! {
        FormDialog {
            title: "Groups of @{person.handle}",
            busy: *busy.read(),
            on_close: move |_| on_close.call(()),
            p { class: "help", "Everyone is in Company. A group includes the groups above it: someone in Engineering also reads what Company shares." }
            CheckTree {
                id: "edit-groups",
                label: "Groups",
                rows,
                on_toggle: move |(path, on): (String, bool)| {
                    let mut next = checked.peek().clone();
                    next.retain(|p| *p != path);
                    if on {
                        next.push(path);
                    }
                    checked.set(next);
                },
            }
            if let Some(err) = error.read().clone() {
                div { class: "notice error", role: "alert", Icon { name: "alert" } div { class: "body", "{err}" } }
            }
            div { class: "row-actions",
                button { class: "btn btn-secondary btn-sm", disabled: *busy.read(), onclick: move |_| on_close.call(()), "Cancel" }
                button {
                    class: "btn btn-primary btn-sm",
                    disabled: *busy.read(),
                    onclick: move |_| {
                        let id = id.clone();
                        async move {
                            let Some(api) = app.api() else { return };
                            busy.set(true);
                            let mut list = checked.peek().clone();
                            if !list.iter().any(|g| g == margin_shared::ROOT_GROUP) {
                                list.push(margin_shared::ROOT_GROUP.to_string());
                            }
                            match api.put_person_groups(&id, &list).await {
                                Ok(person) => {
                                    let mut lib = app.lib;
                                    lib.write().apply(Event::MembershipChanged { person_id: person.id.clone(), groups: person.groups.clone() });
                                    store_person(app, person);
                                    busy.set(false);
                                    on_close.call(());
                                }
                                Err(e) => {
                                    error.set(Some(app.failure(&e)));
                                    busy.set(false);
                                }
                            }
                        }
                    },
                    if *busy.read() { span { class: "spinner" } }
                    "Save groups"
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Invitations
// ---------------------------------------------------------------------------

#[component]
fn InvitationsSection() -> Element {
    let app = use_app();
    let admin = app.is_admin();
    let lib = app.lib.read();
    let invitations: Vec<Invitation> = lib.invitations_in_order().into_iter().cloned().collect();
    let people: Vec<Person> = lib.people.values().cloned().collect();
    let groups: Vec<Group> = lib.groups.values().cloned().collect();
    drop(lib);
    let mut inviting = use_signal(|| false);
    let mut confirm_revoke = use_signal(|| None::<Invitation>);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let now = Utc::now();
    let (open, closed): (Vec<Invitation>, Vec<Invitation>) = invitations
        .into_iter()
        .partition(|i| i.accepted_at.is_none());

    if !admin {
        return rsx! {
            section { class: "set-sec",
                h2 { "Invitations" }
                p { class: "help", "Ask an admin to invite people." }
            }
        };
    }

    rsx! {
        section { class: "set-sec",
            div { class: "set-sec-head",
                h2 { "Invitations" }
                button { class: "btn btn-secondary btn-sm", onclick: move |_| inviting.set(true), Icon { name: "plus" } "Invite…" }
            }
            p { class: "help", "Invite someone by their GitHub login. They become a member when they first sign in with it; an invitation stays open for {margin_shared::INVITATION_DAYS} days." }
            if let Some(err) = error.read().clone() {
                ErrorNotice { message: err }
            }
            if open.is_empty() {
                p { class: "faint", "No open invitations." }
            }
            div { class: "org-list", role: "list",
                for i in open {
                    div { key: "{i.id}", class: "org-row", role: "listitem",
                        div { class: "grow",
                            div { class: "org-title",
                                b { "@{i.github_login}" }
                                span { class: "badge", {org::role_label(i.role)} }
                                if !org::is_pending(&i, now) {
                                    span { class: "badge bad", "Expired" }
                                }
                            }
                            small {
                                "{groups_text(&i.groups, &groups)} · invited by {handle_of(&people, &i.invited_by)} · "
                                if org::is_pending(&i, now) {
                                    "expires {date(&i.expires_at)}"
                                } else {
                                    "expired {date(&i.expires_at)}; invite them again to send a new one"
                                }
                            }
                        }
                        if org::is_pending(&i, now) {
                            button {
                                class: "btn btn-secondary btn-sm",
                                onclick: {
                                    let i = i.clone();
                                    move |_| confirm_revoke.set(Some(i.clone()))
                                },
                                "Revoke…"
                            }
                        }
                    }
                }
            }
            if !closed.is_empty() {
                details { class: "org-accepted",
                    summary { "Accepted ({closed.len()})" }
                    div { class: "org-list", role: "list",
                        for i in closed {
                            div { key: "{i.id}", class: "org-row", role: "listitem",
                                div { class: "grow",
                                    div { class: "org-title",
                                        b { "@{i.github_login}" }
                                        span { class: "badge", {org::role_label(i.role)} }
                                    }
                                    small {
                                        "Accepted {i.accepted_at.as_ref().map(date).unwrap_or_default()}"
                                        if let Some(by) = &i.accepted_by { " as {handle_of(&people, by)}" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if *inviting.read() {
            InviteDialog { on_close: move |_| inviting.set(false) }
        }
        if let Some(i) = confirm_revoke.read().clone() {
            ConfirmDialog {
                title: "Revoke the invitation of @{i.github_login}?",
                message: "They won't be able to sign in with it. You can invite them again later.",
                confirm_label: "Revoke",
                busy: *busy.read(),
                error: error.read().clone(),
                on_cancel: move |_| confirm_revoke.set(None),
                on_confirm: {
                    let id = i.id.clone();
                    move |_| {
                        let id = id.clone();
                        async move {
                            let Some(api) = app.api() else { return };
                            busy.set(true);
                            match api.delete_invitation(&id).await {
                                Ok(()) => {
                                    error.set(None);
                                    let mut lib = app.lib;
                                    lib.write().apply(Event::InvitationDeleted { id });
                                    confirm_revoke.set(None);
                                }
                                Err(e) => error.set(Some(app.failure(&e))),
                            }
                            busy.set(false);
                        }
                    }
                },
            }
        }
    }
}

/// "Invite…": a GitHub login, a role (Admin only for the owner), groups.
#[component]
fn InviteDialog(on_close: EventHandler<()>) -> Element {
    let app = use_app();
    let owner = app.person().is_some_and(|p| p.org_role == OrgRole::Owner);
    let mut login = use_signal(String::new);
    let mut role = use_signal(|| OrgRole::Member);
    let mut checked = use_signal(Vec::<String>::new);
    let mut problem = use_signal(|| None::<String>);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let lib = app.lib.read();
    let groups: Vec<Group> = lib.groups.values().cloned().collect();
    drop(lib);
    let rows = group_rows(
        &groups
            .iter()
            .filter(|g| g.path != ROOT_GROUP)
            .cloned()
            .collect::<Vec<_>>(),
        &checked.read(),
    );
    let roles: &[OrgRole] = if owner {
        &[OrgRole::Member, OrgRole::Admin]
    } else {
        &[OrgRole::Member]
    };
    rsx! {
        FormDialog {
            title: "Invite someone",
            busy: *busy.read(),
            on_close: move |_| on_close.call(()),
            form {
                class: "org-form",
                novalidate: true,
                onsubmit: move |e: FormEvent| async move {
                    e.prevent_default();
                    if *busy.peek() {
                        return;
                    }
                    let lib = app.lib.peek();
                    let people: Vec<Person> = lib.people.values().cloned().collect();
                    let invitations: Vec<Invitation> = lib.invitations.values().cloned().collect();
                    drop(lib);
                    let typed = login.peek().clone();
                    if let Some(p) = org::invitation_problem(&typed, &people, &invitations, Utc::now()) {
                        problem.set(Some(p));
                        focus_first("#invite-login");
                        return;
                    }
                    problem.set(None);
                    let Some(api) = app.api() else { return };
                    busy.set(true);
                    let body = NewInvitation {
                        github_login: typed.trim().trim_start_matches('@').to_string(),
                        role: *role.peek(),
                        groups: checked.peek().clone(),
                    };
                    match api.create_invitation(&body).await {
                        Ok(invitation) => {
                            let mut lib = app.lib;
                            lib.write().apply(Event::InvitationChanged { invitation });
                            busy.set(false);
                            on_close.call(());
                        }
                        Err(e) => {
                            error.set(Some(app.failure(&e)));
                            busy.set(false);
                        }
                    }
                },
                div { class: "field",
                    label { r#for: "invite-login", "GitHub login" }
                    input {
                        id: "invite-login",
                        class: "input",
                        autocomplete: "off",
                        spellcheck: "false",
                        placeholder: "octocat",
                        "aria-invalid": problem.read().is_some().to_string(),
                        "aria-describedby": problem.read().is_some().then_some("invite-login-err"),
                        onmounted: move |e| async move { let _ = e.set_focus(true).await; },
                        oninput: move |e| {
                            problem.set(None);
                            login.set(e.value());
                        },
                    }
                    if let Some(p) = problem.read().clone() {
                        span { id: "invite-login-err", class: "field-error", role: "alert", "{p}" }
                    }
                }
                div { class: "field",
                    span { class: "label", id: "invite-role-label", "Role" }
                    div { class: "seg", role: "radiogroup", "aria-labelledby": "invite-role-label",
                        for r in roles.iter().copied() {
                            button {
                                key: "{r:?}",
                                r#type: "button",
                                role: "radio",
                                "aria-checked": (r == *role.read()).to_string(),
                                tabindex: if r == *role.read() { "0" } else { "-1" },
                                onclick: move |_| role.set(r),
                                {org::role_label(r)}
                            }
                        }
                    }
                    if !owner {
                        span { class: "faint", style: "font-size: 13px", "Only the organisation owner invites admins." }
                    }
                }
                if rows.is_empty() {
                    p { class: "faint", "They'll be in Company. Add groups under Groups to put them in more." }
                } else {
                    CheckTree {
                        id: "invite-groups",
                        label: "Groups (they're always in Company)",
                        rows,
                        on_toggle: move |(path, on): (String, bool)| {
                            let mut next = checked.peek().clone();
                            next.retain(|p| *p != path);
                            if on {
                                next.push(path);
                            }
                            checked.set(next);
                        },
                    }
                }
                if let Some(err) = error.read().clone() {
                    div { class: "notice error", role: "alert", Icon { name: "alert" } div { class: "body", "{err}" } }
                }
                div { class: "row-actions",
                    button { r#type: "button", class: "btn btn-secondary btn-sm", disabled: *busy.read(), onclick: move |_| on_close.call(()), "Cancel" }
                    button {
                        r#type: "submit",
                        class: "btn btn-primary btn-sm",
                        disabled: *busy.read(),
                        if *busy.read() { span { class: "spinner" } }
                        "Send invitation"
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Debug)]
enum GroupEdit {
    Add,
    Rename(Group),
}

/// Who is in a group: "Everyone (2 people)" for the root, else its direct members.
pub fn members_text(group: &Group, people: usize) -> String {
    match (group.path == ROOT_GROUP, group.members, people) {
        (true, _, 1) => "Everyone (1 person)".into(),
        (true, _, n) => format!("Everyone ({n} people)"),
        (false, 1, _) => "1 member".into(),
        (false, n, _) => format!("{n} members"),
    }
}

#[component]
fn GroupsSection(loading: bool) -> Element {
    let app = use_app();
    let admin = app.is_admin();
    let groups: Vec<Group> = app.lib.read().groups.values().cloned().collect();
    let everyone = app.lib.read().people.len();
    let tree = org::tree_order(&groups);
    let mut editing = use_signal(|| None::<GroupEdit>);
    let mut confirm_delete = use_signal(|| None::<Group>);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);

    rsx! {
        section { class: "set-sec",
            div { class: "set-sec-head",
                h2 { "Groups" }
                if admin {
                    button { class: "btn btn-secondary btn-sm", onclick: move |_| editing.set(Some(GroupEdit::Add)), Icon { name: "plus" } "Add group…" }
                }
            }
            p { class: "help", "Groups are who a note or chat can be shared with. A group includes the groups under it: sharing with Company reaches everyone." }
            if let Some(err) = error.read().clone() {
                ErrorNotice { message: err }
            }
            if loading && tree.is_empty() {
                p { class: "faint", "Loading groups…" }
            }
            div { class: "org-list", role: "tree", "aria-label": "Groups",
                for g in tree.iter().cloned() {
                    div {
                        key: "{g.path}",
                        class: "org-row",
                        role: "treeitem",
                        "aria-level": "{org::depth(&g.path) + 1}",
                        style: "padding-left: {org::depth(&g.path) * 20}px",
                        div { class: "grow",
                            div { class: "org-title",
                                b { "{g.name}" }
                                span { class: "mono faint", "{g.path}" }
                            }
                            small {
                                {members_text(&g, everyone)}
                                if g.source != margin_shared::SOURCE_MANUAL { " · from {g.source}" }
                            }
                        }
                        if admin {
                            div { class: "row-actions org-actions",
                                if g.source == margin_shared::SOURCE_MANUAL {
                                    button {
                                        class: "btn btn-secondary btn-sm",
                                        onclick: {
                                            let g = g.clone();
                                            move |_| editing.set(Some(GroupEdit::Rename(g.clone())))
                                        },
                                        "Rename…"
                                    }
                                }
                                match org::delete_blocker(&g, &groups) {
                                    // Not a dead button: why it can't go yet.
                                    Some(reason) => rsx! { small { class: "faint org-reason", "{reason}" } },
                                    None => rsx! {
                                        button {
                                            class: "btn btn-secondary btn-sm",
                                            onclick: {
                                                let g = g.clone();
                                                move |_| confirm_delete.set(Some(g.clone()))
                                            },
                                            "Delete…"
                                        }
                                    },
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(edit) = editing.read().clone() {
            GroupDialog { edit, on_close: move |_| editing.set(None) }
        }
        if let Some(g) = confirm_delete.read().clone() {
            ConfirmDialog {
                title: "Delete the group “{g.name}”?",
                message: "Nothing shared with {g.path} is deleted, but no one reads it through this group any more.",
                confirm_label: "Delete group",
                busy: *busy.read(),
                error: error.read().clone(),
                on_cancel: move |_| confirm_delete.set(None),
                on_confirm: {
                    let path = g.path.clone();
                    move |_| {
                        let path = path.clone();
                        async move {
                            let Some(api) = app.api() else { return };
                            busy.set(true);
                            match api.delete_group(&path).await {
                                Ok(()) => {
                                    error.set(None);
                                    let mut lib = app.lib;
                                    lib.write().apply(Event::GroupDeleted { path });
                                    confirm_delete.set(None);
                                }
                                Err(e) => error.set(Some(app.failure(&e))),
                            }
                            busy.set(false);
                        }
                    }
                },
            }
        }
    }
}

/// "Add group…" (a parent, a slug, a name; the full path shown) or "Rename…".
#[component]
fn GroupDialog(edit: GroupEdit, on_close: EventHandler<()>) -> Element {
    let app = use_app();
    let groups: Vec<Group> = app.lib.read().groups.values().cloned().collect();
    let tree = org::tree_order(&groups);
    let renaming = match &edit {
        GroupEdit::Rename(g) => Some(g.clone()),
        GroupEdit::Add => None,
    };
    let mut parent = use_signal(|| ROOT_GROUP.to_string());
    let mut slug = use_signal(String::new);
    let mut name = use_signal({
        let start = renaming
            .as_ref()
            .map(|g| g.name.clone())
            .unwrap_or_default();
        move || start
    });
    let mut problem = use_signal(|| None::<String>);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let preview = org::new_group_path(&parent.read(), &slug.read(), &groups);
    let title = match &renaming {
        Some(g) => format!("Rename “{}”", g.name),
        None => "Add a group".to_string(),
    };

    rsx! {
        FormDialog {
            title,
            busy: *busy.read(),
            on_close: move |_| on_close.call(()),
            form {
                class: "org-form",
                novalidate: true,
                onsubmit: {
                    let renaming = renaming.clone();
                    move |e: FormEvent| {
                        let renaming = renaming.clone();
                        async move {
                            e.prevent_default();
                            if *busy.peek() {
                                return;
                            }
                            let n = name.peek().trim().to_string();
                            if n.is_empty() {
                                problem.set(Some("Give the group a name.".into()));
                                focus_first("#group-name");
                                return;
                            }
                            let Some(api) = app.api() else { return };
                            let result = match &renaming {
                                Some(g) => {
                                    busy.set(true);
                                    api.patch_group(&g.path, &GroupPatch { name: Some(n) }).await
                                }
                                None => {
                                    let groups: Vec<Group> = app.lib.peek().groups.values().cloned().collect();
                                    let path = match org::new_group_path(&parent.peek(), &slug.peek(), &groups) {
                                        Ok(p) => p,
                                        Err(p) => {
                                            problem.set(Some(p));
                                            focus_first("#group-slug");
                                            return;
                                        }
                                    };
                                    busy.set(true);
                                    api.create_group(&NewGroup { path, name: n }).await
                                }
                            };
                            match result {
                                Ok(group) => {
                                    let mut lib = app.lib;
                                    lib.write().apply(Event::GroupChanged { group });
                                    busy.set(false);
                                    on_close.call(());
                                }
                                Err(e) => {
                                    error.set(Some(app.failure(&e)));
                                    busy.set(false);
                                }
                            }
                        }
                    }
                },
                if renaming.is_none() {
                    div { class: "field",
                        label { r#for: "group-parent", "Under" }
                        select {
                            id: "group-parent",
                            class: "select-input",
                            onchange: move |e: FormEvent| {
                                problem.set(None);
                                parent.set(e.value());
                            },
                            for g in tree.iter() {
                                option {
                                    key: "{g.path}",
                                    value: "{g.path}",
                                    selected: g.path == *parent.read(),
                                    {format!("{}{} ({})", "  ".repeat(org::depth(&g.path)), g.name, g.path)}
                                }
                            }
                        }
                    }
                    div { class: "field",
                        label { r#for: "group-slug", "Path" }
                        input {
                            id: "group-slug",
                            class: "input mono",
                            autocomplete: "off",
                            spellcheck: "false",
                            placeholder: "design",
                            "aria-describedby": "group-path-preview",
                            onmounted: move |e| async move { let _ = e.set_focus(true).await; },
                            oninput: move |e| {
                                problem.set(None);
                                slug.set(e.value());
                            },
                        }
                        span { id: "group-path-preview", class: "faint", style: "font-size: 13px",
                            match &preview {
                                Ok(path) => rsx! { "The group's path: " span { class: "mono", "{path}" } },
                                Err(_) if slug.read().trim().is_empty() => rsx! { "Lowercase letters, digits and dashes; it can't change later." },
                                Err(p) => rsx! { "{p}" },
                            }
                        }
                    }
                }
                div { class: "field",
                    label { r#for: "group-name", "Name" }
                    input {
                        id: "group-name",
                        class: "input",
                        initial_value: "{name}",
                        placeholder: "Design",
                        onmounted: move |e| {
                            let focus = renaming.is_some();
                            async move {
                                if focus {
                                    let _ = e.set_focus(true).await;
                                }
                            }
                        },
                        oninput: move |e| {
                            problem.set(None);
                            name.set(e.value());
                        },
                    }
                }
                if let Some(p) = problem.read().clone() {
                    span { class: "field-error", role: "alert", "{p}" }
                }
                if let Some(err) = error.read().clone() {
                    div { class: "notice error", role: "alert", Icon { name: "alert" } div { class: "body", "{err}" } }
                }
                div { class: "row-actions",
                    button { r#type: "button", class: "btn btn-secondary btn-sm", disabled: *busy.read(), onclick: move |_| on_close.call(()), "Cancel" }
                    button {
                        r#type: "submit",
                        class: "btn btn-primary btn-sm",
                        disabled: *busy.read(),
                        if *busy.read() { span { class: "spinner" } }
                        if matches!(edit, GroupEdit::Add) { "Add group" } else { "Rename" }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Agents
// ---------------------------------------------------------------------------

/// What an agent's dialog edits.
#[derive(Clone, PartialEq, Debug)]
enum AgentEdit {
    Create,
    Rename(Agent),
    Ceiling(Agent),
}

/// A confirmation an agent's action waits on.
#[derive(Clone, PartialEq, Debug)]
enum AgentConfirm {
    Disable(Agent),
    Delete(Agent),
    Rotate(Agent),
    Revoke(Agent),
}

/// The ceiling in words: "My private notes and chats, Company, Engineering".
fn ceiling_text(agent: &Agent, me: Option<&Person>, groups: &[Group], people: &[Person]) -> String {
    let owner = me
        .filter(|m| m.id == agent.owner_id)
        .map(|m| m.id.clone())
        .unwrap_or_default();
    agent
        .ceiling
        .iter()
        .map(|p| {
            if p.starts_with(margin_shared::PERSON_PREFIX) && owner.is_empty() {
                if *p == person_id(&agent.owner_id) {
                    format!(
                        "{}'s private notes and chats",
                        handle_of(people, &agent.owner_id)
                    )
                } else {
                    org::path_label(p, "", groups, people)
                }
            } else {
                match groups.iter().find(|g| g.path == *p) {
                    Some(g) => g.name.clone(),
                    None => org::path_label(p, &owner, groups, people),
                }
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// "Token mga_7f3a… · made Oct 9, 2025 · last used 2 h ago", or that there is none.
fn token_text(agent: &Agent) -> String {
    match &agent.token {
        Some(t) => format!(
            "Token {}… · made {} · {}",
            t.prefix,
            date(&t.created_at),
            match &t.last_used_at {
                Some(at) => format!("last used {}", ago(at)),
                None => "never used".into(),
            }
        ),
        None => "No token: make one to let the agent connect.".into(),
    }
}

#[component]
fn AgentsSection() -> Element {
    let app = use_app();
    let me = app.person();
    let admin = app.is_admin();
    let lib = app.lib.read();
    let groups: Vec<Group> = lib.groups.values().cloned().collect();
    let people: Vec<Person> = lib.people.values().cloned().collect();
    let mine: Vec<Agent> = me
        .as_ref()
        .map(|m| lib.agents_of(&m.id).into_iter().cloned().collect())
        .unwrap_or_default();
    let mut others: Vec<Agent> = lib
        .agents
        .values()
        .filter(|a| me.as_ref().is_some_and(|m| a.owner_id != m.id))
        .cloned()
        .collect();
    drop(lib);
    others.sort_by(|a, b| {
        handle_of(&people, &a.owner_id)
            .cmp(&handle_of(&people, &b.owner_id))
            .then(a.name.cmp(&b.name))
    });
    // The documents agents ingested, for each agent's "Documents (n)": the change feed,
    // once a session (events keep it current).
    let mut documents = use_resource(move || async move {
        if app.lib.peek().documents_loaded {
            return Ok(());
        }
        let api = app.api().ok_or_else(|| "Not signed in.".to_string())?;
        let docs = api.agent_documents().await.map_err(|e| app.failure(&e))?;
        let mut lib = app.lib;
        lib.write().replace_agent_documents(docs);
        Ok::<(), String>(())
    });
    let documents_failed = match &*documents.read() {
        Some(Err(e)) => Some(format!("Couldn't list the agents' documents. {e}")),
        _ => None,
    };
    let mut editing = use_signal(|| None::<AgentEdit>);
    let mut confirm = use_signal(|| None::<AgentConfirm>);
    let mut shown_token = use_signal(|| None::<(String, String)>);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);

    // Enabling needs no confirmation; disabling does.
    let set_disabled = move |id: Id, disabled: bool| async move {
        let Some(api) = app.api() else { return false };
        busy.set(true);
        let ok = match api
            .patch_agent(
                &id,
                &AgentPatch {
                    disabled: Some(disabled),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(agent) => {
                error.set(None);
                let mut lib = app.lib;
                lib.write().apply(Event::AgentChanged { agent });
                true
            }
            Err(e) => {
                error.set(Some(app.failure(&e)));
                false
            }
        };
        busy.set(false);
        ok
    };
    let mint = move |agent: Agent| async move {
        let Some(api) = app.api() else { return false };
        busy.set(true);
        let ok = match api.mint_agent_token(&agent.id).await {
            Ok(minted) => {
                error.set(None);
                shown_token.set(Some((minted.agent.name.clone(), minted.token.clone())));
                let mut lib = app.lib;
                lib.write().apply(Event::AgentChanged {
                    agent: minted.agent,
                });
                true
            }
            Err(e) => {
                error.set(Some(app.failure(&e)));
                false
            }
        };
        busy.set(false);
        ok
    };

    rsx! {
        section { class: "set-sec",
            div { class: "set-sec-head",
                h2 { "My agents" }
                if me.is_some() {
                    button { class: "btn btn-secondary btn-sm", onclick: move |_| editing.set(Some(AgentEdit::Create)), Icon { name: "plus" } "Create agent…" }
                }
            }
            p { class: "help", "An agent acts for you, never beyond what you can read: its ceiling is the part of your access it may use. Give an outside agent, like Claude Code, a token and run margin-mcp with it." }
            if let Some(err) = error.read().clone() {
                ErrorNotice { message: err }
            }
            if let Some(err) = documents_failed {
                ErrorNotice { message: err, on_retry: move |_| documents.restart() }
            }
            if mine.is_empty() {
                p { class: "faint", "You have no agents yet." }
            }
            div { class: "org-list", role: "list",
                for a in mine {
                    div { key: "{a.id}", class: "org-row agent-row", role: "listitem",
                        div { class: "grow",
                            div { class: "org-title",
                                b { class: "mono", "{a.name}" }
                                span { class: "badge", {org::kind_label(a.kind)} }
                                if a.disabled { span { class: "badge bad", "Disabled" } }
                            }
                            small { "May read: {ceiling_text(&a, me.as_ref(), &groups, &people)}" }
                            small { "{token_text(&a)}" }
                            div { class: "row-actions",
                                if a.token.is_some() {
                                    button {
                                        class: "btn btn-secondary btn-sm",
                                        disabled: *busy.read(),
                                        onclick: { let a = a.clone(); move |_| confirm.set(Some(AgentConfirm::Rotate(a.clone()))) },
                                        "Rotate token…"
                                    }
                                    button {
                                        class: "btn btn-secondary btn-sm",
                                        disabled: *busy.read(),
                                        onclick: { let a = a.clone(); move |_| confirm.set(Some(AgentConfirm::Revoke(a.clone()))) },
                                        "Revoke token…"
                                    }
                                } else {
                                    button {
                                        class: "btn btn-primary btn-sm",
                                        disabled: *busy.read(),
                                        onclick: {
                                            let a = a.clone();
                                            move |_| {
                                                let a = a.clone();
                                                async move { mint(a).await; }
                                            }
                                        },
                                        "Create token"
                                    }
                                }
                                button {
                                    class: "btn btn-secondary btn-sm",
                                    onclick: { let a = a.clone(); move |_| editing.set(Some(AgentEdit::Ceiling(a.clone()))) },
                                    "Edit ceiling…"
                                }
                                button {
                                    class: "btn btn-secondary btn-sm",
                                    onclick: { let a = a.clone(); move |_| editing.set(Some(AgentEdit::Rename(a.clone()))) },
                                    "Rename…"
                                }
                                if a.disabled {
                                    button {
                                        class: "btn btn-secondary btn-sm",
                                        disabled: *busy.read(),
                                        onclick: {
                                            let id = a.id.clone();
                                            move |_| {
                                                let id = id.clone();
                                                async move { set_disabled(id, false).await; }
                                            }
                                        },
                                        "Enable"
                                    }
                                } else {
                                    button {
                                        class: "btn btn-secondary btn-sm",
                                        disabled: *busy.read(),
                                        onclick: { let a = a.clone(); move |_| confirm.set(Some(AgentConfirm::Disable(a.clone()))) },
                                        "Disable…"
                                    }
                                }
                                button {
                                    class: "btn btn-secondary btn-sm",
                                    disabled: *busy.read(),
                                    onclick: { let a = a.clone(); move |_| confirm.set(Some(AgentConfirm::Delete(a.clone()))) },
                                    "Delete…"
                                }
                            }
                            AgentDocuments { agent: a.clone() }
                        }
                    }
                }
            }
        }
        if admin && !others.is_empty() {
            section { class: "set-sec",
                h2 { "All agents" }
                p { class: "help", "Everyone else's agents. An admin can disable one; only its owner changes what it reads." }
                div { class: "org-list", role: "list",
                    for a in others {
                        div { key: "{a.id}", class: "org-row", role: "listitem",
                            div { class: "grow",
                                div { class: "org-title",
                                    b { class: "mono", "{a.name}" }
                                    span { class: "faint", "of {handle_of(&people, &a.owner_id)}" }
                                    span { class: "badge", {org::kind_label(a.kind)} }
                                    if a.disabled { span { class: "badge bad", "Disabled" } }
                                }
                                small { "May read: {ceiling_text(&a, None, &groups, &people)}" }
                                small {
                                    match &a.token {
                                        Some(t) => format!("Token {}… · {}", t.prefix, t.last_used_at.as_ref().map(|at| format!("last used {}", ago(at))).unwrap_or_else(|| "never used".into())),
                                        None => "No token".to_string(),
                                    }
                                }
                                AgentDocuments { agent: a.clone() }
                            }
                            if a.disabled {
                                button {
                                    class: "btn btn-secondary btn-sm",
                                    disabled: *busy.read(),
                                    onclick: {
                                        let id = a.id.clone();
                                        move |_| {
                                            let id = id.clone();
                                            async move { set_disabled(id, false).await; }
                                        }
                                    },
                                    "Enable"
                                }
                            } else {
                                button {
                                    class: "btn btn-secondary btn-sm",
                                    disabled: *busy.read(),
                                    onclick: { let a = a.clone(); move |_| confirm.set(Some(AgentConfirm::Disable(a.clone()))) },
                                    "Disable…"
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(edit) = editing.read().clone() {
            AgentDialog {
                edit,
                on_close: move |_| editing.set(None),
                on_created: move |agent: Agent| {
                    editing.set(None);
                    async move {
                        mint(agent).await;
                    }
                },
            }
        }
        if let Some((name, token)) = shown_token.read().clone() {
            TokenDialog { name, token, on_close: move |_| shown_token.set(None) }
        }
        if let Some(c) = confirm.read().clone() {
            {
                let (title, message, label) = match &c {
                    AgentConfirm::Disable(a) => (
                        format!("Disable {}?", a.name),
                        "It can't connect or read anything until it's enabled again. Its token stays.".to_string(),
                        "Disable",
                    ),
                    AgentConfirm::Delete(a) => (
                        format!("Delete {}?", a.name),
                        "Its token stops working at once. What it added to Margin stays.".to_string(),
                        "Delete agent",
                    ),
                    AgentConfirm::Rotate(a) => (
                        format!("Rotate the token of {}?", a.name),
                        "The current token stops working at once. Give the new one to wherever the agent runs.".to_string(),
                        "Rotate token",
                    ),
                    AgentConfirm::Revoke(a) => (
                        format!("Revoke the token of {}?", a.name),
                        "The agent can't connect until you make a new token.".to_string(),
                        "Revoke token",
                    ),
                };
                rsx! {
                    ConfirmDialog {
                        title,
                        message,
                        confirm_label: label,
                        busy: *busy.read(),
                        error: error.read().clone(),
                        on_cancel: move |_| confirm.set(None),
                        on_confirm: {
                            let c = c.clone();
                            move |_| {
                                let c = c.clone();
                                async move {
                                    let ok = match c {
                                        AgentConfirm::Disable(a) => set_disabled(a.id, true).await,
                                        AgentConfirm::Rotate(a) => mint(a).await,
                                        AgentConfirm::Delete(a) => {
                                            let Some(api) = app.api() else { return };
                                            busy.set(true);
                                            let ok = match api.delete_agent(&a.id).await {
                                                Ok(()) => {
                                                    error.set(None);
                                                    let mut lib = app.lib;
                                                    lib.write().apply(Event::AgentDeleted { id: a.id });
                                                    true
                                                }
                                                Err(e) => {
                                                    error.set(Some(app.failure(&e)));
                                                    false
                                                }
                                            };
                                            busy.set(false);
                                            ok
                                        }
                                        AgentConfirm::Revoke(a) => {
                                            let Some(api) = app.api() else { return };
                                            busy.set(true);
                                            let ok = match api.revoke_agent_token(&a.id).await {
                                                Ok(()) => {
                                                    error.set(None);
                                                    let mut lib = app.lib;
                                                    lib.write().apply(Event::AgentChanged { agent: Agent { token: None, ..a } });
                                                    true
                                                }
                                                Err(e) => {
                                                    error.set(Some(app.failure(&e)));
                                                    false
                                                }
                                            };
                                            busy.set(false);
                                            ok
                                        }
                                    };
                                    if ok {
                                        confirm.set(None);
                                    }
                                }
                            }
                        },
                    }
                }
            }
        }
    }
}

/// The documents an agent ingested that the signed-in person reads ("Documents (n)"):
/// each with its audience, and for its owner Change… and Delete. The list comes from
/// the change feed, fetched once (`GET routes::CONTEXT_CHANGES`), and stays current
/// through `DocumentChanged` and `AudienceRevoked`.
#[component]
pub fn AgentDocuments(agent: Agent) -> Element {
    let app = use_app();
    let mut confirm = use_signal(|| None::<margin_shared::context::Document>);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let lib = app.lib.read();
    let loaded = lib.documents_loaded;
    let docs: Vec<margin_shared::context::Document> =
        lib.documents_of(&agent).into_iter().cloned().collect();
    drop(lib);
    let mine = app.person().is_some_and(|p| p.id == agent.owner_id);
    if !loaded {
        return rsx! { small { class: "faint", "Documents: loading…" } };
    }
    rsx! {
        details { class: "agent-docs",
            summary { "Documents ({docs.len()})" }
            if let Some(err) = error.read().clone() {
                ErrorNotice { message: err }
            }
            if docs.is_empty() {
                p { class: "faint",
                    if mine {
                        "None yet. What {agent.name} adds through margin-mcp ingest shows here, visible only to you unless it names an audience."
                    } else {
                        "None you can read."
                    }
                }
            }
            div { class: "org-list", role: "list", "aria-label": "Documents of {agent.name}",
                for d in docs {
                    div { key: "{d.id}", class: "org-row agent-doc", role: "listitem",
                        Icon { name: "file-add" }
                        div { class: "grow",
                            b { "{d.name}" }
                            crate::ui::audience::AudienceLine { target: crate::ui::audience::AudienceTarget::document(&d) }
                        }
                        if mine {
                            button {
                                class: "btn btn-secondary btn-sm",
                                "aria-label": "Delete {d.name}",
                                onclick: { let d = d.clone(); move |_| confirm.set(Some(d.clone())) },
                                "Delete…"
                            }
                        }
                    }
                }
            }
        }
        if let Some(d) = confirm.read().clone() {
            ConfirmDialog {
                title: "Delete “{d.name}”?",
                message: "Its passages leave Margin's search for everyone who could read them. The agent can add it again.",
                confirm_label: "Delete document",
                busy: *busy.read(),
                error: error.read().clone(),
                on_cancel: move |_| confirm.set(None),
                on_confirm: {
                    let id = d.id.clone();
                    move |_| {
                        let id = id.clone();
                        async move {
                            let Some(api) = app.api() else { return };
                            busy.set(true);
                            match api.delete_document(&id).await {
                                Ok(()) => {
                                    error.set(None);
                                    let mut lib = app.lib;
                                    lib.write().agent_documents.remove(&id);
                                    confirm.set(None);
                                }
                                Err(e) => error.set(Some(app.failure(&e))),
                            }
                            busy.set(false);
                        }
                    }
                },
            }
        }
    }
}

/// Create an agent (name, kind, ceiling), rename one, or edit its ceiling.
#[component]
fn AgentDialog(
    edit: AgentEdit,
    on_close: EventHandler<()>,
    on_created: EventHandler<Agent>,
) -> Element {
    let app = use_app();
    let Some(me) = app.person() else {
        return rsx! {};
    };
    let groups: Vec<Group> = app.lib.read().groups.values().cloned().collect();
    let start = edit.clone();
    let mut name = use_signal(|| match &start {
        AgentEdit::Rename(a) | AgentEdit::Ceiling(a) => a.name.clone(),
        AgentEdit::Create => String::new(),
    });
    let mut kind = use_signal(|| match &start {
        AgentEdit::Rename(a) | AgentEdit::Ceiling(a) => a.kind,
        AgentEdit::Create => AgentKind::External,
    });
    let me_start = me.clone();
    let mut ceiling = use_signal(move || match &start {
        AgentEdit::Rename(a) | AgentEdit::Ceiling(a) => a.ceiling.clone(),
        AgentEdit::Create => org::default_ceiling(AgentKind::External, &me_start),
    });
    // Until the ceiling is touched, it follows the kind's default.
    let mut touched = use_signal(|| false);
    let mut problem = use_signal(|| None::<String>);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let (title, submit) = match &edit {
        AgentEdit::Create => ("Create an agent".to_string(), "Create and make a token"),
        AgentEdit::Rename(a) => (format!("Rename {}", a.name), "Rename"),
        AgentEdit::Ceiling(a) => (format!("What {} may read", a.name), "Save ceiling"),
    };
    let shows_name = !matches!(edit, AgentEdit::Ceiling(_));
    let shows_ceiling = !matches!(edit, AgentEdit::Rename(_));
    let rows = ceiling_rows(&me, &groups, &ceiling.read());
    let me_for_kind = me.clone();
    let me_for_submit = me.clone();

    rsx! {
        FormDialog {
            title,
            busy: *busy.read(),
            on_close: move |_| on_close.call(()),
            form {
                class: "org-form",
                novalidate: true,
                onsubmit: {
                    let edit = edit.clone();
                    move |e: FormEvent| {
                        let edit = edit.clone();
                        let me = me_for_submit.clone();
                        async move {
                            e.prevent_default();
                            if *busy.peek() {
                                return;
                            }
                            let n = name.peek().trim().to_string();
                            if !matches!(edit, AgentEdit::Ceiling(_)) && !margin_shared::context::is_agent_name(&n) {
                                problem.set(Some("Use up to 40 lowercase letters, digits, - and _, like claude-code.".into()));
                                focus_first("#agent-name");
                                return;
                            }
                            let asked = ceiling.peek().clone();
                            let checked = if matches!(edit, AgentEdit::Rename(_)) {
                                None
                            } else {
                                match org::normalise_ceiling(&org::expansion(&me), &asked) {
                                    Ok(c) => Some(c),
                                    Err(p) => {
                                        problem.set(Some(p));
                                        return;
                                    }
                                }
                            };
                            problem.set(None);
                            let Some(api) = app.api() else { return };
                            busy.set(true);
                            let result = match &edit {
                                AgentEdit::Create => api.create_agent(&NewAgent { name: n, kind: *kind.peek(), ceiling: checked }).await,
                                AgentEdit::Rename(a) => api.patch_agent(&a.id, &AgentPatch { name: Some(n), ..Default::default() }).await,
                                AgentEdit::Ceiling(a) => api.patch_agent(&a.id, &AgentPatch { ceiling: checked, ..Default::default() }).await,
                            };
                            match result {
                                Ok(agent) => {
                                    let mut lib = app.lib;
                                    lib.write().apply(Event::AgentChanged { agent: agent.clone() });
                                    busy.set(false);
                                    if matches!(edit, AgentEdit::Create) {
                                        on_created.call(agent);
                                    } else {
                                        on_close.call(());
                                    }
                                }
                                Err(e) => {
                                    error.set(Some(app.failure(&e)));
                                    busy.set(false);
                                }
                            }
                        }
                    }
                },
                if shows_name {
                    div { class: "field",
                        label { r#for: "agent-name", "Name" }
                        input {
                            id: "agent-name",
                            class: "input mono",
                            autocomplete: "off",
                            spellcheck: "false",
                            placeholder: "claude-code",
                            initial_value: "{name}",
                            onmounted: move |e| async move { let _ = e.set_focus(true).await; },
                            oninput: move |e| {
                                problem.set(None);
                                name.set(e.value());
                            },
                        }
                    }
                }
                if matches!(edit, AgentEdit::Create) {
                    div { class: "field",
                        span { class: "label", id: "agent-kind-label", "Kind" }
                        div { class: "seg", role: "radiogroup", "aria-labelledby": "agent-kind-label",
                            for k in org::AGENT_KINDS {
                                button {
                                    key: "{k:?}",
                                    r#type: "button",
                                    role: "radio",
                                    "aria-checked": (k == *kind.read()).to_string(),
                                    tabindex: if k == *kind.read() { "0" } else { "-1" },
                                    onclick: {
                                        let me = me_for_kind.clone();
                                        move |_| {
                                            kind.set(k);
                                            if !*touched.peek() {
                                                ceiling.set(org::default_ceiling(k, &me));
                                            }
                                        }
                                    },
                                    {org::kind_label(k)}
                                }
                            }
                        }
                        span { class: "faint", style: "font-size: 13px", {org::kind_help(*kind.read())} }
                    }
                }
                if shows_ceiling {
                    CheckTree {
                        id: "agent-ceiling",
                        label: "May read",
                        rows,
                        on_toggle: move |(path, on): (String, bool)| {
                            touched.set(true);
                            problem.set(None);
                            let next = org::toggle_ceiling(&ceiling.peek(), &path, on);
                            ceiling.set(next);
                        },
                    }
                }
                if let Some(p) = problem.read().clone() {
                    span { class: "field-error", role: "alert", "{p}" }
                }
                if let Some(err) = error.read().clone() {
                    div { class: "notice error", role: "alert", Icon { name: "alert" } div { class: "body", "{err}" } }
                }
                div { class: "row-actions",
                    button { r#type: "button", class: "btn btn-secondary btn-sm", disabled: *busy.read(), onclick: move |_| on_close.call(()), "Cancel" }
                    button {
                        r#type: "submit",
                        class: "btn btn-primary btn-sm",
                        disabled: *busy.read(),
                        if *busy.read() { span { class: "spinner" } }
                        "{submit}"
                    }
                }
            }
        }
    }
}

/// A new agent token, shown this once, with Copy and the margin-mcp command.
#[component]
fn TokenDialog(name: String, token: String, on_close: EventHandler<()>) -> Element {
    let app = use_app();
    let command = org::token_command(&app.service_url());
    let mut copied = use_signal(|| None::<(&'static str, Result<(), String>)>);
    let copy = move |what: &'static str, text: String| async move {
        let result = copy_to_clipboard(text).await;
        copied.set(Some((what, result)));
    };
    let note = |what: &'static str| match &*copied.read() {
        Some((w, Ok(()))) if *w == what => Some("Copied".to_string()),
        Some((w, Err(e))) if *w == what => Some(e.clone()),
        _ => None,
    };
    let token_note = note("token");
    let command_note = note("command");
    rsx! {
        FormDialog {
            title: "The token of {name}",
            on_close: move |_| on_close.call(()),
            p { "Copy it now: Margin keeps only a fingerprint and won't show it again. Anyone with it reads what {name} may read." }
            div { class: "field",
                label { r#for: "agent-token", "Token" }
                div { class: "token-line",
                    input { id: "agent-token", class: "input mono", readonly: true, value: "{token}", onfocus: move |_| { let _ = crate::ui::common::js("document.getElementById('agent-token')?.select();"); } }
                    button {
                        class: "btn btn-secondary btn-sm",
                        onclick: {
                            let token = token.clone();
                            move |_| copy("token", token.clone())
                        },
                        Icon { name: "copy" }
                        "Copy"
                    }
                }
                if let Some(n) = token_note {
                    span { class: "save-state", role: "status", "{n}" }
                }
            }
            div { class: "field",
                span { class: "label", "On the computer the agent runs on" }
                p { class: "help", "Run this there and paste the token when it asks; it stays out of the shell history." }
                div { class: "token-line",
                    code { class: "mono token-command", "{command}" }
                    button {
                        class: "btn btn-secondary btn-sm",
                        onclick: {
                            let command = command.clone();
                            move |_| copy("command", command.clone())
                        },
                        Icon { name: "copy" }
                        "Copy"
                    }
                }
                if let Some(n) = command_note {
                    span { class: "save-state", role: "status", "{n}" }
                }
            }
            div { class: "row-actions",
                button { class: "btn btn-primary btn-sm", onclick: move |_| on_close.call(()), "Done" }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Account: the person, my settings, devices per person
// ---------------------------------------------------------------------------

/// "Signed in as @alice (Alice Martin) · Owner", with the name editable.
#[component]
pub fn PersonLine(person: Person, device: String, on_sign_out: EventHandler<()>) -> Element {
    let app = use_app();
    let mut editing = use_signal(|| false);
    let mut value = use_signal(String::new);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let id = person.id.clone();
    rsx! {
        div { class: "kv",
            span { "Signed in as" }
            span { "@{person.handle} ({person.name}) · {org::role_label(person.org_role)}" }
            span { "GitHub" }
            span { "{person.github_login}" }
            span { "This device" }
            span { "{device}" }
        }
        if *editing.read() {
            form {
                class: "org-form",
                onsubmit: move |e: FormEvent| {
                    let id = id.clone();
                    async move {
                        e.prevent_default();
                        let n = value.peek().trim().to_string();
                        if n.is_empty() {
                            error.set(Some("Enter a name.".into()));
                            focus_first("#my-name");
                            return;
                        }
                        let Some(api) = app.api() else { return };
                        busy.set(true);
                        match api.patch_person(&id, &PersonPatch { name: Some(n), ..Default::default() }).await {
                            Ok(person) => {
                                error.set(None);
                                store_person(app, person);
                                editing.set(false);
                            }
                            Err(e) => error.set(Some(app.failure(&e))),
                        }
                        busy.set(false);
                    }
                },
                div { class: "field",
                    label { r#for: "my-name", "Your name" }
                    input {
                        id: "my-name",
                        class: "input",
                        initial_value: "{value}",
                        "aria-invalid": error.read().is_some().to_string(),
                        onmounted: move |e| async move { let _ = e.set_focus(true).await; },
                        oninput: move |e| {
                            error.set(None);
                            value.set(e.value());
                        },
                    }
                    if let Some(err) = error.read().clone() {
                        span { class: "field-error", role: "alert", "{err}" }
                    }
                }
                div { class: "row-actions",
                    button { r#type: "submit", class: "btn btn-primary btn-sm", disabled: *busy.read(),
                        if *busy.read() { span { class: "spinner" } }
                        "Save name"
                    }
                    button { r#type: "button", class: "btn btn-ghost btn-sm", onclick: move |_| editing.set(false), "Cancel" }
                }
            }
        } else {
            div { class: "row-actions",
                button {
                    class: "btn btn-secondary btn-sm",
                    onclick: {
                        let name = person.name.clone();
                        move |_| {
                            value.set(name.clone());
                            error.set(None);
                            editing.set(true);
                        }
                    },
                    "Edit name…"
                }
                button { class: "btn btn-secondary btn-sm", onclick: move |_| on_sign_out.call(()), "Sign out…" }
            }
        }
    }
}

/// The providers "My settings" offers: the configured ones, and the one picked if it
/// lost its credential since (so the choice shows).
pub fn my_provider_options(settings: Option<&Settings>, picked: Option<Provider>) -> Vec<Provider> {
    Provider::ALL
        .into_iter()
        .filter(|p| settings.is_none_or(|s| s.has_credential(*p)) || picked == Some(*p))
        .collect()
}

/// Settings → Account → My settings: the provider and reasoning a person's own chats use.
#[component]
pub fn MySettingsSection(person: Person) -> Element {
    let app = use_app();
    let settings = app.lib.read().settings.clone();
    let mine = person.settings.clone();
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let mut saved = use_signal(|| false);
    let options = my_provider_options(settings.as_ref(), mine.default_provider);
    let org_provider = settings.as_ref().map(|s| s.default_provider.label());
    let org_effort = settings
        .as_ref()
        .map(|s| effort_label(shown_effort(s.reasoning_effort)));
    let effort = mine.reasoning_effort.map(shown_effort);

    let save = move |next: PersonSettings| async move {
        let Some(api) = app.api() else { return };
        busy.set(true);
        saved.set(false);
        match api.patch_my_settings(&next).await {
            Ok(settings) => {
                error.set(None);
                saved.set(true);
                set_my_settings(app, settings);
            }
            Err(e) => error.set(Some(app.failure(&e))),
        }
        busy.set(false);
    };

    rsx! {
        section { class: "set-sec",
            h2 { "My settings" }
            p { class: "help", "What your own new chats use. Organisation default follows what an admin set under AI." }
            div { class: "field",
                span { class: "label", id: "my-provider-label", "Provider for new chats" }
                div { class: "seg seg-wrap", role: "radiogroup", "aria-labelledby": "my-provider-label", "aria-busy": busy.read().to_string(),
                    button {
                        role: "radio",
                        "aria-checked": mine.default_provider.is_none().to_string(),
                        tabindex: if mine.default_provider.is_none() { "0" } else { "-1" },
                        onclick: {
                            let mine = mine.clone();
                            move |_| {
                                let next = PersonSettings { default_provider: None, ..mine.clone() };
                                async move {
                                    if !*busy.peek() { save(next).await; }
                                }
                            }
                        },
                        match org_provider {
                            Some(p) => format!("Organisation default ({p})"),
                            None => "Organisation default".to_string(),
                        }
                    }
                    for p in options {
                        button {
                            key: "{p.as_str()}",
                            role: "radio",
                            "aria-checked": (mine.default_provider == Some(p)).to_string(),
                            tabindex: if mine.default_provider == Some(p) { "0" } else { "-1" },
                            onclick: {
                                let mine = mine.clone();
                                move |_| {
                                    let next = PersonSettings { default_provider: Some(p), ..mine.clone() };
                                    async move {
                                        if !*busy.peek() { save(next).await; }
                                    }
                                }
                            },
                            {p.label()}
                        }
                    }
                }
                if let Some(p) = mine.default_provider
                    && settings.as_ref().is_some_and(|s| !s.has_credential(p))
                {
                    span { class: "faint", style: "font-size: 13px", "{p.label()} isn't set up on this Margin any more; ask an admin, or pick another." }
                }
            }
            div { class: "field",
                span { class: "label", id: "my-effort-label", "Reasoning" }
                div { class: "seg seg-wrap", role: "radiogroup", "aria-labelledby": "my-effort-label", "aria-busy": busy.read().to_string(),
                    button {
                        role: "radio",
                        "aria-checked": effort.is_none().to_string(),
                        tabindex: if effort.is_none() { "0" } else { "-1" },
                        onclick: {
                            let mine = mine.clone();
                            move |_| {
                                let next = PersonSettings { reasoning_effort: None, ..mine.clone() };
                                async move {
                                    if !*busy.peek() { save(next).await; }
                                }
                            }
                        },
                        match org_effort {
                            Some(e) => format!("Organisation default ({e})"),
                            None => "Organisation default".to_string(),
                        }
                    }
                    for e in ReasoningEffort::ALL {
                        button {
                            key: "{e.as_str()}",
                            role: "radio",
                            "aria-checked": (effort == Some(e)).to_string(),
                            tabindex: if effort == Some(e) { "0" } else { "-1" },
                            onclick: {
                                let mine = mine.clone();
                                move |_| {
                                    let next = PersonSettings { reasoning_effort: Some(e), ..mine.clone() };
                                    async move {
                                        if !*busy.peek() { save(next).await; }
                                    }
                                }
                            },
                            {effort_label(e)}
                        }
                    }
                }
            }
            if let Some(err) = error.read().clone() {
                span { class: "save-state error", role: "alert", "{err}" }
            } else if *saved.read() {
                span { class: "save-state", role: "status", "Saved" }
            }
        }
    }
}

/// Everyone else's devices, grouped by person (admins): each with Revoke.
pub fn devices_by_person(
    devices: &[Device],
    me: &str,
    people: &[Person],
) -> Vec<(String, Vec<Device>)> {
    let mut groups: Vec<(String, Vec<Device>)> = Vec::new();
    let mut others: Vec<&Device> = devices
        .iter()
        .filter(|d| d.person_id.as_deref().is_some_and(|p| p != me))
        .collect();
    others.sort_by(|a, b| {
        handle_of(people, a.person_id.as_deref().unwrap_or_default())
            .cmp(&handle_of(
                people,
                b.person_id.as_deref().unwrap_or_default(),
            ))
            .then(b.created_at.cmp(&a.created_at))
    });
    for d in others {
        let who = handle_of(people, d.person_id.as_deref().unwrap_or_default());
        match groups.last_mut() {
            Some((w, list)) if *w == who => list.push(d.clone()),
            _ => groups.push((who, vec![d.clone()])),
        }
    }
    groups
}

/// The everyone's-devices list of Settings → Account (admins).
#[component]
pub fn EveryonesDevices(
    groups: Vec<(String, Vec<Device>)>,
    on_revoke: EventHandler<Device>,
) -> Element {
    rsx! {
        section { class: "set-sec",
            h2 { "Everyone's devices" }
            p { class: "help", "Other people's signed-in devices. Revoking one signs it out; they can sign in again unless they're disabled." }
            if groups.is_empty() {
                p { class: "faint", "No one else has signed in yet." }
            }
            for (who, list) in groups {
                div { key: "{who}", class: "org-devices",
                    h3 { class: "org-who", "{who}" }
                    for d in list {
                        div { key: "{d.id}", class: "device",
                            span {
                                class: if d.online { "dot ok" } else { "dot" },
                                title: if d.online { "Online" } else { "Offline" },
                                "aria-hidden": "true",
                            }
                            div { class: "grow",
                                "{d.name}"
                                small { "{presence_text(&d)} · added {date(&d.created_at)}" }
                            }
                            button {
                                class: "btn btn-secondary btn-sm",
                                onclick: {
                                    let d = d.clone();
                                    move |_| on_revoke.call(d.clone())
                                },
                                "Revoke…"
                            }
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The organisation's settings, as a member reads them
// ---------------------------------------------------------------------------

/// Whether a member reads which ChatGPT account the organisation signed in with. Off by
/// default (the account's email is the admin's business); the owner may turn it on.
pub const MEMBERS_SEE_CHATGPT_EMAIL: bool = false;

/// Whether a provider can answer, in words, as a member reads it: the ChatGPT account's
/// email only when `MEMBERS_SEE_CHATGPT_EMAIL`.
pub fn setup_text(settings: &Settings, provider: Provider) -> String {
    match (provider, &settings.chatgpt) {
        _ if !settings.has_credential(provider) => "Not set up".into(),
        (Provider::ChatGpt, Some(account)) if MEMBERS_SEE_CHATGPT_EMAIL => {
            format!("Signed in as {}", account.email)
        }
        (Provider::ChatGpt, Some(_)) => "Yes, signed in to the organisation's account".into(),
        _ => "Yes".into(),
    }
}

/// Settings → AI for a member: every value, read-only, and who changes them.
#[component]
pub fn ReadOnlyAi(settings: Settings) -> Element {
    rsx! {
        section { class: "set-sec",
            h2 { "Providers" }
            p { class: "help", "{ADMIN_CHANGES_THESE}" }
            div { class: "kv",
                span { "Default provider" }
                span { {settings.default_provider.label()} }
                span { "Reasoning" }
                span { {effort_label(shown_effort(settings.reasoning_effort))} }
            }
        }
        for p in Provider::ALL {
            section { key: "{p.as_str()}", class: "set-sec",
                h2 { {p.label()} }
                div { class: "kv",
                    span { "Set up" }
                    span { {setup_text(&settings, p)} }
                    if p == Provider::OpenAi {
                        span { "Base URL" }
                        span { class: "mono", "{settings.openai_base_url}" }
                    }
                    for route in Route::ALL {
                        span { key: "{p.as_str()}-l-{route:?}", {slot_label(route)} }
                        span { class: "mono", {settings.models(p).get(route).to_string()} }
                    }
                }
            }
        }
        section { class: "set-sec",
            h2 { "Retrieval pipeline" }
            div { class: "kv",
                for (k, v) in crate::ui::settings::pipeline_rows(&settings.pipeline) {
                    span { key: "k-{k}", "{k}" }
                    span { class: "mono", "{v}" }
                }
            }
        }
    }
}

/// Settings → Voice & video for a member.
#[component]
pub fn ReadOnlyVideo(settings: Settings) -> Element {
    rsx! {
        section { class: "set-sec",
            h2 { "Voice and video render" }
            p { class: "help", "{ADMIN_CHANGES_THESE}" }
            div { class: "kv",
                span { "Voice" }
                span { {settings.voice.clone().unwrap_or_else(|| "Default voice".into())} }
                span { "Size" }
                span { "{settings.render_width} × {settings.render_height}" }
                span { "Frame rate" }
                span { "{settings.render_fps} fps" }
            }
        }
    }
}

/// Settings → Advanced for a member: traces are for admins and the eval runner.
#[component]
pub fn ReadOnlyAdvanced(settings: Settings) -> Element {
    rsx! {
        section { class: "set-sec",
            h2 { "Traces" }
            p { class: "help", "The service records a trace of every model call, search and job, never what was said; admins and the evaluation runner read them. Only an organisation admin changes how long they're kept." }
            div { class: "kv",
                span { "Kept for" }
                span { if settings.trace_retention_days == 1 { "1 day" } else { "{settings.trace_retention_days} days" } }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{ALICE, BOB, TOKEN, TOKEN_BOB, sample_me};
    use crate::store::LibraryState;
    use crate::ui::settings::SettingsPane;
    use crate::ui::test_render::{Extra, render_with};

    fn org_lib() -> LibraryState {
        let s = crate::fake::FakeState::new();
        let mut lib = LibraryState::from_library(crate::fake::sample_library());
        lib.replace_people(s.people.clone());
        lib.replace_groups(s.groups.clone());
        lib.replace_agents(s.agents.clone());
        lib.replace_invitations(s.invitations.clone());
        lib.replace_devices(s.devices.clone());
        lib.settings = Some(s.settings.clone());
        lib
    }

    fn as_member(lib: &mut LibraryState) -> Extra {
        // A member's service lists only their own agents and no invitations.
        lib.agents.retain(|_, a| a.owner_id == BOB);
        lib.invitations.clear();
        lib.devices
            .retain(|_, d| d.person_id.as_deref() == Some(BOB));
        Extra {
            me: Some(sample_me(TOKEN_BOB)),
            ..Default::default()
        }
    }

    #[test]
    fn organisation_as_the_owner() {
        let html = render_with(
            "/",
            org_lib(),
            Some(SettingsPane::Organisation),
            Extra {
                me: Some(sample_me(TOKEN)),
                ..Default::default()
            },
        );
        // People: both, with roles; the owner row has no state control.
        assert!(html.contains("<b>@alice</b>"), "{html}");
        assert!(html.contains("<b>@bob</b>"));
        assert!(html.contains(">Owner<") && html.contains(">Member<"));
        assert!(html.contains(">Make admin…<"));
        assert_eq!(
            html.matches(">Disable…<").count(),
            3,
            "Bob, Alice's agent and Bob's agent"
        );
        assert_eq!(html.matches(">Edit groups…<").count(), 2);
        // Invitations: carol's open one with Revoke, Bob's accepted one collapsed.
        assert!(html.contains("<b>@carol-dev</b>"));
        assert!(html.contains(">Revoke…<"));
        assert!(html.contains("Accepted (1)"));
        assert!(html.contains(">Invite…<"));
        // Groups: the tree, Company undeletable, Engineering with members.
        assert!(html.contains("company/engineering"));
        assert!(html.contains("Everyone (2 people)"));
        assert!(html.contains("can&#39;t be deleted"));
        assert!(html.contains("Take its 2 members out first"));
        assert!(html.contains(">Add group…<"));
        // Agents: mine with its token prefix; everyone else's (Bob's) to disable.
        assert!(html.contains("<b class=\"mono\">claude-code</b>"));
        assert!(html.contains("Token mga_7f3a…"));
        assert!(html.contains(">Rotate token…<") && html.contains(">Revoke token…<"));
        assert!(html.contains("May read: Company, Engineering"));
        assert!(html.contains("<h2>All agents</h2>"));
        assert!(html.contains("triage-bot"));
        assert!(html.contains("of @bob"));
    }

    #[test]
    fn organisation_as_a_member() {
        let mut lib = org_lib();
        let extra = as_member(&mut lib);
        let html = render_with("/", lib, Some(SettingsPane::Organisation), extra);
        assert!(
            html.contains("<b>@alice</b>") && html.contains("<b>@bob</b>"),
            "{html}"
        );
        // No admin controls anywhere: no dead buttons, the sentence instead.
        for admin_only in [
            ">Make admin…<",
            ">Edit groups…<",
            ">Invite…<",
            ">Add group…<",
            ">Enable<",
        ] {
            assert!(!html.contains(admin_only), "{admin_only}");
        }
        assert!(html.contains("Ask an admin to invite people."));
        assert!(!html.contains("carol-dev"));
        assert!(!html.contains("<h2>All agents</h2>"));
        // Their own agent, without a token yet.
        assert!(html.contains("triage-bot"));
        assert!(html.contains(">Create token<"));
        assert!(!html.contains("claude-code"));
    }

    #[test]
    fn account_shows_the_person_my_settings_and_devices() {
        let html = render_with(
            "/",
            org_lib(),
            Some(SettingsPane::Account),
            Extra {
                me: Some(sample_me(TOKEN)),
                ..Default::default()
            },
        );
        assert!(html.contains("@alice (Alice Martin) · Owner"), "{html}");
        assert!(html.contains(">Edit name…<"));
        assert!(html.contains("<h2>My settings</h2>"));
        assert!(html.contains("Organisation default (OpenAI)"));
        assert!(html.contains("Organisation default (Medium)"));
        // Alice's two devices; Bob's laptop under everyone's devices.
        assert!(html.contains("test-mac") && html.contains("iPhone"));
        assert!(html.contains("<h2>Everyone&#39;s devices</h2>"));
        assert!(html.contains("Bob&#39;s laptop"));
        assert!(html.contains("@bob"));

        let mut lib = org_lib();
        let extra = as_member(&mut lib);
        let html = render_with("/", lib, Some(SettingsPane::Account), extra);
        assert!(html.contains("@bob (Bob Chen) · Member"));
        assert!(!html.contains("Everyone&#39;s devices"));
        assert!(!html.contains("test-mac"));
    }

    #[test]
    fn a_member_reads_the_organisations_settings() {
        for pane in [
            SettingsPane::Ai,
            SettingsPane::Video,
            SettingsPane::Advanced,
        ] {
            let mut lib = org_lib();
            let extra = as_member(&mut lib);
            let html = render_with("/", lib, Some(pane), extra);
            assert!(
                html.contains("Only an organisation admin changes"),
                "{pane:?}: {html}"
            );
            // Values, not controls (in the pane; the screen behind has its own).
            let pane_html = &html[html.find("class=\"settings\"").unwrap()..];
            assert!(!pane_html.contains(">Save<"), "{pane:?}");
            assert!(!pane_html.contains("role=\"radiogroup\""), "{pane:?}");
            assert!(!pane_html.contains("<input"), "{pane:?}");
            assert!(!pane_html.contains("<select"), "{pane:?}");
        }
        let mut lib = org_lib();
        let extra = as_member(&mut lib);
        let html = render_with("/", lib, Some(SettingsPane::Ai), extra);
        assert!(html.contains("Model for chats"));
        assert!(html.contains("https://api.openai.com/v1"));
        // An admin keeps the controls.
        let html = render_with(
            "/",
            org_lib(),
            Some(SettingsPane::Ai),
            Extra {
                me: Some(sample_me(TOKEN)),
                ..Default::default()
            },
        );
        let pane_html = &html[html.find("class=\"settings\"").unwrap()..];
        assert!(pane_html.contains("role=\"radiogroup\""));
        assert!(pane_html.contains("<input"));
        assert!(!html.contains("Only an organisation admin changes"));
    }

    #[test]
    fn nobody_is_an_admin_until_me_has_loaded() {
        // `me` without a person yet: the member's read-only pane, no admin controls.
        let mut me = sample_me(TOKEN);
        me.person = None;
        let html = render_with(
            "/",
            org_lib(),
            Some(SettingsPane::Ai),
            Extra {
                me: Some(me.clone()),
                ..Default::default()
            },
        );
        assert!(
            html.contains("Only an organisation admin changes"),
            "{html}"
        );
        let pane_html = &html[html.find("class=\"settings\"").unwrap()..];
        assert!(!pane_html.contains("<input"));
        let html = render_with(
            "/",
            org_lib(),
            Some(SettingsPane::Organisation),
            Extra {
                me: Some(me),
                ..Default::default()
            },
        );
        assert!(!html.contains(">Invite…<") && !html.contains(">Add group…<"));
    }

    #[test]
    fn a_member_doesnt_read_the_chatgpt_accounts_email() {
        let mut lib = org_lib();
        let mut settings = lib.settings.clone().unwrap();
        settings.chatgpt = Some(crate::fake::fake_account("owner@example.com"));
        lib.settings = Some(settings.clone());
        let extra = as_member(&mut lib);
        let html = render_with("/", lib, Some(SettingsPane::Ai), extra);
        assert!(!html.contains("owner@example.com"), "{html}");
        assert!(html.contains("Yes, signed in to the organisation&#39;s account"));
        assert_eq!(
            setup_text(&settings, Provider::ChatGpt),
            "Yes, signed in to the organisation's account"
        );
    }

    #[test]
    fn rows_of_the_trees() {
        let s = crate::fake::FakeState::new();
        let alice = s.person(ALICE).unwrap().clone();
        let rows = ceiling_rows(
            &alice,
            &s.groups,
            &["company".into(), "company/sales".into()],
        );
        let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "person/p-alice",
                "company",
                "company/engineering",
                "company/sales"
            ]
        );
        assert_eq!(rows[0].label, "My private notes and chats");
        assert!(!rows[0].checked && rows[1].checked && !rows[2].checked);
        assert!(rows[3].struck && rows[3].checked);
        let rows = group_rows(&s.groups, &["company/engineering".into()]);
        assert!(!rows[0].checked && rows[0].note.is_some());
        assert!(rows[1].checked && rows[1].depth == 1);
        assert_eq!(
            groups_text(&["company".into(), "company/engineering".into()], &s.groups),
            "Engineering"
        );
        assert_eq!(groups_text(&[], &s.groups), "Company only");
        // Devices of the others, by person.
        let grouped = devices_by_person(&s.devices, ALICE, &s.people);
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped[0].0, "@bob");
        assert_eq!(grouped[0].1[0].id, "d3");
        let options = my_provider_options(Some(&s.settings), Some(Provider::Claude));
        assert_eq!(options, vec![Provider::OpenAi, Provider::Claude]);
        assert_eq!(
            my_provider_options(Some(&s.settings), None),
            vec![Provider::OpenAi]
        );
    }
}
