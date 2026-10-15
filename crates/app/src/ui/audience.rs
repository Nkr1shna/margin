//! Who may read what (L2, `docs/l2-brief.md` D9): the Audience control on the units an
//! owner sets by hand (a topic, a folder, a file of no topic, an agent's document), the
//! one-line summary shown wherever a row is, the "follows its topic" line on lessons and
//! notes with the narrowed notice, and the outline card's line. The control is a dialog
//! on computers and a sheet on phones (`.modal-scrim.sheet`); only the row's owner
//! changes it, everyone else reads the summary.

use dioxus::prelude::*;
use margin_shared::context::Document;
use margin_shared::{
    Agent, Audience, AudienceUnit, Group, Id, Person, PersonState, Proposal, ROOT_GROUP, Source,
    Topic, Workspace, person_id,
};

use crate::org::{self, AudienceDraft, Names, Side};
use crate::state::{AppCtx, use_app};
use crate::store::LibraryState;
use crate::ui::common::{ConfirmDialog, Icon};
use crate::ui::organisation::{CheckRow, CheckTree, load_org};

/// A unit whose audience can be set, as the control needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct AudienceTarget {
    pub unit: AudienceUnit,
    pub id: Id,
    /// What the dialog calls it ("Fourier series", "notes", "deploy-runbook.md").
    pub name: String,
    pub owner: Option<Id>,
    /// As stored (read empty, private to the owner).
    pub audience: Audience,
}

impl AudienceTarget {
    fn new(
        unit: AudienceUnit,
        id: &str,
        name: &str,
        owner: &Option<Id>,
        access: &[String],
        deny: &[String],
    ) -> Self {
        Self {
            unit,
            id: id.to_string(),
            name: name.to_string(),
            owner: owner.clone(),
            audience: org::audience_of(owner.as_deref(), access, deny),
        }
    }

    pub fn topic(t: &Topic) -> Self {
        Self::new(
            AudienceUnit::Topic,
            &t.id,
            &t.title,
            &t.owner,
            &t.access,
            &t.deny,
        )
    }

    pub fn workspace(w: &Workspace) -> Self {
        Self::new(
            AudienceUnit::Workspace,
            &w.id,
            &w.name,
            &w.owner,
            &w.access,
            &w.deny,
        )
    }

    /// A file of no topic and no folder (topics' and folders' files follow them).
    pub fn source(s: &Source) -> Self {
        Self::new(
            AudienceUnit::Source,
            &s.id,
            &s.name,
            &s.owner,
            &s.access,
            &s.deny,
        )
    }

    pub fn document(d: &Document) -> Self {
        Self::new(
            AudienceUnit::Document,
            &d.id,
            &d.name,
            &d.owner,
            &d.access,
            &d.deny,
        )
    }
}

/// The directory, the groups and the agents, as the store has them.
#[derive(Clone, PartialEq, Default)]
pub struct Directory {
    pub groups: Vec<Group>,
    pub people: Vec<Person>,
    pub agents: Vec<Agent>,
}

impl Directory {
    pub fn of(lib: &LibraryState) -> Self {
        Self {
            groups: org::tree_order(&lib.groups.values().cloned().collect::<Vec<_>>()),
            people: lib.people_in_order().into_iter().cloned().collect(),
            agents: lib.agents.values().cloned().collect(),
        }
    }

    pub fn names(&self) -> Names<'_> {
        Names {
            groups: &self.groups,
            people: &self.people,
            agents: &self.agents,
        }
    }
}

/// The summary of a target for the signed-in person.
fn summary_of(app: AppCtx, target: &AudienceTarget) -> String {
    let dir = Directory::of(&app.lib.read());
    let me = app.person().map(|p| p.id);
    org::audience_summary(
        &target.audience,
        target.owner.as_deref(),
        me.as_deref(),
        dir.names(),
    )
}

/// Whether the signed-in person owns the target (only they change its audience).
fn owns(app: AppCtx, owner: Option<&str>) -> bool {
    app.person()
        .is_some_and(|me| owner.is_some_and(|o| o == me.id))
}

