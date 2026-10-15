## L1 — accounts and principals: the build brief

Read: `origin/dev` at `9a4eee1`; `docs/ROADMAP.md`, `docs/plan-live-context.md` (§1–§3, §7 L1/L2, §8, §9 with the owner's answers), `docs/agent-phase4-brief.md` (the shape), `docs/plan-linux-service.md` (secrets, paths); `crates/service/src/{auth,devices,events,server,state,db,settings,context_api,retrieval,ingest,search,chats,library,turns,workspaces,trace,secrets,config}.rs`, `crates/service/tests/{common/mod,api}.rs`; `crates/shared/src/{lib,chat,context,device,routes}.rs`; `crates/app/src/{state,store,main,api,fake,secrets}.rs`, `ui/{settings,signin,sidebar,shell}.rs`; `crates/mcp/src/{login,config,client,serve,main}.rs`. No code changed.

### Decided by the owner (2026-10-14, plan §9: every recommendation accepted)

- **Q2** First principals: invitations by GitHub login now; GitHub-org auto-admission in L6.
- **Q3** Admins cannot read private knowledge bases; a logged owner-only export on the service host is the only path (not built here; L2-audit).
- **Q5** An agent is owned by a person and its ceiling is a subset of the owner's access. Margin's own agent gets the owner's full access; **external and voice agents exclude the private KB by default**.
- **Q6** No email invitations until a mail sender exists.
- **Q8** Group mentions are capped at 50 recipients (L3; recorded here as a shared constant so L3 doesn't invent one).
- **Linux is the only service platform.** Nothing in this brief touches the macOS backends being removed (`KeychainStore`, `launchd`, `sandbox-exec`); secrets go through `SecretStore` (`secrets.rs:29`, `FileStore` `secrets.rs:93`). User-facing text says "the service's secret store", never "Keychain".

### What exists today, checked

| Plan item | State | Where |
|---|---|---|
| One allowed GitHub login; the callback admits only `config.allowed_github_login` (case-insensitive) | **As built.** The admission check is one `match` arm. | `auth.rs:319-332`, `config.rs:39,213` |
| Sign-in flow: `POST /auth/start` → browser → `/auth/github/callback` → `POST /auth/poll` mints the device token once | **Kept as is.** `poll` writes `DeviceRow` and returns `LoginState::Approved { token, github_login }`. GitHub's `/user` answer is read for `login` only (`id` is dropped). | `auth.rs:118-234`, `auth.rs:351-438`, `shared/lib.rs:865-898` |
| `DeviceRow { id, name, token_hash, github_login, created_at, last_seen_at }` | **No person.** Every device is "the" user. | `state.rs:137-146` |
| `require_device` on the whole `/api` router; `CurrentDevice(DeviceRow)` as an axum extension | **As built.** `device_for_token` does `list_where(DEVICE, "token_hash")` and touches `last_seen_at` every 60 s under the write lock. | `auth.rs:478-533`, `server.rs:321-324` |
| Handlers that read `CurrentDevice` | `chats.rs:324,511,542,559` (create, message, stop, confirm), `workspaces.rs:43,74`, `uploads.rs:121`, `sources.rs:412`, `folders.rs:312`, `events.rs:36`. **Everything else ignores the caller**: `library::get_library` lists every row (`library.rs:132-150`), `chats::list_chats` every chat (`chats.rs:309-317`), `search::find` (`search.rs:38-60`), `settings::patch_settings` (`settings.rs:29`), `auth::list_devices`/`revoke_device` (any device revokes any device, `auth.rs:557-597`), `trace::list_traces` (`trace.rs:660`), the context API (`context_api.rs`). | |
| Events: one `broadcast::Sender<Event>`; every socket forwards every event | **The leak.** `App.events` (`state.rs:279`), `App::emit` (`state.rs:340-343`); `events::run` subscribes (`events.rs:81`) and `serve` sends each `Event` unfiltered (`events.rs:118-123`); `?traces=1` streams every trace row to any device (`events.rs:134-143`); presence (`events.rs:96-102`). `Event` has no audience (`shared/lib.rs:915-1011`). `DeviceHub` keys sockets by device id (`devices.rs:21-27`). | |
| `document.actor` (`"owner"` or `agent:<name>`), `document.access` default `["company"]`, `passage.access` schemafull `DEFAULT ['company']`, indexed `doc.access.*` / `access.*` | **The seam exists, unenforced.** `retrieval::Filter` adds `AND access CONTAINSANY $paths` only when `ContextQuery.access` is `Some` (`retrieval.rs:455-520`); `visible_paths` expands by prefix over a full `list(db::DOCUMENT)` (`retrieval.rs:425-451`); the agent's `search` passes `access: None` (`agent_tools.rs:774`); the context API lets the caller supply any `access` (`context.rs:183-187`). `ingest::new_document` stamps `company` (`ingest.rs:197`). | `context.rs:30-56`, `db.rs:96,107,124,234-237` |
| "Agent" identity for the context API | A client-chosen header `x-margin-agent` on a **device token** (`context_api.rs:60-66`, `context.rs:515-518`); traces get `actor: agent:<name>, via: mcp` (`retrieval.rs:116-129`). `margin-mcp login` is a device sign-in named "Claude Code (host)" (`mcp/src/login.rs:19-71`), token in the macOS Keychain (`mcp/src/config.rs:46-76`), sent as Bearer with the header (`client.rs:50-57`). `margin-mcp serve` uses only `search`, `outline`, `passages`, `ingest`, `changes`, `documents`, `delete_document` (`client.rs:61-130`). | |
| Settings: one row `SETTINGS_ID = "main"`; `SettingsRow.migrations: Vec<String>` runs one-time migrations once (`ingest::migrate`, `passages-v1`) | **Reused** for `people-v1`. `Settings` on the wire carries no secrets (`has_api_key` flags only). | `state.rs:148-185,264`, `ingest.rs:1857-1920`, `shared/lib.rs:604-653` |
| Store: schemaless `{ doc }` tables, `DEFINE TABLE IF NOT EXISTS` per name, additive index block, `list_where`/`set_where`/`query_rows`/`put_many` | **Enough.** New tables are names in the list; new indexes are lines in `SCHEMA`. `trace` is schemafull (`db.rs:127-169`): a new column is one `DEFINE FIELD IF NOT EXISTS`. | `db.rs:24-58,94-100,313,472-660` |
| Ownership on records | **None.** `Topic` (`lib.rs:55-63`), `Lesson` (`:87-106`), `Note` (`:109-120`), `Chat` (`chat.rs:20-56`), `Source` (`lib.rs:322-347`), `Proposal` (`:237-247`) have no owner or audience; `Workspace` has `device_id`/`device_name` (`workspaces.rs:47-56`); `StoredLesson`/`StoredNote.author: Author { Owner, Agent }` is *who wrote the text*, not whose it is (`state.rs:98-120`). `trace` has `actor` and `device_id` (`db.rs:140,161`), stamped `"owner"` at `turns.rs:1937`, `trace.rs:370`. | |
| App | `Session { api, me: Option<Me> }` (`app/state.rs:22-25`), `Me` loaded at `main.rs:484`; `Api::{me, devices, revoke_device}` (`api.rs:235,342,346`); `SettingsPane` has 9 panes (`ui/settings.rs:66-89`); `AccountSection` shows the GitHub login, this device, and a Devices list with revoke (`ui/settings.rs:385-520`); the sidebar's avatar initials come from `me.github_login` (`sidebar.rs:114-119`); `LibraryState.devices` applies `DeviceChanged`/`DeviceDeleted` (`store.rs:45,333-345`); `fake.rs` has the auth routes and `me` (`fake.rs:727-771,880-915`), `FakeState` (`:25-40`). `Me` literals that a new required field would break: `fake.rs:895,910`, `ui/test_render.rs:62`, `examples/render_screens.rs:290`, `api.rs:793` (test), `crates/mcp/tests/common/mod.rs:260`. | |
| Tests | `tests/common/mod.rs`: fake GitHub with `OWNER_CODE` → login `nkr1SHNA` and `OTHER_CODE` → `someone-else` (`:60-110`), `Harness::sign_in` (`:1278-1286`), `allowed_github_login: "Nkr1shna"` (`:1123`), `Socket::{next_event, until}` (`:1326,1553-1571`). `tests/api.rs:60-90` covers approve/deny. Enough: L1 adds a third code and `sign_in_as`. | |

**The design in one paragraph.** A `person` table holds everyone who may sign in; the bootstrap owner is created from `allowed_github_login` by a one-time migration that also hangs every existing device, topic, lesson, note, chat, source, folder, proposal and document on that person. Every `/api` request resolves a `Principal` (a person through a device token, or an agent through an `mga_` token with a ceiling inside its owner's access) before any handler runs; the principal carries its expanded read set (`person/<id>`, every group and ancestor, `company`). Rows gain `owner`, `access` and `deny`; a row's `access` is never empty and defaults to private. Events are emitted with an audience and each socket sends only what its principal may see. List routes, item routes, ⌘K and the chat/folder routes filter on the principal; retrieval receives the principal's paths but real enforcement inside the index is L2's (today every passage says `company`). Invitations by GitHub login turn into persons at the callback. The app gets an Organisation pane (people, invitations, groups, agents) and per-person settings; `margin-mcp` can run on an agent token.

### 1. Design decisions

#### D0 settled: the principal alphabet (first commit, `margin_shared::principal`)

```rust
/// "person/<id>", "agent/<id>", or a group path ("company", "company/engineering").
pub type PrincipalId = String;
pub const PERSON_PREFIX: &str = "person/";
pub const AGENT_PREFIX: &str = "agent/";
pub const ROOT_GROUP: &str = "company";          // = context::ACCESS_COMPANY
pub const AGENT_TOKEN_PREFIX: &str = "mga_";
/// L3's cap on recipients of a group mention (owner, Q8).
pub const MENTION_RECIPIENTS_MAX: usize = 50;

pub fn person_id(id: &str) -> PrincipalId;        // "person/<id>"
pub fn agent_id(id: &str) -> PrincipalId;
/// A group path: 1..=8 segments of [a-z0-9-]{1,40}, the first `company`.
pub fn is_group_path(path: &str) -> bool;
/// "company/a/b" → ["company", "company/a", "company/a/b"].
pub fn ancestors(path: &str) -> Vec<String>;
/// A handle: 2..=39 chars of [a-z0-9-], no leading/trailing '-' (GitHub's rule, lowercased).
pub fn is_handle(h: &str) -> bool;
pub fn handle_for_login(login: &str) -> String; // lowercased; the service de-duplicates with "-2"

#[derive(Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum OrgRole { Owner, Admin, Member }        // Owner: exactly one, the bootstrap person
pub enum PersonState { Invited, Active, Disabled }

pub struct Person {
    pub id: Id, pub github_login: String, pub github_id: Option<u64>,
    pub handle: String, pub name: String, pub avatar_url: Option<String>,
    pub org_role: OrgRole, pub state: PersonState,
    /// Direct memberships (group paths; ancestors are implied, never stored).
    #[serde(default)] pub groups: Vec<String>,
    #[serde(default)] pub settings: PersonSettings,
    pub created_at: DateTime<Utc>,
}
/// What a person overrides for their own chats; `None` = the org's `Settings`.
#[derive(Default)] pub struct PersonSettings {
    pub default_provider: Option<Provider>, pub reasoning_effort: Option<ReasoningEffort>,
}
pub struct PersonPatch { name, avatar_url, org_role, state, settings }  // Option fields; double_option where clearing matters
pub struct Group { pub path: String, pub name: String, pub source: String /* "manual" */, pub members: u32, pub created_at }
pub struct NewGroup { pub path: String, pub name: String }
pub struct GroupPatch { pub name: Option<String> }
pub enum AgentKind { Margin, External, Voice }
pub struct Agent {
    pub id: Id, pub owner_id: Id, pub name: String /* is_agent_name */, pub kind: AgentKind,
    /// Principals the agent may act as; always ⊆ the owner's expansion, stored as an
    /// ancestor-closed set (D7). Never empty.
    pub ceiling: Vec<PrincipalId>,
    pub disabled: bool,
    /// Present when a token exists: never the token itself.
    pub token: Option<AgentTokenInfo { prefix: String /* "mga_ab12" */, created_at, last_used_at: Option<_>, expires_at: Option<_> }>,
    pub created_at: DateTime<Utc>,
}
pub struct NewAgent { pub name: String, pub kind: AgentKind, pub ceiling: Option<Vec<PrincipalId>> }
pub struct AgentPatch { pub name, pub ceiling, pub disabled }
/// `POST /api/agents/{id}/token`: the token, shown once.
pub struct AgentToken { pub token: String, pub agent: Agent }
pub struct Invitation {
    pub id: Id, pub github_login: String, pub role: OrgRole /* Admin | Member */,
    pub groups: Vec<String>, pub invited_by: Id, pub created_at, pub expires_at,
    pub accepted_at: Option<DateTime<Utc>>, pub accepted_by: Option<Id>,
}
pub struct NewInvitation { pub github_login: String, pub role: OrgRole, pub groups: Vec<String> }
```

`Me` gains `#[serde(default)] person: Option<Person>` and `#[serde(default)] agent: Option<Agent>`; `Me.device` becomes `Option<Device>` (`#[serde(default)]`; always `Some` for a device token, so the wire is unchanged for today's apps). `Device` gains `#[serde(default)] person_id: Option<Id>`. `LoginState::Approved` is unchanged.

Tests (shared): `is_group_path` on `company`, `company/eng-1`, `Company`, `company//x`, `company/x/`, nine segments; `ancestors`; `is_handle`; `handle_for_login("Nkr1shna") == "nkr1shna"`; `Me` without `person` parses; `Agent` round-trips without `token`.

#### D1 settled: `Principal` and `require_principal` (first commit: the types and the stub; L1-auth: the real thing)

```rust
// crates/service/src/principal.rs
pub struct Principal {
    pub kind: PrincipalKind,          // Person | Agent { owner_id: Id, agent_id: Id }
    pub person_id: Id,                // the person, or the agent's owner
    pub id: PrincipalId,              // "person/<id>" | "agent/<id>"
    pub paths: Vec<String>,           // the EXPANDED read set (D5); for an agent: owner's ∩ ceiling
    pub deny_ids: Vec<String>,        // own id + direct and implied groups (L2 uses it)
    pub org_role: OrgRole,            // Member for every agent
    pub device: Option<DeviceRow>,    // Some for device tokens
    pub person: Person,               // the person row as read (handle, settings)
}
pub struct CurrentPrincipal(pub Arc<Principal>);   // Extension inserted by `require_principal`
```

`require_principal` replaces `require_device` on the same `route_layer` (`server.rs:321-324`): parse the Bearer; a token starting with `mga_` is looked up in `agent` by `token_hash` (`index agent_token`), else in `device` by `token_hash` as today. Refusals are all 401 `unauthorized` (never say which half failed), except an agent token on a route agents may not call: 403 `agent_not_allowed` ("Agent tokens can read context; sign in with a device for this."). A device whose person is `Disabled`, or an agent whose owner is disabled or that is `disabled`/expired: 401. `last_seen_at`/`last_used_at` are touched at most every 60 s as `device_for_token` does (`auth.rs:517-531`).

**`CurrentDevice` stays** as the extractor the nine existing handlers already use, re-implemented as `FromRequestParts` over the principal: `Some(device)` → `CurrentDevice(DeviceRow)`; `None` → 403 `device_required`. So `chats.rs`, `workspaces.rs`, `uploads.rs`, `sources.rs`, `folders.rs`, `events.rs` keep their signatures. A `RequireAdmin` extractor (403 `admin_required`, "Only an organisation admin can do this.") guards admin routes; `RequireOwner` the two owner-only ones (D4).

**Agent tokens may call** `/api/me`, `/api/projects`, `/api/context/*` (search, outline, passages, ingest, changes, delete) and nothing else in L1 (`margin-mcp serve` needs exactly these; `client.rs:61-130`). The router splits `/api` into the agent-callable subset and the rest; `require_principal` reads a route-layer marker (`Extension(AgentsAllowed)`) on the subset. The events socket is device-only (an agent is never "online" and runs no device tools).

**Expansion** (`groups::expansion(state, person_id) -> Arc<Expansion>`, L1-groups; the first commit's body returns `[company, person/<id>]` with the person's stored role so L1-auth's tests run): `paths` = `person/<id>` ∪ `company` ∪ for each direct membership its `ancestors`, sorted, deduplicated; `deny_ids` = the same minus nothing (L2 reads it). Cached in `App.expansions: Mutex<HashMap<Id, Arc<Expansion>>>` and **cleared whole** (`invalidate_all`) after any person, group, membership or agent write: one org, small tables, no per-key bookkeeping to get wrong. An agent's `paths` = owner's `paths` ∩ `agent.ceiling` (both ancestor-closed, so a plain set intersection).

#### D2 settled: tables, indexes, the migration (first commit `db.rs`; L1-auth runs it)

`db::{PERSON = "person", GROUP = "group", MEMBERSHIP = "membership", AGENT = "agent", INVITATION = "invitation"}` in the schemaless list (`db.rs:24-58`; note `group` is a SurrealQL keyword: the table is written `` `group` `` in every statement, and `Store::list`/`list_where` already quote table names — verify; else name it `principal_group`). Row shapes: the wire types plus `PersonRow.login_key` (the login lowercased, for the unique index) and `AgentRow { token_hash: Option<String>, token_prefix, token_expires_at, last_used_at }`; `MembershipRow { id, person_id, group_path, source }`; `DeviceRow.person_id: Option<Id>` (`#[serde(default)]`).

Indexes (additive lines in `SCHEMA`, `db.rs:94-100`): `person_login ON person FIELDS doc.login_key UNIQUE`, `person_handle ON person FIELDS doc.handle UNIQUE`, `agent_token ON agent FIELDS doc.token_hash`, `agent_owner ON agent FIELDS doc.owner_id`, `device_person ON device FIELDS doc.person_id`, `membership_person ON membership FIELDS doc.person_id`, `membership_group ON membership FIELDS doc.group_path`, `invitation_login ON invitation FIELDS doc.login_key`; `{topic,lesson,note,chat,source,workspace,proposal}_access ON <t> FIELDS doc.access.*` and `document_owner ON document FIELDS doc.owner`. `trace`: `DEFINE FIELD IF NOT EXISTS principal ON trace TYPE option<string>` (`db.rs:127-169`) and `TraceRow.principal: Option<String>` on the wire.

**`people-v1` migration** (`people::migrate`, called in `Service::start` after `trace::start` and before `recover`, `server.rs:150-152`; recorded in `SettingsRow.migrations` like `passages-v1`, `ingest.rs:1868-1920`; under `write_lock`; idempotent so a crash mid-way re-runs it):

1. If no `person` has `org_role == Owner`: create one from `config.allowed_github_login` (`handle = handle_for_login`, `name = login`, `state = Active`, `groups = []`), the `company` group (`source: manual`) if missing, and the owner's membership of `company`.
2. Every `DeviceRow` with `person_id == None` → the owner (today only the owner's login could sign in, whatever the case; `auth.rs:319`).
3. Every `topic`, `lesson`, `note`, `chat`, `source`, `workspace`, `proposal` row with `owner == None` → `owner = owner.id`, `access = ["person/<owner id>"]`, `deny = []` (private to the migrated owner, plan §7 L1). `put_many` in batches of `INSERT_MANY_MAX` (`db.rs:219`).
4. Every `document` with `owner == None` → `owner = owner.id`. **`document.access` and `passage.access` stay `["company"]`**: L2 re-stamps them from the owning rows in one pass when it starts enforcing inside the index (plan §2.2); changing them in L1 would hide everything from the owner's own agent for no gain.
5. Push `people-v1`. Log counts. Nothing is deleted; a backup before deploying is the deploy runbook's existing rule.

`allowed_github_login` keeps one meaning after the migration: the bootstrap owner. If the config value later differs from the owner person's login, the service logs a warning at start and does nothing (the owner is a row now; transferring ownership is not an L1 feature; Q1).

#### D3 settled: admission at the callback, devices per person (L1-auth, `auth.rs`)

`github_login_for_code` also returns GitHub's numeric `id` (`GithubUser { login, id }`, `auth.rs:351-354`). `callback_outcome` (`auth.rs:290-342`) replaces the `eq_ignore_ascii_case` arm with `people::admit(state, &login, id) -> Admission`:

- `Admission::Person(person)`: a person with `login_key == login.to_lowercase()` and `state == Active` (the GitHub `id` is stored if it was `None`; a stored `id` that **differs** from GitHub's is refused as "The GitHub account @x isn't the one that was invited." — a renamed login handed to someone else).
- `Admission::Invited(person)`: no person, but an `invitation` with that `login_key`, `accepted_at == None`, `expires_at > now`: under the write lock the person is created (`org_role = invitation.role`, `state = Active`, memberships from `invitation.groups` ∪ `company`, handle de-duplicated), the invitation gets `accepted_at`/`accepted_by`, `PersonChanged` and `InvitationChanged` are emitted after the lock.
- Disabled person: `Denied("The GitHub account @x is disabled on this Margin. Ask an admin.")`.
- Else `Denied` with today's text (`auth.rs:328`).

`LoginPhase::Approved { github_login }` becomes `Approved { person_id, github_login }`; `poll` writes `DeviceRow { person_id: Some(..), .. }` (`auth.rs:214-228`). The device's name stays the app's.

`me` (`auth.rs:547-555`) → `Me { github_login, device: Some(..), person: Some(person with groups), agent: None }`; for an agent token `Me { github_login: owner's, device: None, person: Some(owner, settings omitted), agent: Some(agent) }`.

`list_devices` returns the caller's own devices; an admin's call returns everyone's (`Device.person_id` set so the app groups them). `revoke_device` is allowed for the caller's own device or by an admin (404 otherwise, never 403: don't confirm the id exists). Revoking still cascades folders (`auth.rs:588-590`) and closes sockets through `revocations` (`state.rs:281`).

**Disabling a person** (`PATCH /api/people/{id} { state: disabled }`, admin; the owner can't be disabled or demoted: 400 `owner_fixed`): every device of the person is revoked the same way (`revocations.send` per device so their sockets close; rows deleted; folders deleted with them, as `revoke_device` does), the person's agent tokens stop working at the next request (`require_principal` reads the owner's state), `PersonChanged` goes out. Re-enabling does not bring devices back: the person signs in again. Their rows keep their owner: nothing is reassigned (admins cannot read a private KB; Q3).

#### D4 settled: people and invitations routes (first commit `routes.rs` + stubs; L1-auth builds)

| Route | Who | Does |
|---|---|---|
| `GET /api/people` | anyone (a directory: handles and names are what mentions need in L3) | `Vec<Person>` with `groups`; `settings` only on the caller's own row |
| `GET /api/people/{id}` | anyone | one person (404) |
| `PATCH /api/people/{id}` | self: `name`, `avatar_url`, `settings`; admin: also `state`, `org_role ∈ {Member, Admin}`; **owner only**: setting or removing `Admin` (`RequireOwner`) | `Person`; `PersonChanged`; `owner_fixed` for the owner's role/state |
| `PUT /api/people/{id}/groups` body `Vec<String>` | admin | replaces the person's direct memberships (`source: manual` rows only; every path must exist: 400 `unknown_group`); `MembershipChanged { person_id, groups }`, expansions invalidated |
| `GET /api/me/settings` / `PATCH /api/me/settings` | self | `PersonSettings` (also reachable through `PATCH /api/people/{me}`; this is the short form the app uses) |
| `GET /api/invitations` | admin | pending and accepted, newest first |
| `POST /api/invitations` body `NewInvitation` | admin (`role: Admin` needs the owner) | 409 `already_member` when a person has that login (disabled included: re-enable instead), 409 `already_invited` while one is pending; `expires_at = now + 30 days`; groups must exist; `InvitationChanged` (audience: admins) |
| `DELETE /api/invitations/{id}` | admin | revokes a pending one; 404 |

`routes.rs` constants: `PEOPLE`, `person(id)`, `person_groups(id)`, `ME_SETTINGS`, `INVITATIONS`, `invitation(id)`; the doc table gains the rows. The sign-in page (`auth.rs:448-472`) shows the person's handle on success ("Signed in to Margin as @bob.").

#### D5 settled: groups and the `AccessSource` seam (L1-groups, `groups.rs`)

| Route | Who | Does |
|---|---|---|
| `GET /api/groups` | anyone | every group with `members` (direct count), sorted by path: the tree is the paths |
| `POST /api/groups` `NewGroup` | admin | `is_group_path`, parent must exist (`company` always does and can't be created), 409 `exists`; `GroupChanged` |
| `PATCH /api/groups/{*path}` | admin | rename (`name` only; moving a path is not L1) |
| `DELETE /api/groups/{*path}` | admin | 400 `has_children` / `has_members` until empty; `company` refused; `GroupDeleted { path }` |

`access.rs` (L1-groups): `pub trait AccessSource: Send + Sync { fn name(&self) -> &'static str; fn groups(&self) -> BoxFuture<anyhow::Result<Vec<(String, String)>>>; fn members(&self, path: &str) -> BoxFuture<anyhow::Result<Vec<String /* GitHub login */>>>; }` and `pub struct Manual;` (returns what the tables hold) in a `Vec<Arc<dyn AccessSource>>` on `App.access_sources`. No sync job in L1: the trait, the `source` column on `group`/`membership`, and the rule that a write through the API only touches rows whose `source == "manual"` (400 `managed_by_source` otherwise). L6 adds `github-org` and the job.

#### D6 settled: ownership on records and what L1 enforces (first commit types; L1-events enforces)

Every row in `topic`, `lesson`, `note`, `chat`, `source`, `workspace`, `proposal` gains, on the wire type, `#[serde(default)] pub owner: Option<Id>` (always `Some` after `people-v1`), `#[serde(default)] pub access: Vec<String>`, `#[serde(default)] pub deny: Vec<String>`; `Document` gains `owner: Option<Id>`. Apps ignore what they don't know; a row read with an empty `access` is treated as private to its owner (defence in depth, same as the migration's stamp).

**At write (L1-events, every creating handler and tool):** `owner = principal.person_id` (an agent's rows belong to its owner; `actor` says the agent), `access = [person/<owner>]`, `deny = []`. Lessons, notes, videos and sources filed under a topic **copy the topic's `access`/`deny`** (so a list query is one conjunct and the topic is the unit of audience in the library); a chat's items carry the chat's; a folder's sources the folder's. `Document.owner` from the owning row at `upsert_document` (`ingest.rs:228-260`); `document.access` still `company` (D2-4).

**What L1 enforces (and nothing else):**

- **List routes** return the principal's slice: `GET /api/library` (`library.rs:132-150`) with `WHERE doc.access CONTAINSANY $paths` on each table (the `doc.access.*` indexes), `GET /api/chats` (`chats.rs:309`), `GET /api/workspaces`, `GET /api/workspaces/{id}/files`, `GET /api/search` (`search.rs:38-60`: the two FULLTEXT statements gain the conjunct). `deny` is written and read back; it is not evaluated until L2 (nothing writes a non-empty one in L1; the UI for it is L2's).
- **Item routes** answer 404 outside the principal's access: `topic`/`lesson`/`note`/`stored_note` helpers (`library.rs:25-57`) and `chats::chat` gain a `&Principal`; so do `patch_*`, `delete_*`, `regenerate`, `request_video`, `get_proposal`/`accept`, `get_chat`/`patch_chat`/`delete_chat`/`post_message`/`stop`/`continue`/`confirm`/`item_output`, `delete_source`/`reprocess`/`transcript`, `delete_workspace` (still also `not_yours` by device), `sync`/`files`. A `Video` follows its lesson. Writing (patch/delete) additionally needs `owner == principal.person_id` (403 `not_owner`); admins are not exempt (Q3).
- **Chat controls:** stop and continue by anyone in the chat's audience; confirm only by the chat's owner (403 `not_owner`; `turns::decide` gets the principal, `turns.rs:960`).
- **Settings:** `PATCH /api/settings` admin-only; `GET` for anyone (no secrets in it; members need the provider list to pick a model for a chat).
- **Traces:** `GET /api/traces`, `/api/traces/rollup` and `?traces=1` on the socket are admin-only (plan §2.4: admins and the eval runner); every trace row gets `principal = principal.id` (`turns.rs:1937`, `trace.rs:370`, `retrieval::Caller::stamp` `retrieval.rs:125-129`).
- **Retrieval plumbing, not enforcement:** `TurnInput` gains `principal: Arc<Principal>` (`agent/src/turn.rs:104-120` is `crates/agent`: the orchestrator adds the field in the first commit; the service fills it at `turns.rs:581-589`); `ToolContext` carries it; the agent's `search` passes `access: Some(principal.paths.clone())` instead of `None` (`agent_tools.rs:774`); the context API fills `ContextQuery.access` from the principal and **refuses** a caller-supplied value that isn't a subset (400 `bad_access`, plan §2.2). With every passage at `company` this changes no result today; it means L2-retrieve changes the stamping and the query, not the callers.

**Accepted L1 gaps (closed by L2, say so in the contract):** agent tools that read a library row by id (`get_topic`, `read_note`, `list_sources`, …) still take any id; `read_passage`/`document_outline`/`passages` by `document_id` likewise; passages all say `company` so a search by B finds A's private note's passages. A second *trusted* person (a tester) is fine on L1; a second *untrusted* one is not until L2 ships (plan §8.3 still stands for untrusted accounts).

#### D7 settled: agents and agent tokens (L1-groups, `agents.rs`)

| Route | Who | Does |
|---|---|---|
| `GET /api/agents` | anyone: own agents; admin: everyone's | `Vec<Agent>` (no tokens) |
| `POST /api/agents` `NewAgent` | anyone, for themselves | `name` must pass `is_agent_name` and be unique per owner (409 `exists`); `ceiling` defaults by kind (below); `AgentChanged` |
| `PATCH /api/agents/{id}` | owner (admin: `disabled` only) | rename, new ceiling (validated), disable; `AgentChanged` |
| `DELETE /api/agents/{id}` | owner | its token dies with it; `AgentDeleted` |
| `POST /api/agents/{id}/token` | owner | mints `mga_` + `random_token()` (`state.rs:671`), stores `sha256_hex`, replaces any previous token (rotate), returns `AgentToken` **once**; `expires_at: None` in L1 (voice's 2-hour tokens are L5's `expires_in` parameter on the same route) |
| `DELETE /api/agents/{id}/token` | owner | revokes; the next request on it is 401 |

**Ceiling math** (`agents::normalise_ceiling(owner: &Expansion, kind, asked: Option<Vec<_>>) -> Result<Vec<_>, Rejected>`): the stored ceiling is `asked ∪ ancestors(each asked group)` ∩ `owner.paths`; an entry outside the owner's expansion is 400 `outside_owner` naming it; the result must be non-empty. Defaults: `Margin` → the owner's whole expansion (private KB included); `External` and `Voice` → the expansion minus `person/<owner>` (Q5). Changing memberships re-validates nothing stored: the request-time intersection (D1) already drops what the owner lost; the Settings UI shows ceiling entries the owner no longer has as struck through. Admins cannot widen or read anyone's ceiling beyond listing it.

**The principal of an agent request** (D1) is `kind: Agent`, `person_id: owner`, `org_role: Member`, `paths: owner ∩ ceiling`. Context API documents ingested on an agent token get `owner = owner person`, `actor = agent/<id>` (the `x-margin-agent` header is ignored on an agent token; on a device token it keeps today's `agent:<name>` naming so the dogfood path works unchanged).

#### D8 settled: per-principal events (L1-events, `events.rs`, `state.rs`)

The wire `Event` is unchanged; the audience is service-side:

```rust
// state.rs
pub enum Audience { Everyone, Admins, Person(Id), PersonOrAdmins(Id), Paths(Vec<String>) }
pub struct Emitted { pub audience: Audience, pub event: Event }
pub events: broadcast::Sender<Emitted>;                     // was Sender<Event>
impl App {
    pub fn emit_to(&self, audience: Audience, event: Event);
    /// Everyone: `Hello`, `SettingsChanged`, `EmbeddingModelChanged`, `PersonChanged`,
    /// `GroupChanged`, `GroupDeleted`, `MembershipChanged` (a directory, no content).
    pub fn emit(&self, event: Event);                        // only for those; debug_assert! on anything else
    pub fn emit_row(&self, access: &[String], event: Event); // Paths(access.to_vec()); access must be non-empty
}
impl Audience { pub fn allows(&self, p: &Principal) -> bool }   // Paths: any path ∈ p.paths; Admins: Admin|Owner
```

Every emit site (`state.rs:345-373` helpers, `library.rs:192-627`, `chats.rs:90,99,504`, `turns.rs`, `ask.rs`, `sources.rs`, `folders.rs`, `workspaces.rs`, `ingest.rs`, `agent_tools.rs`, `embedding.rs`, `settings.rs`, `auth.rs:594`, `events.rs:98`) names its audience: a row's event → `emit_row(&row.access, ..)`; `ChatItemChanged`/`ChatItemDelta` → the chat's `access` (the chat is loaded where the item is saved; `save_item_locked` `chats.rs:96-102` takes it); `DocumentChanged` → `Person(doc.owner)` in L1 (L2 widens it with the document's audience); `DeviceChanged`/`DeviceDeleted` → `PersonOrAdmins(device.person_id)`; `AgentChanged`/`AgentDeleted` → `PersonOrAdmins(owner)`; `InvitationChanged`/`InvitationDeleted` → `Admins`; `Trace` → `Admins` (and only with `?traces=1`). `events::serve` (`events.rs:118-123`) does `if emitted.audience.allows(&principal) { send }`; the principal is the one `require_principal` built at upgrade and is **re-read after `AccessChanged`**: the socket keeps a `watch` on `App.expansions`' generation and rebuilds its `Principal` when memberships change, so a person removed from a group stops receiving that group's events without reconnecting. Directed frames (`DirectedFrame`) are unchanged: they are addressed to a device. `EVENTS_CAPACITY` stays; a filtered-out event is still consumed from the receiver, so lag accounting is as before.

Tests (`events.rs`, unit): `Audience::allows` for each variant; a socket for B never receives A's `NoteChanged`; `Admins` events reach an admin and not a member.

#### D9 settled: per-person settings (L1-auth routes, L1-events in turns)

`PersonSettings` lives on the person row (`PATCH /api/me/settings`, D4). `turns::start` (`turns.rs:581-589`) uses `person.settings.reasoning_effort.unwrap_or(settings.reasoning_effort)`; `chats::create_chat` (`chats.rs:333-336`) uses `person.settings.default_provider` before the org default. The org `Settings` (providers, keys, pipeline, render, retention) stays one row, admin-written. That is the whole split in L1: the plan's `OrgSettings`/`PersonSettings` wire types are not introduced; `Settings` keeps its name and shape (the app and the fake don't change for it), `PersonSettings` is the new, small thing. L5 adds `voice` to it.

#### D10 settled: the app (L1-app, `crates/app/**`)

- **`Session.me`** carries the person; the sidebar avatar uses `person.handle` (`sidebar.rs:114-119`); the account row at the bottom of the sidebar shows `@handle · role` and opens Settings → Account. No person switcher: one person per device token; switching accounts is Sign out → sign in (Q2 below).
- **Settings → Account** (`AccountSection`, `ui/settings.rs:385-520`): "Signed in as @handle (name) · Member/Admin/Owner", editable name; **My settings**: default provider (the configured ones, or "Organisation default"), reasoning effort (or default) → `PATCH /api/me/settings`; Devices: the caller's; for an admin a second list "Everyone's devices" grouped by person with revoke. Sign out unchanged.
- **Settings → AI** stays as is for admins; for a member every control is read-only with "Only an organisation admin changes these; your own defaults are under Account." (the owner's rule: no dead controls — read-only values with the sentence, not disabled buttons).
- **Settings → Organisation** (new `SettingsPane::Organisation` after Account; icon `users`, added to the icon set beside `globe`): sections **People** (table: handle, name, role, state, groups; admin actions per row: Make admin / Remove admin (owner only), Disable… / Enable, Edit groups… (a checkbox tree); the owner row shows "Owner" and no state control), **Invitations** (admin: "Invite…" dialog with GitHub login, role, groups → `POST`; pending list with expiry and Revoke; accepted ones collapsed; every non-admin sees "Ask an admin to invite people."), **Groups** (tree; admin: "Add group…" under a parent (slug validated with `is_group_path` before sending, the full path shown), Rename, Delete (disabled-with-reason when it has children or members: show the reason text, not a dead button)), **Agents** ("My agents": name, kind, ceiling as a checkbox tree over *my* expansion (private KB as its own row "My private notes and chats"), Create, Rename, Edit ceiling, Disable, Delete; "Token": Create/Rotate shows the token once in a monospace field with Copy and the line `margin-mcp token <token> --url <service>`; Revoke; `last_used_at`; admins see an "All agents" list read-only with Disable). Every control functional on desktop and phone widths; `ConfirmDialog` for disable/delete/revoke/rotate.
- **Store** (`store.rs`): `people: BTreeMap<Id, Person>`, `groups: BTreeMap<String, Group>`, `agents: BTreeMap<Id, Agent>`, `invitations: BTreeMap<Id, Invitation>`; `apply` handles `PersonChanged`, `GroupChanged/Deleted`, `MembershipChanged` (patches `people[id].groups`), `AgentChanged/Deleted`, `InvitationChanged/Deleted`; `load_library` (`main.rs:499`) also fetches `GET /api/people`, `/api/groups`, `/api/agents` and, for admins, `/api/invitations`. `Api` gains one method per route.
- **Signed-out reason**: a 401 with a disabled person shows the service's sentence on the sign-in screen (`signed_out_reason`, `app/state.rs:42-45`), as a revoke does today.
- **`fake.rs`**: the routes above with the same rules (two persons, `alice` owner and `bob` member, groups `company`, `company/engineering`; an agent with a token; invitations; `me` per token: a second `TOKEN_BOB`), so the render tests and `cargo test -p margin-app` cover the admin and member views. Events from the fake carry no audience (the fake is one person per run); the per-principal filter is a service test.

#### D11 settled: `margin-mcp` on an agent token (L1-mcp, `crates/mcp/**`)

- `margin-mcp token <mga_…> [--url]` stores an agent token for the service URL (`TokenStore`, entry key `<url>#agent`); `margin-mcp token --forget` removes it. `margin-mcp login` is unchanged (device token). `signed_in_client` (`main.rs:72-76`) prefers the agent token when both exist; `margin-mcp whoami` prints `/api/me` ("agent triage-bot of @krishna, may read: company, company/engineering"). `Client` keeps sending `x-margin-agent` (harmless on an agent token). The `serve`/`ingest`/`ingest-dir`/`purge` commands need nothing else: they call only the agent-allowed routes. README/`docs/plan-dogfood.md` note: create an agent "claude-code" in Settings → Organisation → Agents with the ceiling you want, mint a token, `margin-mcp token …`. The `TokenStore` on Linux CI is the in-memory one in tests; the binary's `Keychain` store is a Mac developer convenience and stays (margin-mcp runs on the developer's machine, not the service).

### 2. First commit (orchestrator lands on `dev` → the build branch before branching)

1. `crates/shared/src/principal.rs` (D0) and `pub mod principal; pub use principal::*;` in `lib.rs`; `Me`/`Device` changes; `owner`/`access`/`deny` on `Topic`, `Lesson`, `Note`, `Chat`, `Source`, `Workspace`, `Proposal`; `Document.owner`; `TraceRow.principal`; `Event::{PersonChanged, GroupChanged, GroupDeleted, MembershipChanged, AgentChanged, AgentDeleted, InvitationChanged, InvitationDeleted, AccessChanged { person_id }}`; `routes.rs` constants and doc rows (D4, D5, D7: `PEOPLE`, `person`, `person_groups`, `ME_SETTINGS`, `GROUPS`, `group(path)`, `AGENTS`, `agent`, `agent_token`, `INVITATIONS`, `invitation`); new error codes in `error_code` (`admin_required`, `device_required`, `agent_not_allowed`, `not_owner`, `owner_fixed`, `outside_owner`, `bad_access`, `unknown_group`, `has_children`, `has_members`, `already_member`, `already_invited`, `managed_by_source`). Round-trip tests.
2. `crates/agent/src/turn.rs`: `TurnInput` gains `pub actor: String` (`"person/<id>"` or `"agent/<id>"`, stamped on trace rows and the `Action` items) and nothing more: `crates/agent` must not depend on the service, so the service's `ToolContext` is what carries the `Arc<Principal>`. One field, one test update.
3. `crates/service`: `principal.rs` (D1 types, `require_principal` for device tokens with the trivial expansion, `CurrentDevice`/`RequireAdmin`/`RequireOwner` extractors, `AgentsAllowed`); `people.rs`, `invitations.rs`, `groups.rs`, `agents.rs`, `access.rs` with the handlers `unimplemented!()` and `groups::expansion` returning the trivial expansion; `db.rs` tables, indexes, the trace column; `state.rs` `Audience`/`Emitted`/`emit_to`/`emit_row` with `emit` forwarding everything as `Everyone` **for now** (so every area builds; L1-events turns the assertion on); `App.expansions`, `App.access_sources`; `server.rs` routes wired, `require_principal` on the layer, the agent-allowed split, the `people::migrate` call (a no-op stub; L1-auth lands the migration and its `people-v1` marker). `cargo test -p margin-service` must pass with every existing test: before `people-v1` there is no person row, so the stub principal synthesises the owner from `config.allowed_github_login` with `paths = [company, person/bootstrap]`, and everything behaves as today until L1-auth lands.
4. The `Me` literals (`fake.rs:895,910`, `ui/test_render.rs:62`, `examples/render_screens.rs:290`, `api.rs:793`, `crates/mcp/tests/common/mod.rs:260`): `device: Some(..)`, `person: None`, `agent: None`. Nothing else in `crates/app` or `crates/mcp`.
5. `docs/agent-contract.md`: §1 ownership rows for the five areas; §3 the new types; a "L1" section under §5 saying what is and isn't enforced (D6's gap list verbatim); §9 the open questions below. `docs/ROADMAP.md`: L1 🟡 with this brief linked.

`Cargo.toml`: nothing new (ids, hashing, tokens all exist).

### 3. Areas (worktree each; files are exclusive; branch `agent/l1-<area>` from the build branch)

| Area | Worktree | Builds | Owns (exclusively) |
|---|---|---|---|
| **L1-auth** | `agent/l1-auth` | D1 `require_principal` for device tokens (agent lookup calls `agents::principal_for_token`, L1-groups' function, stubbed `Ok(None)` in the first commit), extractors; D2 `people::migrate`; D3 admission, devices per person, disabling; D4 people, invitations, `/api/me/settings`; `tests/auth.rs` (new), `tests/common/mod.rs` (a third GitHub code `BOB_CODE` → `bob-dev`, `Harness::sign_in_as(code, device)`, `Harness::invite`) | `crates/service/src/{auth,principal,people,invitations}.rs`, `tests/{auth,api}.rs`, `tests/common/mod.rs` |
| **L1-groups** | `agent/l1-groups` | D5 groups, memberships, `AccessSource` + `Manual`, `expansion` and the cache; D7 agents, ceiling math, tokens, `principal_for_token`; the context API on an agent token (owner/actor stamping, `bad_access`, `AgentsAllowed`); traces admin-only and `principal` stamped in `retrieval::Caller`; `tests/groups.rs`, `tests/agents.rs` | `crates/service/src/{groups,agents,access,context_api,trace}.rs`, `tests/{groups,agents}.rs` |
| **L1-events** | `agent/l1-events` | D8 audiences on every emit and the per-socket filter with re-expansion; D6 ownership at write, list and item filtering, chat controls, settings admin gate; D9 in `turns`/`chats`; `ToolContext.principal`, `TurnInput.actor`, `search`'s `access`; `tests/events.rs` (new), the ownership cases in `tests/{chats,courses,sources,folders}.rs` | `crates/service/src/{events,state,chats,library,turns,ask,search,sources,uploads,folders,workspaces,agent_tools,ingest,settings,video,embedding}.rs`, `tests/{events,chats,courses,sources,folders,media,r1_e2e,agent_e2e,claude_e2e,providers,routes}.rs` |
| **L1-app** | `agent/l1-app` | D10 | `crates/app/**` |
| **L1-mcp** | `agent/l1-mcp` | D11 | `crates/mcp/**` |

Seams fixed here so the five build in parallel: (a) `principal.rs` is L1-auth's, but its `require_principal` calls `crate::agents::principal_for_token(state, token) -> AppResult<Option<Arc<Principal>>>` and `crate::groups::expansion(state, person_id) -> AppResult<Arc<Expansion>>`, both signatures in the first commit; (b) `state.rs` is L1-events', so L1-auth/L1-groups call `emit`/`emit_to`/`emit_row` as the first commit defines them and never add fields to `App` (anything they need — `expansions`, `access_sources` — is in the first commit); (c) `server.rs` is frozen after the first commit: a route an area needs that isn't there is reported to the orchestrator; (d) `tests/common/mod.rs` is L1-auth's: L1-events and L1-groups write tests against `Harness::sign_in_as` as the first commit declares it (a `todo!()` body until L1-auth lands; the integrator merges L1-auth first); (e) `crates/shared` is the orchestrator's. Merge order: L1-auth → L1-groups → L1-events → L1-mcp → L1-app.

### 4. E2E tests the integrator adds (`crates/service/tests/l1_e2e.rs`, through the real client, the fake GitHub, fakes only)

1. **Migration.** A store seeded (Mem) with two devices, three topics with lessons and notes, two chats, one source, one workspace, one document, no person: start → one owner person (`handle nkr1shna`, `Owner`, in `company`), both devices `person_id` = owner, every row `owner` = owner and `access == ["person/<owner>"]`, `document.owner` set and `document.access == ["company"]`, `people-v1` in `migrations`; start again → nothing changes (same ids, same counts).
2. **Two people, two devices.** Owner signs in (`OWNER_CODE`); `bob-dev` with `BOB_CODE` is denied "isn't allowed"; owner `POST /api/invitations { bob-dev, member, [company/engineering] }` (after `POST /api/groups company/engineering`); Bob signs in → a person (`Member`, groups `[company, company/engineering]` (the invited groups plus `company`), expansion `[company, company/engineering, person/bob]`), the invitation accepted; Bob's `GET /api/me` shows it. Owner creates a note; Bob's `GET /api/library` has no notes, `GET /api/search?q=` finds nothing, `GET /api/notes/{id}` (via `PATCH`) is 404; Bob's socket, open throughout, received **no** `NoteChanged`/`TopicChanged`/`DocumentChanged`, only `Hello`, `PersonChanged` and the `DeviceChanged` of his own device; the owner's socket received them. Bob creates a chat → the owner's `GET /api/chats` doesn't list it; the owner's socket got no `ChatChanged`/`ChatItemChanged`/`ChatItemDelta` while Bob's turn ran (fake model); Bob's `POST …/stop` works, the owner's on Bob's chat is 404.
3. **Roles.** Bob's `PATCH /api/settings` → 403 `admin_required`; `POST /api/groups` 403; `GET /api/traces` 403; `?traces=1` socket gets no `Trace`. Owner makes Bob admin → Bob invites `carol-dev`; Bob tries `role: admin` → 403 (owner only); Bob tries to disable the owner → 400 `owner_fixed`; Bob disables Carol after she signed in → Carol's socket closes with "device revoked", her `GET /api/me` is 401, her note stays hers (owner's library doesn't list it).
4. **Agent token.** Owner creates agent `claude-code` (`External`) → default ceiling `[company]` (no `person/owner`); mints a token (shown once; `GET /api/agents` has only the prefix); `POST /api/context/search` with it → 200 and the trace row has `principal == agent/<id>`, `actor == agent/<id>`; `GET /api/library` with it → 403 `agent_not_allowed`; `GET /api/events` → 403; a search with `access: ["person/<owner>"]` → 400 `bad_access`; ceiling `["company/sales"]` → 400 `outside_owner`; `PATCH` ceiling to include `person/<owner>` → allowed (it is the owner's); rotate → the old token is 401; revoke → 401; disable the owner → the agent's token is 401.
5. **Memberships move events.** Owner creates `company/engineering`, puts Bob in it; a second socket of Bob's open before the change; after `PUT /api/people/{bob}/groups []`, an event with audience `Paths(["company/engineering"])` (a test-only emit through `App::emit_to` in the harness) reaches Bob's socket before the change and not after, without reconnecting.
6. **Per-person settings.** Bob sets `default_provider: claude`, `reasoning_effort: high`; a chat Bob creates without `provider` is Claude and the fake Messages server sees `effort: high`; the owner's chats keep the org default.
7. **MCP** (`crates/mcp/tests`): `margin-mcp token` stores and `whoami` reports the agent; `serve` on the agent token lists and searches through the fake service.

App: `cargo test -p margin-app` with the fake's two persons (render tests of Organisation as admin and as member; invite dialog validation; ceiling tree). The iOS check and `dx build --platform android` after the merge.

Manual, before the phase is called done (recorded in the integration commit): on the dev guest, deploy, watch `people-v1` in the log, sign in on the Mac and the iPhone as the owner, invite a second GitHub account (the owner has one), sign in with it on the simulator, confirm the two libraries are disjoint and the owner's events don't reach the second device; create an agent, `margin-mcp token`, run a Claude Code session that searches. Tests never touch the running service or its data; no real model API is called.

### 5. Review lenses

- **One guard.** `grep require_device` finds nothing; `server.rs` has one `route_layer(require_principal)`; no handler reads `Authorization` itself; `CurrentDevice` is constructed only by the extractor.
- **Audience on every emit.** `grep "\.emit(Event::"` in `crates/service` lists only the `Everyone` events named in D8; everything else goes through `emit_row`/`emit_to`; `emit_row` is never given an empty slice (`debug_assert!` + a unit test); `DocumentChanged` is `Person(owner)`.
- **Private by default.** Every `Topic { .. }`, `Chat { .. }`, `Note { .. }`, `Source { .. }`, `Workspace { .. }`, `Proposal { .. }` literal in `crates/service` sets `owner: Some(principal.person_id)` and a non-empty `access` (grep the literals; the migration covers the rest).
- **List and item routes** carry `$paths`: `grep "store.list(db::\(TOPIC\|LESSON\|NOTE\|CHAT\|SOURCE\|WORKSPACE\|PROPOSAL\)"` in handlers finds only the migration, `recover`, and jobs that then filter by the row they hold; the `library::{topic,lesson,note}` helpers take a `&Principal`.
- **Roles.** `RequireAdmin` on exactly: settings PATCH, groups writes, memberships, invitations, people state/role, everyone's devices, traces; `RequireOwner` on admin-role changes and admin invitations. Admins read no content they don't own (no "admin bypass" branch anywhere in `library.rs`/`chats.rs`).
- **Tokens.** Agent tokens are hashed with `sha256_hex` like device tokens, returned once, never logged (grep `token` in `tracing::` calls), prefixed `mga_`; `random_token()` is the only generator. A ceiling is stored ancestor-closed and ⊆ the owner's expansion (property test over random trees).
- **Expansion invalidation.** Every write to `person`, `group`, `membership`, `agent` is followed by `expansions.invalidate_all()` **after** the write lock is released; the socket re-expands on it.
- **Agent surface.** `AgentsAllowed` marks only `/api/me`, `/api/projects`, `/api/context/*`; `margin-mcp serve`'s calls are a subset (`client.rs`).
- **No Keychain, no macOS.** No new `cfg(target_os = "macos")`; strings say "secret store".
- **Strings.** Every error names what to do ("Ask an admin to invite you.", "Only the organisation owner can make someone an admin."); the app shows the service's sentence, never a code.
- **Builds.** `cargo test -p margin-service` per area, the workspace after the merge, clippy and fmt, the iOS check, Android.

### 6. Risks to settle before branching

- **`state.rs` and `chats.rs` are hot files** (providers, R1, phase 4 all touched them): L1-events owns them alone; L1-auth and L1-groups add nothing to `App` (first commit) and emit through the three functions. If L1-groups needs state (the expansion cache), it is in the first commit.
- **`group` as a table name.** SurrealQL reserves `GROUP`; the first commit's `db.rs` test creates and lists the table through `Store::list` before anything else is built; if it fails, the name is `org_group` everywhere and the brief is edited, not worked around in one place.
- **`require_principal` before the migration ran.** The first commit's stub synthesises the bootstrap principal; once `people-v1` exists every device must have a `person_id` or be refused. A device row written by a `poll` racing the migration can't happen (the migration runs before serving), but the test for e2e 1 restarts the service with the store kept to prove the second start is a no-op.
- **Events re-expansion on the socket** adds a `watch` to every socket; keep it one `u64` generation (`App.expansions.generation`), compared on each event, not a per-event store read.
- **The owner's own content goes private, including for Margin's own agent on a device token.** Fine in L1 (the owner is the principal). The ChatGPT/OpenAI/Claude provider settings stay org-wide; per-person keys are not in scope (one org, one set of keys, plan §3).
- **A second untrusted account** is still not safe until L2 (D6's gap list). The roadmap row says so.
- **`crates/agent` change** is one field (`TurnInput.actor`); 4-agent-style ownership isn't needed, the orchestrator does it in the first commit and L1-events reads it.

### 7. What L2 builds on (so nothing here is redone)

`Principal.paths`/`deny_ids` and the cache; `access`/`deny` fields and indexes on every library row and `document.owner`; every retrieval call site already passing `principal.paths` (L2-retrieve turns `Request.access` into a required `Principal` and re-stamps `document`/`passage.access` from the owning rows in a `passages-access-v1` pass); the `library::*` helpers taking `&Principal` (L2 extends them to the agent tools' by-id reads); `emit_row(&row.access, ..)` (publishing just changes `access` and re-emits); `trace.principal` (L2-audit's "who could have seen what"); `AccessSource` (L6); `AgentToken.expires_at` on the row (L5 mints 2-hour voice tokens through the same route); `MENTION_RECIPIENTS_MAX` and the people directory (L3); `PersonSettings` (L5's `voice`).

### 8. Owner questions (with recommendations; none block)

1. **Ownership transfer.** The bootstrap owner is fixed in L1 (can't be disabled or demoted; `allowed_github_login` changes are logged and ignored). Recommend **as designed**; a `POST /api/people/{id}/make-owner` by the owner is one route later if needed. *Not blocking.*
2. **Person switcher in the sidebar** (plan §7 L1-app): read as "the account row shows the signed-in person"; no multi-account switching on one device. Recommend **no switcher** (Sign out → sign in). *Not blocking.*
3. **Who may invite admins and change roles.** Recommend **owner only** for the Admin role; admins invite and manage members. *Not blocking.*
4. **Everyone can list people and groups** (a directory for L3 mentions; names and handles only, never content). Recommend **yes**. *Not blocking.*
5. **Invitations expire after 30 days**; a revoked or expired one can be re-sent. Recommend **30 days**. *Not blocking.*
6. **Traces admin-only** (plan §2.4), including the `?traces=1` stream a member's app could open. Recommend **admin-only**; the eval runner uses an admin's token. *Not blocking.*
7. **Members see the org AI settings read-only** (so they can pick a configured provider for their chats) rather than nothing. Recommend **read-only with the sentence**. *Not blocking.*
8. **`margin-mcp login` (device token, full access) stays** beside `margin-mcp token` (agent token, ceiling). Recommend **keep both**, document that the agent token is the one to use. *Not blocking.*

Notes for the owner, not questions: nothing is deleted or reassigned by `people-v1`; a second *untrusted* person should wait for L2; the Organisation pane works on phones; the macOS service backends are untouched by L1 and can be removed in parallel (no shared files but `secrets.rs`, which L1 doesn't edit).
