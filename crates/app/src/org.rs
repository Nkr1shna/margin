//! Accounts and principals as the app shows them (L1, `docs/l1-brief.md` D10): a
//! person's expanded access, agent ceilings, the group tree, and the words for roles,
//! states and kinds. Pure functions, shared by Settings → Organisation and the fake.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use margin_shared::context::{AuditKind, AuditRow};
use margin_shared::{
    Agent, AgentKind, Audience, Group, Invitation, OrgRole, Person, PersonState, ROOT_GROUP,
    ancestors, is_group_path, person_id,
};

/// What `person` may read: `person/<id>`, `company`, and every group they're in with its
/// ancestors; sorted, without repeats (the service's `Expansion.paths`).
pub fn expansion(person: &Person) -> Vec<String> {
    let mut paths: BTreeSet<String> = BTreeSet::new();
    paths.insert(person_id(&person.id));
    paths.insert(ROOT_GROUP.to_string());
    for group in &person.groups {
        paths.extend(ancestors(group));
    }
    paths.into_iter().collect()
}

/// The ceiling to store for `asked` (D7): each entry with its ancestors, all inside
/// `owner_paths`. `Err` names the first entry outside them, or says the ceiling is empty.
pub fn normalise_ceiling(owner_paths: &[String], asked: &[String]) -> Result<Vec<String>, String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for entry in asked {
        let closed = if is_group_path(entry) {
            ancestors(entry)
        } else {
            vec![entry.clone()]
        };
        for path in closed {
            if !owner_paths.contains(&path) {
                return Err(format!(
                    "{path} is outside your access: an agent can only act within what you can read."
                ));
            }
            out.insert(path);
        }
    }
    if out.is_empty() {
        return Err("Give the agent at least one thing it may read.".into());
    }
    Ok(out.into_iter().collect())
}

/// An agent's ceiling when none is asked for (Q5): Margin's own agent gets the owner's
/// whole access; external and voice agents everything but the owner's private notes and
/// chats.
pub fn default_ceiling(kind: AgentKind, owner: &Person) -> Vec<String> {
    let paths = expansion(owner);
    match kind {
        AgentKind::Margin => paths,
        AgentKind::External | AgentKind::Voice => {
            let private = person_id(&owner.id);
            paths.into_iter().filter(|p| *p != private).collect()
        }
    }
}

/// A ceiling with `path` checked or unchecked, kept ancestor-closed: checking a group
/// checks its ancestors; unchecking one unchecks the groups under it.
pub fn toggle_ceiling(ceiling: &[String], path: &str, on: bool) -> Vec<String> {
    let mut set: BTreeSet<String> = ceiling.iter().cloned().collect();
    if on {
        if is_group_path(path) {
            set.extend(ancestors(path));
        } else {
            set.insert(path.to_string());
        }
    } else {
        let under = format!("{path}/");
        set.retain(|p| p != path && !(is_group_path(path) && p.starts_with(&under)));
    }
    set.into_iter().collect()
}

pub fn role_label(role: OrgRole) -> &'static str {
    match role {
        OrgRole::Owner => "Owner",
        OrgRole::Admin => "Admin",
        OrgRole::Member => "Member",
    }
}

pub fn state_label(state: PersonState) -> &'static str {
    match state {
        PersonState::Invited => "Invited",
        PersonState::Active => "Active",
        PersonState::Disabled => "Disabled",
    }
}

pub const AGENT_KINDS: [AgentKind; 3] = [AgentKind::External, AgentKind::Margin, AgentKind::Voice];

pub fn kind_label(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Margin => "Margin",
        AgentKind::External => "External",
        AgentKind::Voice => "Voice",
    }
}

/// What a kind of agent is, and what it reads by default.
pub fn kind_help(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Margin => {
            "Margin's own agent. By default it reads everything you can, your private notes and chats included."
        }
        AgentKind::External => {
            "An outside agent, like Claude Code through margin-mcp. By default it reads what your groups share, not your private notes and chats."
        }
        AgentKind::Voice => {
            "A voice agent. By default it reads what your groups share, not your private notes and chats."
        }
    }
}

