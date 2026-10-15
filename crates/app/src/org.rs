//! Accounts and principals as the app shows them (L1, `docs/l1-brief.md` D10): a
//! person's expanded access, agent ceilings, the group tree, and the words for roles,
//! states and kinds. Pure functions, shared by Settings → Organisation and the fake.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use margin_shared::{
    AgentKind, Group, Invitation, OrgRole, Person, PersonState, ROOT_GROUP, ancestors,
    is_group_path, person_id,
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
}