/// "Everyone at the company" → "everyone at the company", for the middle of a sentence.
pub fn lower_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) if first.is_uppercase() => first.to_lowercase().chain(chars).collect(),
        Some(first) => std::iter::once(first).chain(chars).collect(),
        None => String::new(),
    }
}

/// What follows a unit, for the warning about readers losing access: "12 lessons",
/// "this topic", "5 files", "this file".
pub fn follows_what(lib: &LibraryState, target: &AudienceTarget) -> String {
    let n = |n: usize, one: &str, many: &str| {
        if n == 1 {
            format!("1 {one}")
        } else {
            format!("{n} {many}")
        }
    };
    match target.unit {
        AudienceUnit::Topic => match lib.lesson_count_deep(&target.id) {
            0 => "this topic".into(),
            k => n(k, "lesson", "lessons"),
        },
        AudienceUnit::Workspace => match lib.workspaces.get(&target.id).map(|w| w.sync.files) {
            Some(files) if files > 0 => n(files as usize, "file", "files"),
            _ => "this folder".into(),
        },
        AudienceUnit::Source => "this file".into(),
        AudienceUnit::Document => "this document".into(),
    }
}

/// "@bob will lose access to 12 lessons." / "3 people will lose access to this folder."
pub fn losing_text(losing: &[&Person], what: &str) -> String {
    match losing {
        [one] => format!("@{} will lose access to {what}.", one.handle),
        many => format!("{} people will lose access to {what}.", many.len()),
    }
}

/// What a unit's control says follows it.
fn unit_help(unit: AudienceUnit) -> &'static str {
    match unit {
        AudienceUnit::Topic => {
            "Its subtopics, lessons, notes and sources follow it. A lesson that drew on documents fewer people can read stays visible only to you."
        }
        AudienceUnit::Workspace => "Its files follow it.",
        AudienceUnit::Source => "This file only.",
        AudienceUnit::Document => "This document only. An agent reads it only within its ceiling.",
    }
}

/// The group tree for one list: "Company (everyone)" first; a group under a checked one
/// says it's already included; a stored group that no longer exists is struck through.
pub fn audience_group_rows(groups: &[Group], checked: &[String]) -> Vec<CheckRow> {
    let mut rows: Vec<CheckRow> = org::tree_order(groups)
        .into_iter()
        .map(|g| {
            let through = checked
                .iter()
                .filter(|c| **c != g.path && g.path.starts_with(&format!("{c}/")))
                .filter_map(|c| groups.iter().find(|x| x.path == *c))
                .map(|x| x.name.clone())
                .next();
            CheckRow {
                key: g.path.clone(),
                label: if g.path == ROOT_GROUP {
                    format!("{} (everyone)", g.name)
                } else {
                    format!("{} ({})", g.name, g.path)
                },
                depth: org::depth(&g.path),
                checked: checked.contains(&g.path),
                struck: false,
                note: through
                    .filter(|_| !checked.contains(&g.path))
                    .map(|n| format!("already included through {n}")),
            }
        })
        .collect();
    for gone in checked
        .iter()
        .filter(|c| margin_shared::is_group_path(c) && !groups.iter().any(|g| g.path == **c))
    {
        rows.push(CheckRow {
            key: gone.clone(),
            label: gone.clone(),
            depth: 0,
            checked: true,
            struck: true,
            note: Some("this group no longer exists".into()),
        });
    }
    rows
}

/// A one-line summary of who reads a unit, with "Change…" for its owner.
#[component]
pub fn AudienceLine(target: AudienceTarget, #[props(default)] prefix: String) -> Element {
    let app = use_app();
    let mut open = use_signal(|| false);
    let mine = owns(app, target.owner.as_deref());
    let summary = summary_of(app, &target);
    rsx! {
        div { class: "aud-line",
            Icon { name: "users" }
            span { class: "aud-text", "{prefix}{summary}" }
            if mine {
                button {
                    class: "link-btn",
                    "aria-label": "Change who can read {target.name}",
                    "aria-haspopup": "dialog",
                    onclick: move |e| {
                        e.stop_propagation();
                        open.set(true);
                    },
                    "Change…"
                }
            }
        }
        if *open.read() {
            AudienceDialog { target: target.clone(), on_close: move |_| open.set(false) }
        }
    }
}