/// How Settings names an access path: "My private notes and chats" for the owner's own,
/// a group by its name and path, a person by handle.
pub fn path_label(path: &str, me: &str, groups: &[Group], people: &[Person]) -> String {
    if path == person_id(me) {
        return "My private notes and chats".into();
    }
    if let Some(id) = path.strip_prefix(margin_shared::PERSON_PREFIX) {
        return match people.iter().find(|p| p.id == id) {
            Some(p) => format!("@{}'s private notes and chats", p.handle),
            None => "Someone's private notes and chats".into(),
        };
    }
    match groups.iter().find(|g| g.path == path) {
        Some(g) => format!("{} ({})", g.name, g.path),
        None => path.to_string(),
    }
}

/// How deep a group sits: `company` is 0.
pub fn depth(path: &str) -> usize {
    path.matches('/').count()
}

/// Groups in tree order (each after its parent, siblings by path).
pub fn tree_order(groups: &[Group]) -> Vec<Group> {
    let mut list = groups.to_vec();
    list.sort_by(|a, b| a.path.split('/').cmp(b.path.split('/')));
    list
}

/// Why a group can't be deleted, if it can't: the root, groups under it, members.
pub fn delete_blocker(group: &Group, groups: &[Group]) -> Option<String> {
    if group.path == ROOT_GROUP {
        return Some("The company group is everyone; it can't be deleted.".into());
    }
    if group.source != margin_shared::SOURCE_MANUAL {
        return Some(format!("It comes from {}; change it there.", group.source));
    }
    let under = format!("{}/", group.path);
    let children = groups.iter().filter(|g| g.path.starts_with(&under)).count();
    if children > 0 {
        return Some(format!(
            "Delete the {} under it first.",
            plural(children, "group", "groups")
        ));
    }
    if group.members > 0 {
        return Some(format!(
            "Take its {} out first (Edit groups… on each).",
            plural(group.members as usize, "member", "members")
        ));
    }
    None
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// A new group's slug, checked before sending: the full path it makes, or what's wrong.
pub fn new_group_path(parent: &str, slug: &str, groups: &[Group]) -> Result<String, String> {
    let slug = slug.trim();
    if slug.is_empty() {
        return Err("Enter a short name for the path, like engineering.".into());
    }
    let path = format!("{parent}/{slug}");
    if !is_group_path(&path) {
        return Err(if depth(&path) >= margin_shared::GROUP_PATH_MAX_SEGMENTS {
            "Groups nest at most 8 deep.".into()
        } else {
            "Use lowercase letters, digits and dashes (up to 40), like design-team.".into()
        });
    }
    if groups.iter().any(|g| g.path == path) {
        return Err(format!("{path} already exists."));
    }
    Ok(path)
}

/// A GitHub login: 1..=39 letters, digits and single dashes, not at either end.
pub fn is_github_login(login: &str) -> bool {
    (1..=39).contains(&login.len())
        && !login.starts_with('-')
        && !login.ends_with('-')
        && !login.contains("--")
        && login
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Why an invitation can't be sent for `login`, checked before sending.
pub fn invitation_problem(
    login: &str,
    people: &[Person],
    invitations: &[Invitation],
    now: DateTime<Utc>,
) -> Option<String> {
    let login = login.trim().trim_start_matches('@');
    if login.is_empty() {
        return Some("Enter their GitHub login.".into());
    }
    if !is_github_login(login) {
        return Some("That isn't a GitHub login: letters, digits and dashes, up to 39.".into());
    }
    let key = login.to_ascii_lowercase();
    if let Some(p) = people
        .iter()
        .find(|p| p.github_login.to_ascii_lowercase() == key)
    {
        return Some(if p.state == PersonState::Disabled {
            format!("@{} is a disabled member: enable them instead.", p.handle)
        } else {
            format!("@{} is already a member.", p.handle)
        });
    }
    if invitations
        .iter()
        .any(|i| i.github_login.to_ascii_lowercase() == key && is_pending(i, now))
    {
        return Some(format!("@{login} is already invited."));
    }
    None
}

/// An invitation not yet accepted and not expired.
pub fn is_pending(invitation: &Invitation, now: DateTime<Utc>) -> bool {
    invitation.accepted_at.is_none() && invitation.expires_at > now
}

/// The signed-in person's row after `incoming` (an event, or an answer that may leave out
/// their settings) arrived, and whether their settings must be fetched
/// (`GET /api/me/settings`). Events never carry anyone's settings, so a row with the
/// default settings can't tell "reset on another device" from "not included": it keeps
/// the known settings for now and asks for them.
pub fn merge_own_person(known: &Person, incoming: Person) -> (Person, bool) {
    let mut person = incoming;
    let unknown = person.settings == margin_shared::PersonSettings::default();
    if unknown {
        person.settings = known.settings.clone();
    }
    (person, unknown)
}

/// The command that gives `margin-mcp` an agent token: it reads the token from stdin
/// (pasted when it asks), so the token stays out of the shell history.
pub fn token_command(service_url: &str) -> String {
    format!("margin-mcp token - --url {service_url}")
}

// ---------------------------------------------------------------------------
// Audiences (L2, `docs/l2-brief.md` D3, D9)
// ---------------------------------------------------------------------------

/// A row's audience as stored: read empty, it is private to its owner.
pub fn audience_of(owner: Option<&str>, access: &[String], deny: &[String]) -> Audience {
    let access = if access.is_empty() {
        owner.map(|o| vec![person_id(o)]).unwrap_or_default()
    } else {
        access.to_vec()
    };
    Audience {
        access,
        deny: deny.to_vec(),
    }
}

/// Whether `person` reads `audience` (exact membership over their expansion).
pub fn person_reads(person: &Person, audience: &Audience) -> bool {
    let paths = expansion(person);
    margin_shared::reads(&paths, &paths, audience)
}

/// The active people other than the owner who read `before` and won't read `after`.
pub fn losing_readers<'a>(
    people: &'a [Person],
    owner: Option<&str>,
    before: &Audience,
    after: &Audience,
) -> Vec<&'a Person> {
    people
        .iter()
        .filter(|p| p.state == PersonState::Active && Some(p.id.as_str()) != owner)
        .filter(|p| person_reads(p, before) && !person_reads(p, after))
        .collect()
}

