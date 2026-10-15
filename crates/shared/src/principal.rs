//! Accounts and principals (roadmap L1, `docs/l1-brief.md` D0): who may sign in, the
//! groups they belong to, the agents they own, and the invitations that admit them.
//!
//! A principal id is `"person/<id>"`, `"agent/<id>"`, or a group path (`"company"`,
//! `"company/engineering"`). A row's `access` lists principal ids and group paths; a
//! reader whose expanded set (own id, every group and its ancestors, `company`) meets
//! it may read the row.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{Id, Provider, ReasoningEffort};

/// "person/<id>", "agent/<id>", or a group path ("company", "company/engineering").
pub type PrincipalId = String;

pub const PERSON_PREFIX: &str = "person/";
pub const AGENT_PREFIX: &str = "agent/";
/// The root group every person belongs to (`context::ACCESS_COMPANY`).
pub const ROOT_GROUP: &str = crate::context::ACCESS_COMPANY;
/// Agent tokens start with this; device tokens never do.
pub const AGENT_TOKEN_PREFIX: &str = "mga_";
/// L3's cap on recipients of a group mention (owner, Q8).
pub const MENTION_RECIPIENTS_MAX: usize = 50;
/// Most segments of a group path.
pub const GROUP_PATH_MAX_SEGMENTS: usize = 8;
/// Longest segment of a group path.
pub const GROUP_SEGMENT_MAX: usize = 40;
/// How long an invitation stays open (owner, Q5).
pub const INVITATION_DAYS: i64 = 30;

/// `"person/<id>"`.
pub fn person_id(id: &str) -> PrincipalId {
    format!("{PERSON_PREFIX}{id}")
}

/// `"agent/<id>"`.
pub fn agent_id(id: &str) -> PrincipalId {
    format!("{AGENT_PREFIX}{id}")
}