/// A lesson's or note's audience line: it follows its topic's (with "Change…" for the
/// topic's owner), or, for a lesson narrowed by what it drew on, the notice with
/// "Publish…".
#[component]
pub fn FollowsLine(topic_id: Id, #[props(default)] narrowed: bool, kind: &'static str) -> Element {
    let app = use_app();
    let mut open = use_signal(|| false);
    let Some(topic) = app.lib.read().topics.get(&topic_id).cloned() else {
        return rsx! {};
    };
    let target = AudienceTarget::topic(&topic);
    let mine = owns(app, target.owner.as_deref());
    let summary = lower_first(&summary_of(app, &target));
    rsx! {
        if narrowed {
            div { class: "notice aud-narrowed", role: "status",
                Icon { name: "users" }
                div { class: "body",
                    span { "Visible only to you: this {kind} draws on documents with different audiences. Publish the topic to share it." }
                    if mine {
                        div { class: "row-actions",
                            button {
                                class: "btn btn-secondary btn-sm",
                                "aria-haspopup": "dialog",
                                onclick: move |_| open.set(true),
                                "Publish…"
                            }
                        }
                    }
                }
            }
        } else {
            div { class: "aud-line",
                Icon { name: "users" }
                span { class: "aud-text", "Audience: follows ‘{topic.title}’ ({summary})" }
                if mine {
                    button {
                        class: "link-btn",
                        "aria-label": "Change who can read {topic.title}",
                        "aria-haspopup": "dialog",
                        onclick: move |_| open.set(true),
                        "Change…"
                    }
                }
            }
        }
        if *open.read() {
            AudienceDialog { target, on_close: move |_| open.set(false) }
        }
    }
}

/// The outline card's audience: who its lessons will be visible to, and the notice when
/// the planner drew on documents with different audiences.
#[component]
pub fn ProposalAudience(proposal: Proposal) -> Element {
    let app = use_app();
    let audience = org::audience_of(proposal.owner.as_deref(), &proposal.access, &proposal.deny);
    let me = app.person().map(|p| p.id);
    let dir = Directory::of(&app.lib.read());
    let summary = org::audience_summary(
        &audience,
        proposal.owner.as_deref(),
        me.as_deref(),
        dir.names(),
    );
    rsx! {
        if proposal.audience_narrowed {
            div { class: "notice aud-narrowed", role: "status",
                Icon { name: "users" }
                div { class: "body",
                    "Visible only to you: this outline draws on documents with different audiences. Publish the topic once it's made to share it."
                }
            }
        } else {
            div { class: "aud-line",
                Icon { name: "users" }
                span { class: "aud-text", "Audience: {lower_first(&summary)}" }
            }
        }
    }
}