/// Who an audience can name, for its words: the directory, the groups and the agents.
#[derive(Clone, Copy)]
pub struct Names<'a> {
    pub groups: &'a [Group],
    pub people: &'a [Person],
    pub agents: &'a [Agent],
}

/// One audience entry in words: a group as "@engineering", a person as "@bob", an agent
/// by its name.
pub fn entry_label(entry: &str, names: Names<'_>) -> String {
    if let Some(id) = entry.strip_prefix(margin_shared::PERSON_PREFIX) {
        return names
            .people
            .iter()
            .find(|p| p.id == id)
            .map(|p| format!("@{}", p.handle))
            .unwrap_or_else(|| "someone".into());
    }
    if let Some(id) = entry.strip_prefix(margin_shared::AGENT_PREFIX) {
        return names
            .agents
            .iter()
            .find(|a| a.id == id)
            .map(|a| a.name.clone())
            .unwrap_or_else(|| "an agent".into());
    }
    if entry == ROOT_GROUP {
        return "everyone at the company".into();
    }
    format!("@{}", entry.rsplit('/').next().unwrap_or(entry))
}

fn count(n: usize, one: &str, many: &str) -> String {
    plural(n, one, many)
}

/// "@a", "@a and @b", "@a, @b and 3 more".
fn listed(labels: &[String]) -> String {
    match labels {
        [] => String::new(),
        [one] => one.clone(),
        [a, b] => format!("{a} and {b}"),
        [a, b, rest @ ..] => format!("{a}, {b} and {} more", rest.len()),
    }
}