/// A group path: 1..=8 segments of `[a-z0-9-]{1,40}`, the first `company`.
pub fn is_group_path(path: &str) -> bool {
    let segments: Vec<&str> = path.split('/').collect();
    segments.len() <= GROUP_PATH_MAX_SEGMENTS
        && segments.first() == Some(&ROOT_GROUP)
        && segments.iter().all(|s| {
            !s.is_empty()
                && s.len() <= GROUP_SEGMENT_MAX
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// `"company/a/b"` → `["company", "company/a", "company/a/b"]`. Not a group path → empty.
pub fn ancestors(path: &str) -> Vec<String> {
    if !is_group_path(path) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut at = String::new();
    for segment in path.split('/') {
        if !at.is_empty() {
            at.push('/');
        }
        at.push_str(segment);
        out.push(at.clone());
    }
    out
}

/// A handle: 2..=39 chars of `[a-z0-9-]`, no leading or trailing `-` (GitHub's rule,
/// lowercased).
pub fn is_handle(h: &str) -> bool {
    (2..=39).contains(&h.len())
        && !h.starts_with('-')
        && !h.ends_with('-')
        && h.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The handle a GitHub login gets: lowercased. The service de-duplicates with "-2", "-3".
pub fn handle_for_login(login: &str) -> String {
    login.to_ascii_lowercase()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrgRole {
    /// Exactly one: the bootstrap person (`allowed_github_login`). Can't be disabled or
    /// demoted.
    Owner,
    Admin,
    Member,
}

impl OrgRole {
    /// Owner or Admin.
    pub fn is_admin(self) -> bool {
        matches!(self, OrgRole::Owner | OrgRole::Admin)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersonState {
    Invited,
    Active,
    /// Can't sign in; their devices were revoked and their agents' tokens stop working.
    Disabled,
}

/// Someone who may sign in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Person {
    pub id: Id,
    pub github_login: String,
    /// GitHub's numeric id, stored at the first sign-in; a login renamed and handed to
    /// someone else doesn't match it.
    #[serde(default)]
    pub github_id: Option<u64>,
    /// Unique, `is_handle`; what mentions use.
    pub handle: String,
    pub name: String,
    #[serde(default)]
    pub avatar_url: Option<String>,
    pub org_role: OrgRole,
    pub state: PersonState,
    /// Direct memberships (group paths; ancestors are implied, never stored).
    #[serde(default)]
    pub groups: Vec<String>,
    /// Only on the caller's own row; `Default` elsewhere.
    #[serde(default)]
    pub settings: PersonSettings,
    pub created_at: DateTime<Utc>,
}

/// What a person overrides for their own chats; `None` = the org's `Settings`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PersonSettings {
    #[serde(default)]
    pub default_provider: Option<Provider>,
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// `PATCH /api/people/{id}`. Self: `name`, `avatar_url`, `settings`; an admin also
/// `state` and `org_role` (Member/Admin; making or removing an admin is the owner's).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PersonPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `null` clears it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::double_option"
    )]
    pub avatar_url: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_role: Option<OrgRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<PersonState>,
    /// Replaces the person's settings whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<PersonSettings>,
}

/// A group; the tree is the paths.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub path: String,
    pub name: String,
    /// Where it comes from: `"manual"` (the API) in L1; L6 adds `"github-org"`.
    #[serde(default = "manual")]
    pub source: String,
    /// Direct members.
    #[serde(default)]
    pub members: u32,
    pub created_at: DateTime<Utc>,
}

/// `Group.source` of groups and memberships written through the API.
pub const SOURCE_MANUAL: &str = "manual";

fn manual() -> String {
    SOURCE_MANUAL.into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewGroup {
    pub path: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GroupPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    /// Margin's own agent: the owner's whole access by default.
    Margin,
    /// An outside agent (Claude Code through `margin-mcp`): the owner's access minus their
    /// private notes and chats by default (owner, Q5).
    External,
    /// A voice agent (L5): like `External`.
    Voice,
}

/// An agent a person owns; it acts within `ceiling`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Agent {
    pub id: Id,
    pub owner_id: Id,
    /// `context::is_agent_name`; unique per owner.
    pub name: String,
    pub kind: AgentKind,
    /// Principals the agent may act as; always ⊆ the owner's expansion, stored
    /// ancestor-closed. Never empty.
    pub ceiling: Vec<PrincipalId>,
    #[serde(default)]
    pub disabled: bool,
    /// Present when a token exists: never the token itself.
    #[serde(default)]
    pub token: Option<AgentTokenInfo>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentTokenInfo {
    /// The token's first characters ("mga_ab12"), to tell tokens apart.
    pub prefix: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub last_used_at: Option<DateTime<Utc>>,
    /// `None` in L1 (L5's voice tokens expire).
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewAgent {
    pub name: String,
    pub kind: AgentKind,
    /// `None`: the kind's default (D7).
    #[serde(default)]
    pub ceiling: Option<Vec<PrincipalId>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ceiling: Option<Vec<PrincipalId>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
}

/// `POST /api/agents/{id}/token`: the token, shown once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentToken {
    pub token: String,
    pub agent: Agent,
}

/// An invitation by GitHub login; it turns into a person at their first sign-in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Invitation {
    pub id: Id,
    pub github_login: String,
    /// Admin or Member.
    pub role: OrgRole,
    #[serde(default)]
    pub groups: Vec<String>,
    pub invited_by: Id,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    #[serde(default)]
    pub accepted_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub accepted_by: Option<Id>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewInvitation {
    pub github_login: String,
    pub role: OrgRole,
    #[serde(default)]
    pub groups: Vec<String>,
}

/// `ApiError.code`s of L1's refusals.
pub mod access_code {
    /// 403: only an organisation admin can do this.
    pub const ADMIN_REQUIRED: &str = "admin_required";
    /// 403: the route needs a device token (an agent token was used).
    pub const DEVICE_REQUIRED: &str = "device_required";
    /// 403: agent tokens may only read context.
    pub const AGENT_NOT_ALLOWED: &str = "agent_not_allowed";
    /// 403: only the row's owner may change it.
    pub const NOT_OWNER: &str = "not_owner";
    /// 400: the organisation owner's role and state can't change.
    pub const OWNER_FIXED: &str = "owner_fixed";
    /// 400: an agent ceiling names something outside its owner's access.
    pub const OUTSIDE_OWNER: &str = "outside_owner";
    /// 400: a context query asked for access paths the caller doesn't have.
    pub const BAD_ACCESS: &str = "bad_access";
    /// 400: a group path that doesn't exist.
    pub const UNKNOWN_GROUP: &str = "unknown_group";
    /// 400: the group still has groups under it.
    pub const HAS_CHILDREN: &str = "has_children";
    /// 400: the group still has members.
    pub const HAS_MEMBERS: &str = "has_members";
    /// 409: a person with that GitHub login exists.
    pub const ALREADY_MEMBER: &str = "already_member";
    /// 409: an invitation for that login is pending.
    pub const ALREADY_INVITED: &str = "already_invited";
    /// 400: the row comes from an access source other than `manual`.
    pub const MANAGED_BY_SOURCE: &str = "managed_by_source";
    /// 401: the token's person is disabled (the message is the sentence to show).
    pub const PERSON_DISABLED: &str = "person_disabled";

    pub const ALL: [&str; 14] = [
        ADMIN_REQUIRED,
        DEVICE_REQUIRED,
        AGENT_NOT_ALLOWED,
        NOT_OWNER,
        OWNER_FIXED,
        OUTSIDE_OWNER,
        BAD_ACCESS,
        UNKNOWN_GROUP,
        HAS_CHILDREN,
        HAS_MEMBERS,
        ALREADY_MEMBER,
        ALREADY_INVITED,
        MANAGED_BY_SOURCE,
        PERSON_DISABLED,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_paths() {
        assert!(is_group_path("company"));
        assert!(is_group_path("company/eng-1"));
        assert!(!is_group_path("Company"));
        assert!(!is_group_path("company//x"));
        assert!(!is_group_path("company/x/"));
        assert!(!is_group_path("engineering"));
        assert!(!is_group_path(""));
        assert!(!is_group_path(&format!("company/{}", "a".repeat(41))));
        assert!(is_group_path("company/a/b/c/d/e/f/g"));
        assert!(!is_group_path("company/a/b/c/d/e/f/g/h"));
    }

    #[test]
    fn ancestors_of_a_path() {
        assert_eq!(
            ancestors("company/a/b"),
            vec!["company", "company/a", "company/a/b"]
        );
        assert_eq!(ancestors("company"), vec!["company"]);
        assert!(ancestors("person/x").is_empty());
    }

    #[test]
    fn handles() {
        assert!(is_handle("nkr1shna"));
        assert!(is_handle("bob-dev"));
        assert!(!is_handle("a"));
        assert!(!is_handle("-bob"));
        assert!(!is_handle("bob-"));
        assert!(!is_handle("Bob"));
        assert!(!is_handle(&"a".repeat(40)));
        assert_eq!(handle_for_login("Nkr1shna"), "nkr1shna");
        assert!(is_handle(&handle_for_login("Nkr1shna")));
    }

    #[test]
    fn principal_ids() {
        assert_eq!(person_id("p1"), "person/p1");
        assert_eq!(agent_id("a1"), "agent/a1");
        assert_eq!(ROOT_GROUP, "company");
        assert!(OrgRole::Owner.is_admin() && OrgRole::Admin.is_admin());
        assert!(!OrgRole::Member.is_admin());
        assert_eq!(access_code::ALL.len(), 14);
    }

    #[test]
    fn me_without_a_person_parses() {
        let me: crate::Me = serde_json::from_value(serde_json::json!({
            "github_login": "Nkr1shna",
            "device": {"id": "d", "name": "Mac", "created_at": "2026-10-14T08:00:00Z",
                       "last_seen_at": null, "current": true}
        }))
        .unwrap();
        assert!(me.person.is_none() && me.agent.is_none());
        assert_eq!(me.device.unwrap().person_id, None);
    }

    #[test]
    fn agents_round_trip() {
        let at: DateTime<Utc> = "2026-10-14T08:00:00Z".parse().unwrap();
        let agent: Agent = serde_json::from_value(serde_json::json!({
            "id": "a1", "owner_id": "p1", "name": "claude-code", "kind": "external",
            "ceiling": ["company"], "created_at": at
        }))
        .unwrap();
        assert_eq!(agent.token, None);
        assert!(!agent.disabled);
        let json = serde_json::to_value(&agent).unwrap();
        assert_eq!(serde_json::from_value::<Agent>(json).unwrap(), agent);

        let person = Person {
            id: "p1".into(),
            github_login: "Nkr1shna".into(),
            github_id: Some(1),
            handle: "nkr1shna".into(),
            name: "Krishna".into(),
            avatar_url: None,
            org_role: OrgRole::Owner,
            state: PersonState::Active,
            groups: vec![],
            settings: PersonSettings {
                default_provider: Some(Provider::Claude),
                reasoning_effort: None,
            },
            created_at: at,
        };
        let json = serde_json::to_value(&person).unwrap();
        assert_eq!(json["org_role"], "owner");
        assert_eq!(serde_json::from_value::<Person>(json).unwrap(), person);

        let clear = PersonPatch {
            avatar_url: Some(None),
            ..Default::default()
        };
        let json = serde_json::to_string(&clear).unwrap();
        assert_eq!(json, r#"{"avatar_url":null}"#);
        assert_eq!(serde_json::from_str::<PersonPatch>(&json).unwrap(), clear);
        assert_eq!(
            serde_json::from_str::<PersonPatch>("{}").unwrap(),
            PersonPatch::default()
        );

        let group: Group = serde_json::from_value(serde_json::json!({
            "path": "company", "name": "Company", "created_at": at
        }))
        .unwrap();
        assert_eq!(group.source, SOURCE_MANUAL);
        assert_eq!(group.members, 0);
    }
}