/// The Audience control: a dialog on computers, a sheet on phones. The owner picks Only
/// me, or groups, people and their own agents, and who it's never visible to; Save sends
/// it (with a warning first when readers lose access) and shows the service's sentence
/// if it refuses. Anyone else reads the summary.
#[component]
pub fn AudienceDialog(target: AudienceTarget, on_close: EventHandler<()>) -> Element {
    let app = use_app();
    // The directory, unless Settings → Organisation loaded it this session.
    let _directory = use_resource(move || async move {
        if app.lib.peek().org_loaded {
            return;
        }
        if let Some(api) = app.api() {
            let _ = load_org(app, &api).await;
        }
    });
    let owner = target.owner.clone();
    let start = AudienceDraft::from_stored(&target.audience, owner.as_deref());
    let sharing_start = !start.only_me();
    let mut draft = use_signal(move || start);
    let mut sharing = use_signal(move || sharing_start);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let mut confirm = use_signal(|| None::<String>);
    let mut full = use_signal(|| None::<String>);

    let me = app.person();
    let mine = owns(app, owner.as_deref());
    let lib = app.lib.read();
    let dir = Directory::of(&lib);
    let what = follows_what(&lib, &target);
    drop(lib);
    let names = dir.names();
    let me_id = me.as_ref().map(|p| p.id.clone());
    let current = draft.read().clone();
    let body = if *sharing.read() {
        current.body()
    } else {
        Audience::default()
    };
    let after = org::stored_audience(owner.as_deref(), &body);
    let now_summary = org::audience_summary(&after, owner.as_deref(), me_id.as_deref(), names);
    let stored_summary =
        org::audience_summary(&target.audience, owner.as_deref(), me_id.as_deref(), names);
    let owner_label = owner
        .as_deref()
        .map(|o| org::entry_label(&person_id(o), names))
        .unwrap_or_else(|| "its owner".into());
    // Everyone but the owner can be named; disabled people only stay struck through.
    let pickable: Vec<Person> = dir
        .people
        .iter()
        .filter(|p| Some(p.id.as_str()) != owner.as_deref())
        .filter(|p| p.state == PersonState::Active)
        .cloned()
        .collect();
    let own_agents: Vec<Agent> = dir
        .agents
        .iter()
        .filter(|a| owner.as_deref() == Some(a.owner_id.as_str()))
        .cloned()
        .collect();
    let deny_count = current.deny.len();
    let close = move || {
        if !*busy.peek() {
            on_close.call(());
        }
    };
    let mut toggle = move |side: Side, (entry, on): (String, bool)| {
        let mut next = draft.peek().clone();
        match next.toggle(side, &entry, on) {
            Ok(()) => {
                full.set(None);
                error.set(None);
                draft.set(next);
            }
            Err(m) => full.set(Some(m)),
        }
    };
    let unit = target.unit;
    let id = target.id.clone();
    let before = target.audience.clone();
    let save = move |confirmed: bool| {
        let id = id.clone();
        let before = before.clone();
        let owner = owner.clone();
        let what = what.clone();
        async move {
            if *busy.peek() {
                return;
            }
            let body = if *sharing.peek() {
                draft.peek().body()
            } else {
                Audience::default()
            };
            if let Err(m) = margin_shared::validate(&body) {
                error.set(Some(m));
                return;
            }
            if !confirmed {
                let after = org::stored_audience(owner.as_deref(), &body);
                let lib = app.lib.peek();
                let people: Vec<Person> = lib.people.values().cloned().collect();
                let losing = org::losing_readers(&people, owner.as_deref(), &before, &after);
                if !losing.is_empty() {
                    confirm.set(Some(losing_text(&losing, &what)));
                    return;
                }
            }
            let Some(api) = app.api() else { return };
            busy.set(true);
            error.set(None);
            match api.set_audience(unit, &id, &body).await {
                Ok(stored) => {
                    let mut lib = app.lib;
                    lib.write().apply_audience(unit, &id, &stored);
                    busy.set(false);
                    confirm.set(None);
                    on_close.call(());
                }
                Err(e) => {
                    error.set(Some(app.failure(&e)));
                    busy.set(false);
                    confirm.set(None);
                }
            }
        }
    };
    let save_now = save.clone();

    rsx! {
        div {
            class: "modal-scrim center sheet",
            onclick: move |e| {
                e.stop_propagation();
                close();
            },
            onkeydown: move |e: KeyboardEvent| {
                if e.key() == Key::Escape {
                    e.stop_propagation();
                    close();
                }
            },
            div {
                class: "modal org-dialog aud-dialog",
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": "aud-title",
                "aria-busy": busy.read().to_string(),
                onclick: move |e| e.stop_propagation(),
                h2 { id: "aud-title", "Who can read “{target.name}”" }
                p { class: "help", {unit_help(target.unit)} }
                if !mine {
                    p { class: "aud-now", "{stored_summary}" }
                    p { class: "faint", "Only {owner_label} changes who can read it." }
                    div { class: "row-actions",
                        button { class: "btn btn-primary btn-sm", onclick: move |_| on_close.call(()), "Close" }
                    }
                } else {
                    div { class: "seg", role: "radiogroup", "aria-label": "Who can read it",
                        button {
                            r#type: "button",
                            role: "radio",
                            "aria-checked": (!*sharing.read()).to_string(),
                            tabindex: if *sharing.read() { "-1" } else { "0" },
                            onclick: move |_| sharing.set(false),
                            "Only me"
                        }
                        button {
                            r#type: "button",
                            role: "radio",
                            "aria-checked": sharing.read().to_string(),
                            tabindex: if *sharing.read() { "0" } else { "-1" },
                            onclick: move |_| sharing.set(true),
                            "Share with…"
                        }
                    }
                    if *sharing.read() {
                        div { class: "aud-pickers",
                            CheckTree {
                                id: "aud-groups",
                                label: "Groups",
                                rows: audience_group_rows(&dir.groups, &current.access),
                                on_toggle: move |t| toggle(Side::Access, t),
                            }
                            PeoplePicker {
                                id: "aud-people",
                                label: "People",
                                people: pickable.clone(),
                                checked: current.access.clone(),
                                on_toggle: move |t| toggle(Side::Access, t),
                            }
                            if !own_agents.is_empty() {
                                CheckTree {
                                    id: "aud-agents",
                                    label: "My agents",
                                    rows: agent_rows(&own_agents, &current.access),
                                    on_toggle: move |t| toggle(Side::Access, t),
                                }
                            }
                            details { class: "aud-deny", open: deny_count > 0,
                                summary {
                                    if deny_count > 0 { "Never visible to ({deny_count})" } else { "Never visible to…" }
                                }
                                p { class: "help", "Someone here can't read it even through a group above." }
                                CheckTree {
                                    id: "aud-deny-groups",
                                    label: "Groups",
                                    rows: audience_group_rows(&dir.groups, &current.deny),
                                    on_toggle: move |t| toggle(Side::Deny, t),
                                }
                                PeoplePicker {
                                    id: "aud-deny-people",
                                    label: "People",
                                    people: pickable.clone(),
                                    checked: current.deny.clone(),
                                    on_toggle: move |t| toggle(Side::Deny, t),
                                }
                                if !own_agents.is_empty() {
                                    CheckTree {
                                        id: "aud-deny-agents",
                                        label: "My agents",
                                        rows: agent_rows(&own_agents, &current.deny),
                                        on_toggle: move |t| toggle(Side::Deny, t),
                                    }
                                }
                            }
                        }
                    }
                    p { class: "aud-now", role: "status", "Who can read it: {lower_first(&now_summary)}" }
                    if let Some(m) = full.read().clone() {
                        span { class: "field-error", role: "alert", "{m}" }
                    }
                    if let Some(err) = error.read().clone() {
                        div { class: "notice error", role: "alert", Icon { name: "alert" } div { class: "body", "{err}" } }
                    }
                    div { class: "row-actions",
                        button { r#type: "button", class: "btn btn-secondary btn-sm", disabled: *busy.read(), onclick: move |_| on_close.call(()), "Cancel" }
                        button {
                            r#type: "button",
                            class: "btn btn-primary btn-sm",
                            disabled: *busy.read(),
                            onclick: move |_| save_now(false),
                            if *busy.read() { span { class: "spinner" } }
                            "Save"
                        }
                    }
                }
                if let Some(message) = confirm.read().clone() {
                    ConfirmDialog {
                        title: "Remove readers from “{target.name}”?",
                        message: format!("{message} They stop seeing it at once, in search too."),
                        confirm_label: "Save audience",
                        busy: *busy.read(),
                        error: error.read().clone(),
                        on_cancel: move |_| confirm.set(None),
                        on_confirm: move |_| save(true),
                    }
                }
            }
        }
    }
}

/// The owner's agents as checkbox rows ("claude-code (External)").
pub fn agent_rows(agents: &[Agent], checked: &[String]) -> Vec<CheckRow> {
    let mut list: Vec<&Agent> = agents.iter().collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    list.into_iter()
        .map(|a| {
            let key = margin_shared::agent_id(&a.id);
            CheckRow {
                checked: checked.contains(&key),
                key,
                label: format!("{} ({})", a.name, org::kind_label(a.kind)),
                depth: 0,
                struck: false,
                note: a.disabled.then(|| "disabled".into()),
            }
        })
        .collect()
}

/// People to name, by handle: a search field over the directory, the checked first.
#[component]
fn PeoplePicker(
    id: String,
    label: String,
    people: Vec<Person>,
    checked: Vec<String>,
    on_toggle: EventHandler<(String, bool)>,
) -> Element {
    let mut query = use_signal(String::new);
    let q = query.read().trim().trim_start_matches('@').to_lowercase();
    let mut shown: Vec<(Person, bool)> = people
        .into_iter()
        .map(|p| {
            let on = checked.contains(&person_id(&p.id));
            (p, on)
        })
        .filter(|(p, on)| {
            *on || q.is_empty()
                || p.handle.to_lowercase().contains(&q)
                || p.name.to_lowercase().contains(&q)
        })
        .collect();
    shown.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.handle.cmp(&b.0.handle)));
    let search_id = format!("{id}-find");
    rsx! {
        fieldset { class: "check-tree aud-pick", id: "{id}",
            legend { class: "label", "{label}" }
            input {
                id: "{search_id}",
                class: "input",
                r#type: "search",
                autocomplete: "off",
                spellcheck: "false",
                placeholder: "Find someone by handle or name",
                "aria-label": "Find {label.to_lowercase()}",
                oninput: move |e| query.set(e.value()),
            }
            div { class: "aud-scroll", role: "group", "aria-label": "{label}",
                for (p, on) in shown.iter().cloned() {
                    label { key: "{p.id}", class: "check-row",
                        input {
                            r#type: "checkbox",
                            checked: on,
                            onchange: {
                                let key = person_id(&p.id);
                                move |e: FormEvent| on_toggle.call((key.clone(), e.checked()))
                            },
                        }
                        span { class: "check-label", "@{p.handle}" }
                        small { class: "faint", "{p.name}" }
                    }
                }
                if shown.is_empty() {
                    p { class: "faint", "No one matches." }
                }
            }
        }
    }
}