/// Who reads an audience, in words, as the summary line says it everywhere: "Only
/// you", "Everyone at the company", "@engineering and 2 people", "Everyone except
/// @sales". `owner` is the row's owner (always a reader, so never named); `me` the
/// signed-in person.
pub fn audience_summary(
    audience: &Audience,
    owner: Option<&str>,
    me: Option<&str>,
    names: Names<'_>,
) -> String {
    let own = owner.map(person_id);
    let others: Vec<&String> = audience
        .access
        .iter()
        .filter(|e| Some(*e) != own.as_ref())
        .collect();
    let denied: Vec<String> = audience
        .deny
        .iter()
        .filter(|e| Some(*e) != own.as_ref())
        .map(|e| entry_label(e, names))
        .collect();
    if others.is_empty() {
        return match (owner, me) {
            (Some(o), Some(m)) if o == m => "Only you".into(),
            (Some(o), _) => format!("Only {}", entry_label(&person_id(o), names)),
            (None, _) => "Only its owner".into(),
        };
    }
    if others.iter().any(|e| e.as_str() == ROOT_GROUP) {
        return if denied.is_empty() {
            "Everyone at the company".into()
        } else {
            format!("Everyone except {}", listed(&denied))
        };
    }
    let groups: Vec<String> = others
        .iter()
        .filter(|e| is_group_path(e))
        .map(|e| entry_label(e, names))
        .collect();
    let people: Vec<String> = others
        .iter()
        .filter(|e| e.starts_with(margin_shared::PERSON_PREFIX))
        .map(|e| entry_label(e, names))
        .collect();
    let agents: Vec<String> = others
        .iter()
        .filter(|e| e.starts_with(margin_shared::AGENT_PREFIX))
        .map(|e| entry_label(e, names))
        .collect();
    let mut parts: Vec<String> = Vec::new();
    if groups.is_empty() && people.len() <= 2 {
        parts.extend(people);
    } else {
        parts.extend(groups);
        if !people.is_empty() {
            parts.push(count(people.len(), "person", "people"));
        }
    }
    if agents.len() == 1 && parts.is_empty() {
        parts.push(agents[0].clone());
    } else if !agents.is_empty() {
        parts.push(count(agents.len(), "agent", "agents"));
    }
    let mut text = match parts.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    };
    if !denied.is_empty() {
        text = format!("{text}, except {}", listed(&denied));
    }
    text
}

/// What the service will store for `asked` on a row of `owner` (D3): the owner always
/// reads it and is never denied.
pub fn stored_audience(owner: Option<&str>, asked: &Audience) -> Audience {
    let own = owner.map(person_id);
    let mut access: Vec<String> = own.iter().cloned().collect();
    for e in &asked.access {
        if !access.contains(e) {
            access.push(e.clone());
        }
    }
    let deny = asked
        .deny
        .iter()
        .filter(|e| Some(*e) != own.as_ref())
        .cloned()
        .collect();
    Audience { access, deny }
}

/// Which list of an audience an entry is checked in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Who may read it.
    Access,
    /// Never visible to.
    Deny,
}

/// The Audience control's choices, without the owner (who always reads the row).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudienceDraft {
    pub access: Vec<String>,
    pub deny: Vec<String>,
}

impl AudienceDraft {
    /// The control over a stored audience: the owner's own entry left out.
    pub fn from_stored(audience: &Audience, owner: Option<&str>) -> Self {
        let own = owner.map(person_id);
        let keep = |list: &[String]| -> Vec<String> {
            list.iter()
                .filter(|e| Some(*e) != own.as_ref())
                .cloned()
                .collect()
        };
        Self {
            access: keep(&audience.access),
            deny: keep(&audience.deny),
        }
    }

    /// Private to the owner: nothing shared, nothing to deny.
    pub fn only_me(&self) -> bool {
        self.access.is_empty()
    }

    /// Checks or unchecks `entry` in one list. An entry is in one list at most: checking
    /// it in one takes it out of the other. `Err` when the list is full.
    pub fn toggle(&mut self, side: Side, entry: &str, on: bool) -> Result<(), String> {
        let (list, other) = match side {
            Side::Access => (&mut self.access, &mut self.deny),
            Side::Deny => (&mut self.deny, &mut self.access),
        };
        if !on {
            list.retain(|e| e != entry);
            return Ok(());
        }
        if list.iter().any(|e| e == entry) {
            return Ok(());
        }
        if list.len() >= margin_shared::AUDIENCE_MAX {
            return Err(format!(
                "An audience can name at most {} in each list. Use a group instead.",
                margin_shared::AUDIENCE_MAX
            ));
        }
        other.retain(|e| e != entry);
        list.push(entry.to_string());
        Ok(())
    }

    /// The body of `PUT routes::audience`: nothing denied when nothing is shared.
    pub fn body(&self) -> Audience {
        Audience {
            access: self.access.clone(),
            deny: if self.only_me() {
                Vec::new()
            } else {
                self.deny.clone()
            },
        }
    }
}

// ---------------------------------------------------------------------------
// The audit trail (L2 D7), as the Audit view shows it
// ---------------------------------------------------------------------------

pub fn audit_kind_label(kind: AuditKind) -> &'static str {
    match kind {
        AuditKind::AudienceChange => "Audience changed",
        AuditKind::MembershipChange => "Groups changed",
        AuditKind::GroupChange => "Group changed",
        AuditKind::PersonChange => "Person changed",
        AuditKind::SignIn => "Sign-in",
        AuditKind::DeviceRevoked => "Device revoked",
        AuditKind::AgentChange => "Agent changed",
        AuditKind::AgentToken => "Agent token",
        AuditKind::Export => "Export",
    }
}

/// Who made a change: "@alice", "claude-code (agent of @alice)", "root on the service
/// host".
pub fn actor_label(row: &AuditRow, names: Names<'_>) -> String {
    if let Some(user) = row.actor.strip_prefix("host/") {
        return format!("{user} on the service host");
    }
    let who = entry_label(&row.actor, names);
    match &row.on_behalf_of {
        Some(owner) if row.actor.starts_with(margin_shared::AGENT_PREFIX) => {
            format!("{who} (agent of {})", entry_label(owner, names))
        }
        _ => who,
    }
}

/// What a row is about, by name when the service joined it, else by kind ("a topic").
pub fn subject_label(row: &AuditRow, names: Names<'_>) -> String {
    if let Some(name) = &row.subject_name {
        return name.clone();
    }
    let id = &row.subject.id;
    match row.subject.kind.as_str() {
        "person" => entry_label(&person_id(id), names),
        "group" => names
            .groups
            .iter()
            .find(|g| g.path == *id)
            .map(|g| g.name.clone())
            .unwrap_or_else(|| id.clone()),
        "agent" => entry_label(&margin_shared::agent_id(id), names),
        "workspace" => "a folder".into(),
        "topic" => "a topic".into(),
        "source" => "a file".into(),
        "document" => "a document".into(),
        other => format!("a {other}"),
    }
}