/// `GET routes::audience(Source, id)` answered 404: the reader can't open the source
/// any more (its audience changed after the lesson cited it, or it was deleted). Any
/// other answer, or no answer, counts as readable.
pub async fn source_unreadable(api: &crate::api::Api, source_id: &str) -> bool {
    matches!(api.audience(AudienceUnit::Source, source_id).await, Err(e) if e.status == Some(404))
}

/// What the viewer says for a cited source the reader can no longer open.
pub const SOURCE_UNREADABLE: &str =
    "You can't open this source any more. Its owner changed who can read it, or deleted it.";

/// The viewer's notice for a cited source the reader can no longer open.
#[component]
pub fn UnreadableSource() -> Element {
    rsx! {
        div { class: "notice", role: "status",
            Icon { name: "alert" }
            div { class: "body", "{SOURCE_UNREADABLE}" }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::fake::{ALICE, BOB, FakeState, TOKEN, TOKEN_BOB, sample_me};
    use crate::ui::settings::SettingsPane;
    use crate::ui::test_render::{Extra, render_with};

    /// The library as `person` reads it from the fake, with the directory, the chats and
    /// the settings loaded.
    pub fn lib_of(person: &str) -> LibraryState {
        let s = FakeState::new();
        let mut lib = LibraryState::from_library(s.library_for(person));
        lib.replace_people(s.people.clone());
        lib.replace_groups(s.groups.clone());
        lib.replace_agents(
            s.agents
                .iter()
                .filter(|a| person == ALICE || a.owner_id == person)
                .cloned()
                .collect(),
        );
        lib.org_loaded = true;
        lib.settings = Some(s.settings.clone());
        let chats = crate::fake::sample_chats();
        lib.replace_chats(chats.iter().map(|(c, _)| c.clone()).collect());
        for (chat, items) in chats {
            lib.chat_items.insert(chat.id, items);
        }
        lib
    }

    pub fn as_person(token: &str) -> Extra {
        Extra {
            me: Some(sample_me(token)),
            ..Default::default()
        }
    }

    fn dialog(html: &str) -> &str {
        let at = html.find("aud-dialog").expect("the Audience control");
        &html[at..]
    }

    #[test]
    fn the_owner_picks_groups_people_agents_and_denials() {
        let lib = lib_of(ALICE);
        let target = AudienceTarget::topic(&lib.topics["signals"]);
        let html = render_with(
            "/",
            lib,
            None,
            Extra {
                audience: Some(target),
                ..as_person(TOKEN)
            },
        );
        // A dialog on computers, a sheet on phones (the CSS's `.modal-scrim.sheet`).
        assert!(
            html.contains("class=\"modal-scrim center sheet\""),
            "{html}"
        );
        let d = dialog(&html);
        assert!(
            d.contains("Who can read “Signals ") && d.contains("Systems”"),
            "{d}"
        );
        assert!(d.contains("Its subtopics, lessons, notes and sources follow it."));
        // Shared, with Engineering ticked; everyone else can be picked.
        assert!(d.contains(r#"aria-checked="true""#) && d.contains(">Share with…<"));
        assert!(d.contains(">Only me<"));
        assert!(d.contains("Engineering (company/engineering)"));
        assert!(d.contains("Company (everyone)"));
        assert!(
            d.contains("@bob") && !d.contains(">@alice<"),
            "the owner isn't offered"
        );
        assert!(d.contains("claude-code (External)"), "her own agents");
        assert!(!d.contains("triage-bot"), "not Bob's agents");
        assert!(d.contains("Never visible to…"));
        assert!(d.contains("Who can read it: @engineering"));
        assert!(d.contains(">Save<") && d.contains(">Cancel<"));
        assert!(d.contains("type=\"search\""), "the people picker filters");
    }

    #[test]
    fn a_member_reads_the_summary_and_who_changes_it() {
        let lib = lib_of(BOB);
        let target = AudienceTarget::topic(&lib.topics["signals"]);
        let html = render_with(
            "/",
            lib,
            None,
            Extra {
                audience: Some(target),
                ..as_person(TOKEN_BOB)
            },
        );
        let d = dialog(&html);
        assert!(d.contains("@engineering"), "{d}");
        assert!(d.contains("Only @alice changes who can read it."));
        assert!(!d.contains("type=\"checkbox\"") && !d.contains(">Save<"));
        assert!(d.contains(">Close<"));
    }

    #[test]
    fn lessons_and_notes_follow_their_topic() {
        // The owner: the line, with Change…
        let html = render_with("/lesson/l1", lib_of(ALICE), None, as_person(TOKEN));
        assert!(
            html.contains("Audience: follows ‘Fourier series’ (@engineering)"),
            "{html}"
        );
        assert!(html.contains("aria-label=\"Change who can read Fourier series\""));
        // A member reads the line only.
        let html = render_with("/lesson/l1", lib_of(BOB), None, as_person(TOKEN_BOB));
        assert!(html.contains("Audience: follows ‘Fourier series’ (@engineering)"));
        assert!(!html.contains("Change who can read"));
        // Narrowed: visible only to its owner, with Publish…
        let html = render_with("/lesson/l2", lib_of(ALICE), None, as_person(TOKEN));
        assert!(html.contains(
            "Visible only to you: this lesson draws on documents with different audiences. Publish the topic to share it."
        ));
        assert!(html.contains(">Publish…<"));
        assert!(!html.contains("Audience: follows"));
        // Bob can't read it at all.
        let html = render_with("/lesson/l2", lib_of(BOB), None, as_person(TOKEN_BOB));
        assert!(
            html.contains("This lesson isn&#39;t here any more"),
            "{html}"
        );
        assert!(html.contains("or its topic&#39;s owner changed who can read it"));
        // A note.
        let html = render_with("/note/n1", lib_of(ALICE), None, as_person(TOKEN));
        assert!(html.contains("Audience: follows ‘Fourier series’ (@engineering)"));
    }

    #[test]
    fn chats_say_they_are_private() {
        let html = render_with("/chat/c1", lib_of(ALICE), None, as_person(TOKEN));
        assert!(html.contains("Private to you"), "{html}");
    }

    #[test]
    fn a_file_of_no_topic_has_its_own_audience_in_the_chat() {
        let mut lib = lib_of(ALICE);
        lib.chats.get_mut("c1").unwrap().source_ids = vec!["s8".into()];
        let html = render_with("/chat/c1", lib, None, as_person(TOKEN));
        let at = html.find("interview-notes.md").expect("the attached file");
        let rest = &html[at..];
        assert!(rest.contains("Only you"), "{rest}");
        assert!(rest.contains("Change who can read interview-notes.md"));
    }

    #[test]
    fn folders_show_who_reads_them() {
        let mut lib = lib_of(ALICE);
        lib.replace_workspaces(crate::fake::sample_workspaces());
        let html = render_with("/", lib, Some(SettingsPane::Folders), as_person(TOKEN));
        let notes = &html[html.find("~/Documents/notes").expect("w1")..];
        assert!(notes.contains("Only you"), "{notes}");
        assert!(html.contains("Change who can read Papers"));
        let papers = &html[html.find("~/Papers").expect("w2")..];
        assert!(papers.contains("@engineering"));
    }

    #[test]
    fn agents_list_their_documents() {
        let mut lib = lib_of(ALICE);
        let s = FakeState::new();
        lib.replace_agent_documents(
            s.documents
                .iter()
                .filter(|d| s.person_reads(ALICE, &s.document_audience(d).1))
                .cloned()
                .collect(),
        );
        let html = render_with("/", lib, Some(SettingsPane::Organisation), as_person(TOKEN));
        assert!(html.contains("Documents (2)"), "{html}");
        assert!(html.contains("margin-architecture.md") && html.contains("deploy-runbook.md"));
        assert!(html.contains("Change who can read deploy-runbook.md"));
        assert!(html.contains("aria-label=\"Delete margin-architecture.md\""));
        // Bob's agent: nothing Alice can read.
        assert!(html.contains("Documents (0)"));
        assert!(!html.contains("triage-notes.md"));
    }

    #[test]
    fn helpers_say_what_follows_and_who_loses_access() {
        let lib = lib_of(ALICE);
        let topic = AudienceTarget::topic(&lib.topics["signals"]);
        assert_eq!(follows_what(&lib, &topic), "4 lessons");
        let linalg = AudienceTarget::topic(&lib.topics["linalg"]);
        assert_eq!(follows_what(&lib, &linalg), "this topic");
        let people = crate::fake::sample_people();
        let bob = &people[1];
        assert_eq!(
            losing_text(&[bob], "4 lessons"),
            "@bob will lose access to 4 lessons."
        );
        assert_eq!(
            losing_text(&[bob, bob, bob], "this folder"),
            "3 people will lose access to this folder."
        );
        assert_eq!(
            lower_first("Everyone at the company"),
            "everyone at the company"
        );
        assert_eq!(lower_first("@engineering"), "@engineering");
        let rows = audience_group_rows(
            &crate::fake::sample_groups(),
            &["company".into(), "company/gone".into()],
        );
        assert!(rows[0].checked && rows[0].label == "Company (everyone)");
        assert_eq!(
            rows[1].note.as_deref(),
            Some("already included through Company")
        );
        assert!(rows[2].struck && rows[2].checked);
    }
}