/// The change as a diff: entries added (+) and removed (−) for each list, a value that
/// moved ("role: member → admin"), else what it is now.
pub fn audit_change(row: &AuditRow, names: Names<'_>) -> String {
    use serde_json::Value;
    let empty = serde_json::Map::new();
    let before = row.before.as_object().unwrap_or(&empty);
    let after = row.after.as_object().unwrap_or(&empty);
    let mut keys: Vec<&String> = before.keys().chain(after.keys()).collect();
    keys.sort();
    keys.dedup();
    let word = |v: &Value| match v {
        Value::String(s) if s.contains('/') || s == ROOT_GROUP => entry_label(s, names),
        Value::String(s) => s.clone(),
        Value::Null => "none".into(),
        other => other.to_string(),
    };
    let mut parts: Vec<String> = Vec::new();
    for key in keys {
        let (b, a) = (before.get(key), after.get(key));
        if b == a {
            continue;
        }
        let list = |v: Option<&Value>| -> Option<Vec<Value>> {
            match v {
                Some(Value::Array(items)) => Some(items.clone()),
                None | Some(Value::Null) => Some(Vec::new()),
                _ => None,
            }
        };
        match (list(b), list(a)) {
            (Some(b), Some(a)) if !(b.is_empty() && a.is_empty()) => {
                let added: Vec<String> = a.iter().filter(|x| !b.contains(x)).map(word).collect();
                let removed: Vec<String> = b.iter().filter(|x| !a.contains(x)).map(word).collect();
                let mut text = format!("{key}:");
                for x in added {
                    text.push_str(&format!(" +{x}"));
                }
                for x in removed {
                    text.push_str(&format!(" −{x}"));
                }
                parts.push(text);
            }
            _ => {
                let from = b.map(word).unwrap_or_else(|| "none".into());
                let to = a.map(word).unwrap_or_else(|| "none".into());
                parts.push(format!("{key}: {from} → {to}"));
            }
        }
    }
    if parts.is_empty() {
        "No change recorded".into()
    } else {
        parts.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use margin_shared::{PersonSettings, SOURCE_MANUAL};

    fn person(id: &str, groups: &[&str]) -> Person {
        Person {
            id: id.into(),
            github_login: id.into(),
            github_id: None,
            handle: id.into(),
            name: id.into(),
            avatar_url: None,
            org_role: OrgRole::Member,
            state: PersonState::Active,
            groups: groups.iter().map(|g| g.to_string()).collect(),
            settings: PersonSettings::default(),
            created_at: Utc.timestamp_opt(0, 0).unwrap(),
        }
    }

    fn group(path: &str, members: u32) -> Group {
        Group {
            path: path.into(),
            name: path.rsplit('/').next().unwrap().into(),
            source: SOURCE_MANUAL.into(),
            members,
            created_at: Utc.timestamp_opt(0, 0).unwrap(),
        }
    }

    #[test]
    fn own_settings_are_fetched_when_a_row_cant_say() {
        let mut known = person("alice", &[]);
        known.settings.reasoning_effort = Some(margin_shared::ReasoningEffort::High);
        // An event (never with settings): the known ones stay for now, and are fetched,
        // since another device may have reset them.
        let mut event = person("alice", &[]);
        event.name = "Alice M.".into();
        let (merged, fetch) = merge_own_person(&known, event);
        assert_eq!(merged.name, "Alice M.");
        assert_eq!(merged.settings, known.settings);
        assert!(fetch);
        // An answer with the settings: taken as they are.
        let mut answer = person("alice", &[]);
        answer.settings.reasoning_effort = Some(margin_shared::ReasoningEffort::Low);
        let (merged, fetch) = merge_own_person(&known, answer.clone());
        assert_eq!(merged.settings, answer.settings);
        assert!(!fetch);
    }

    #[test]
    fn expansion_has_self_company_and_ancestors() {
        let p = person("bob", &["company/eng/web"]);
        assert_eq!(
            expansion(&p),
            vec!["company", "company/eng", "company/eng/web", "person/bob"]
        );
        assert_eq!(expansion(&person("a", &[])), vec!["company", "person/a"]);
    }

    #[test]
    fn ceilings_are_closed_and_inside_the_owner() {
        let owner = person("a", &["company/eng"]);
        let paths = expansion(&owner);
        assert_eq!(
            normalise_ceiling(&paths, &["company/eng".into()]).unwrap(),
            vec!["company", "company/eng"]
        );
        let err = normalise_ceiling(&paths, &["company/sales".into()]).unwrap_err();
        assert!(err.starts_with("company/sales is outside"), "{err}");
        assert!(normalise_ceiling(&paths, &[]).is_err());
        assert_eq!(
            default_ceiling(AgentKind::External, &owner),
            vec!["company", "company/eng"]
        );
        assert_eq!(default_ceiling(AgentKind::Margin, &owner), paths);
        // Checking a group checks its parents; unchecking one drops what's under it.
        let c = toggle_ceiling(&["person/a".into()], "company/eng", true);
        assert_eq!(c, vec!["company", "company/eng", "person/a"]);
        assert_eq!(toggle_ceiling(&c, "company", false), vec!["person/a"]);
        assert_eq!(
            toggle_ceiling(&c, "person/a", false),
            vec!["company", "company/eng"]
        );
    }

    #[test]
    fn groups_tree_and_deletion() {
        let groups = vec![
            group("company/eng", 0),
            group("company", 2),
            group("company/eng-ops", 0),
            group("company/eng/web", 1),
        ];
        let order: Vec<String> = tree_order(&groups).into_iter().map(|g| g.path).collect();
        assert_eq!(
            order,
            vec![
                "company",
                "company/eng",
                "company/eng/web",
                "company/eng-ops"
            ]
        );
        assert!(
            delete_blocker(&groups[1], &groups)
                .unwrap()
                .contains("can't be deleted")
        );
        assert_eq!(
            delete_blocker(&groups[0], &groups).unwrap(),
            "Delete the 1 group under it first."
        );
        assert_eq!(
            delete_blocker(&groups[3], &groups).unwrap(),
            "Take its 1 member out first (Edit groups… on each)."
        );
        assert_eq!(delete_blocker(&groups[2], &groups), None);
        assert_eq!(
            new_group_path("company", "design", &groups).unwrap(),
            "company/design"
        );
        assert!(new_group_path("company", "Design", &groups).is_err());
        assert!(new_group_path("company", "", &groups).is_err());
        assert!(
            new_group_path("company", "eng", &groups)
                .unwrap_err()
                .contains("exists")
        );
    }

    #[test]
    fn invitations_are_checked_before_sending() {
        let now = Utc.timestamp_opt(1000, 0).unwrap();
        let people = vec![person("bob", &[])];
        let pending = Invitation {
            id: "i".into(),
            github_login: "Carol-Dev".into(),
            role: OrgRole::Member,
            groups: vec![],
            invited_by: "a".into(),
            created_at: now,
            expires_at: now + chrono::Duration::days(30),
            accepted_at: None,
            accepted_by: None,
        };
        let invs = vec![pending.clone()];
        assert_eq!(invitation_problem("dave", &people, &invs, now), None);
        assert_eq!(invitation_problem("@dave", &people, &invs, now), None);
        assert!(invitation_problem("", &people, &invs, now).is_some());
        assert!(invitation_problem("bad login", &people, &invs, now).is_some());
        assert!(invitation_problem("-x", &people, &invs, now).is_some());
        assert_eq!(
            invitation_problem("BOB", &people, &invs, now).unwrap(),
            "@bob is already a member."
        );
        assert_eq!(
            invitation_problem("carol-dev", &people, &invs, now).unwrap(),
            "@carol-dev is already invited."
        );
        // An expired invitation can be sent again.
        let later = now + chrono::Duration::days(31);
        assert!(!is_pending(&pending, later));
        assert_eq!(invitation_problem("carol-dev", &people, &invs, later), None);
        assert_eq!(
            token_command("https://m.example"),
            "margin-mcp token - --url https://m.example"
        );
    }

    fn aud(access: &[&str], deny: &[&str]) -> Audience {
        Audience {
            access: access.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn agent(id: &str, owner: &str, name: &str) -> Agent {
        Agent {
            id: id.into(),
            owner_id: owner.into(),
            name: name.into(),
            kind: AgentKind::External,
            ceiling: vec![ROOT_GROUP.into()],
            disabled: false,
            token: None,
            created_at: Utc.timestamp_opt(0, 0).unwrap(),
        }
    }

    #[test]
    fn audiences_read_as_the_summary_line() {
        let people = vec![
            person("alice", &["company/eng"]),
            person("bob", &["company/sales"]),
            person("carol", &[]),
            person("dave", &[]),
        ];
        let groups = vec![
            group("company", 4),
            group("company/eng", 1),
            group("company/sales", 1),
        ];
        let agents = vec![agent("x", "alice", "claude-code")];
        let names = Names {
            groups: &groups,
            people: &people,
            agents: &agents,
        };
        let s = |a: &Audience, me: &str| audience_summary(a, Some("alice"), Some(me), names);
        // Read empty, a row is its owner's alone.
        let private = audience_of(Some("alice"), &[], &[]);
        assert_eq!(private.access, ["person/alice"]);
        assert_eq!(s(&private, "alice"), "Only you");
        assert_eq!(s(&private, "bob"), "Only @alice");
        assert_eq!(
            s(&aud(&["person/alice", "company"], &[]), "alice"),
            "Everyone at the company"
        );
        assert_eq!(
            s(
                &aud(&["person/alice", "company"], &["company/sales"]),
                "alice"
            ),
            "Everyone except @sales"
        );
        assert_eq!(
            s(
                &aud(
                    &["person/alice", "company/eng", "person/bob", "person/carol"],
                    &[]
                ),
                "alice"
            ),
            "@eng and 2 people"
        );
        assert_eq!(
            s(&aud(&["person/alice", "person/bob"], &[]), "alice"),
            "@bob"
        );
        assert_eq!(
            s(
                &aud(
                    &["person/alice", "person/bob", "person/carol", "person/dave"],
                    &[]
                ),
                "alice"
            ),
            "3 people"
        );
        assert_eq!(
            s(&aud(&["person/alice", "agent/x"], &[]), "alice"),
            "claude-code"
        );
        assert_eq!(
            s(
                &aud(&["person/alice", "company/eng"], &["person/bob"]),
                "alice"
            ),
            "@eng, except @bob"
        );
        assert_eq!(entry_label("company", names), "everyone at the company");
        assert_eq!(entry_label("person/zed", names), "someone");
    }

    #[test]
    fn the_control_keeps_the_owner_and_one_list_per_entry() {
        let stored = aud(&["person/alice", "company/eng"], &["person/bob"]);
        let mut d = AudienceDraft::from_stored(&stored, Some("alice"));
        assert_eq!(d.access, ["company/eng"]);
        assert_eq!(d.deny, ["person/bob"]);
        // Sharing with Bob takes him out of the denials.
        d.toggle(Side::Access, "person/bob", true).unwrap();
        assert_eq!(d.access, ["company/eng", "person/bob"]);
        assert!(d.deny.is_empty());
        d.toggle(Side::Access, "company/eng", false).unwrap();
        assert_eq!(d.access, ["person/bob"]);
        // Only me sends nothing, denials included.
        let only = AudienceDraft {
            access: vec![],
            deny: vec!["person/bob".into()],
        };
        assert!(only.only_me());
        assert_eq!(only.body(), Audience::default());
        // At most AUDIENCE_MAX in a list.
        let mut full = AudienceDraft::default();
        for i in 0..margin_shared::AUDIENCE_MAX {
            full.toggle(Side::Access, &format!("person/p{i}"), true)
                .unwrap();
        }
        assert!(full.toggle(Side::Access, "person/extra", true).is_err());
        assert!(full.toggle(Side::Deny, "person/extra", true).is_ok());
        // The service stores the owner first and never denies them.
        let asked = aud(
            &["company/eng", "person/alice"],
            &["person/alice", "agent/x"],
        );
        let stored = stored_audience(Some("alice"), &asked);
        assert_eq!(stored.access, ["person/alice", "company/eng"]);
        assert_eq!(stored.deny, ["agent/x"]);
    }

    #[test]
    fn readers_who_lose_access_are_counted_before_saving() {
        let people = vec![
            person("alice", &["company/eng"]),
            person("bob", &["company/eng"]),
            person("carol", &[]),
        ];
        let before = aud(&["person/alice", "company"], &[]);
        let after = aud(&["person/alice", "company/eng"], &[]);
        let losing: Vec<&str> = losing_readers(&people, Some("alice"), &before, &after)
            .iter()
            .map(|p| p.id.as_str())
            .collect();
        assert_eq!(losing, ["carol"]);
        // Denying Bob loses him too; the owner never counts.
        let denied = aud(&["person/alice", "company/eng"], &["person/bob"]);
        let losing = losing_readers(&people, Some("alice"), &after, &denied);
        assert_eq!(losing.len(), 1);
        assert_eq!(losing[0].id, "bob");
        assert!(losing_readers(&people, Some("alice"), &after, &before).is_empty());
    }

    #[test]
    fn audit_rows_read_as_who_what_and_a_diff() {
        let people = vec![person("alice", &[]), person("bob", &[])];
        let groups = vec![group("company", 2), group("company/eng", 1)];
        let agents = vec![agent("x", "alice", "claude-code")];
        let names = Names {
            groups: &groups,
            people: &people,
            agents: &agents,
        };
        let mut row = AuditRow {
            kind: AuditKind::AudienceChange,
            at: Utc.timestamp_opt(0, 0).unwrap(),
            actor: "person/alice".into(),
            on_behalf_of: None,
            subject: margin_shared::context::AuditSubject {
                kind: "topic".into(),
                id: "t1".into(),
            },
            before: serde_json::json!({ "access": ["person/alice", "company"], "deny": [] }),
            after: serde_json::json!({ "access": ["person/alice", "company/eng"], "deny": ["person/bob"] }),
            device_id: None,
            subject_name: None,
        };
        assert_eq!(actor_label(&row, names), "@alice");
        assert_eq!(subject_label(&row, names), "a topic");
        assert_eq!(
            audit_change(&row, names),
            "access: +@eng −everyone at the company · deny: +@bob"
        );
        row.subject_name = Some("Thermodynamics".into());
        assert_eq!(subject_label(&row, names), "Thermodynamics");
        row.kind = AuditKind::PersonChange;
        row.subject = margin_shared::context::AuditSubject {
            kind: "person".into(),
            id: "bob".into(),
        };
        row.subject_name = None;
        row.before = serde_json::json!({ "role": "member", "state": "active" });
        row.after = serde_json::json!({ "role": "admin", "state": "active" });
        assert_eq!(subject_label(&row, names), "@bob");
        assert_eq!(audit_change(&row, names), "role: member → admin");
        row.actor = "agent/x".into();
        row.on_behalf_of = Some("person/alice".into());
        assert_eq!(actor_label(&row, names), "claude-code (agent of @alice)");
        row.actor = "host/root".into();
        assert_eq!(actor_label(&row, names), "root on the service host");
        assert_eq!(audit_kind_label(AuditKind::Export), "Export");
    }
}
