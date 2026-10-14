# Live context: product model, access control, accounts, inbox, meetings, voice — and the build plan

Written 2026-10-14 at `92b3d7d` (R1 first commit) on `claude/agent-build`. Read: `PLAN.md`, `docs/HANDOFF.md`, `docs/agent-architecture.md`, `docs/agent-rag-architecture.md` (§2.11, §2.12, §5.2, §6), `docs/agent-providers-brief.md`, `docs/agent-phase4-brief.md`; service `auth.rs`, `devices.rs`, `db.rs`, `server.rs`, `context_api.rs`, `state.rs`; shared `context.rs`, `routes.rs`, `lib.rs`. Studied for **behaviour only** (GPL-3.0-or-later; nothing copied): Zed `crates/{call,collab,channel,livekit_client,livekit_api,rpc}` (`call_impl/room.rs`, `call_impl/mod.rs`, `collab/src/rpc.rs`, `collab/src/db/queries/{rooms,channels,notifications}.rs`, `livekit_api/src/token.rs`, the collab test schema). Checked today on the web: LiveKit server, Rust SDK and Swift SDK licences (all Apache-2.0), LiveKit connect/reconnect and egress docs; ElevenLabs Agents docs (authentication, WebSocket and JS/Swift SDKs, client events, server/client tools, MCP, custom LLM, overrides and dynamic variables, conversation flow, RAG, post-call webhooks, Scribe v2, pricing). Where a fact is unverified it says so. No code changed.

Owner's direction (verbatim pieces, 2026-10-14): "sell this as a Live context app, for the projects in a company, so that it is a good interface for both agents and humans. humans through gui agents through context." "it needs to be divided and access controlled, executives see different knowledge than engineers … it is hierarchical knowledge. everyone has some knowledge base access like company policy, holiday lists … I am planning to drive this entirely with eleven labs api for realtime voice agent, so that this app will be voice first. the UI will change as well." "we also need an inbox feature to get notifications from both agents and other users … tagging each other must be enabled so account management is also required." "Access control for agents as well. meaning whose agents can see." "we also need meetings feature, check the zed repo for collaboration crate … we can extract transcripts from their meetings and that goes into knowledge base as well. Each employee has their own private knowledge base as well for their own notes which agents can or can not access. they can chose to publish anything if they want." And the standing stance: a start-up whose vision changes, so pluggable stages and configuration over code.

What exists that this builds on: one GitHub login (`allowed_github_login`), one device token per device (hashed, revocable, `require_device` on every `/api` route), one broadcast events socket every device receives in full, `project` (one, "My library"), `document.access: Vec<String>` and `passage.access` (hierarchical paths, default `["company"]`, indexed `access.*`, pre-filterable inside the KNN and FULLTEXT `WHERE`: `db.rs` tests prove the `CONTAINSANY` lands in the `KnnScan` predicate), `actor` on documents and traces, the context API routes (`/api/context/*`, R1 fills them), traces in SurrealDB time series.

---

## 1. The product model (one page)

**Org.** One Margin service = one organisation. There is no multi-tenancy inside a service; a second company is a second service. (Risk §9.1: the service is one Mac.)

**People.** A `person` has a GitHub login (sign-in), display name, handle (`@krishna`), state (invited / active / disabled), org role (`owner | admin | member`). Devices belong to a person; a device token identifies a person. A person has a **private knowledge base**: everything they create defaults to audience `["person/<id>"]` (notes, chats, uploads, folder files, meeting notes). Publishing = widening the audience (§2.4).

**Groups** are hierarchical paths: `company`, `company/engineering`, `company/engineering/platform`, `company/executives`, `company/sales/emea`. A person is a member of zero or more groups; membership of a group implies membership of its ancestors (`company/engineering/platform` ⇒ `company/engineering` ⇒ `company`). Everyone is in `company` (policies, holidays). Groups may later be mirrored from a source system (GitHub org teams, Google Workspace groups; §2.7). A **project** is a corpus (R1's `project` record) and belongs to the org; a project is not a group: group membership says who, project says what. Default: one project per company project/team, as §5.2-2 of the RAG doc recommends.

**Agents are owned actors.** An `agent` row has an owner (a person), a name (`@triage-bot`), a kind (`margin` = Margin's own loop on a provider; `external` = something calling the context API or MCP with a token; `voice` = the ElevenLabs agent acting for a person in a conversation), a token (hashed; external and voice agents), and an **access ceiling**: a set of principals the agent may act as, always a subset of the owner's. Default ceiling = the owner's full access; the owner can narrow it ("my coding agent sees `company/engineering` but not my private KB"). "Whose agents can see" is exactly this: an agent never sees more than its owner, and the owner decides how much less. Audit rows record `actor = agent:<id>` and `on_behalf_of = person:<owner id>`.

**Documents** (R1's `document`, plus every library row that is indexed: notes, lessons, chats, meeting transcripts) have an **owner** (a person, or an agent whose owner is then the accountable person) and an **audience**: `access: Vec<String>` of principals who may read, where a principal is a group path, `person/<id>`, or `agent/<id>`; plus `deny: Vec<String>` (same alphabet; wins over `access`). Writers: the owner, org admins, and `edit: Vec<String>` (later; not needed for the first phases).

**Derived content** (a lesson written from three documents, an outline, a summary, a meeting's "decisions" note, an agent's answer card, a retrieval-cache entry) carries the **intersection of its sources' audiences** (§2.4): a lesson from an engineering doc and a company-wide doc is engineering-only. The intersection is computed where the content is created (the packer knows the packed passages), stored on the row, and the row is then an ordinary document.

**Meetings** are rooms with participants; a recorded meeting produces a transcript document owned by the host with audience = the attendees who consented (and the room's group if it is a group room), later publishable by the host.

**Inbox**: notifications addressed to a person from people, agents and the system (mentions, invitations, incoming calls, meeting transcripts ready, access granted, an agent finished a job).

**Voice** is a way of talking to one's own agent (and later to a meeting): the voice agent always acts **as one signed-in person**, under that person's access, with the context API as its tool.

---

## 2. Access control, enforced inside retrieval

### 2.1 Principals and the request context

Every `/api` request resolves to a `Principal` before any handler runs (the current `require_device` middleware becomes `require_principal`):

```rust
pub struct Principal {
    pub kind: PrincipalKind,         // Person | Agent { owner: PersonId } | Service (jobs that run for a person carry that person)
    pub id: String,                  // "person/<id>" | "agent/<id>"
    pub paths: Vec<String>,          // the EXPANDED read set: own id, every group and every ancestor, "company"; for an agent: owner's set ∩ ceiling
    pub deny_ids: Vec<String>,       // the ids a `deny` entry can name for this principal (own id + groups)
    pub org_role: OrgRole,
    pub device_id: Option<Id>,
}
```

Expansion happens once per request from a small cache keyed by person id, invalidated on any membership, group, or agent-ceiling change (`Event::AccessChanged`). Expansion is where hierarchy lives: the index stores flat arrays; the *caller* carries all the paths they are entitled to. A document shared with `company/engineering` is found by a member of `company/engineering/platform` because that member's `paths` include `company/engineering`.

### 2.2 The query rule (the only rule)

Every read of indexed content is a SurrealQL statement whose `WHERE` contains both conjuncts, bound from the principal, **inside** the KNN / FULLTEXT scan:

```sql
WHERE vector <|$k, 128|> $q
  AND access CONTAINSANY $paths          -- $paths = principal.paths
  AND NOT (deny CONTAINSANY $deny_ids)   -- $deny_ids = principal.deny_ids
  AND $scope IN scope_ids AND modality IN $modalities
```

(`$access` is a protected name in SurrealDB 3.3.4; R1 already binds `$paths`.) `deny` is a new `array<string> DEFAULT []` field on `passage` and `document`, indexed `deny.*`. Both legs of the hybrid query, the `read_passage`/`document_outline` lookups, the change feed, ⌘K on lessons/notes, and `search_library` carry the same conjuncts. The retrieval cache key includes `hash(principal.paths, deny_ids)`. Counts, highlights, scores and "N results" are therefore computed over the permitted set only; no post-filter exists anywhere in Rust. Review lens for every later phase: `grep` for a retrieval statement without `$paths` fails the review.

**Default deny.** A row's `access` is never empty: at write time it is `[owner principal] ∪ audience`. A document with no audience is private to its owner. Tables that are not yet schemafull (`note`, `lesson`, `chat`, `chat_item`, `topic`) get `access`/`deny` under `doc.` with `doc.access.*` indexes, and their list routes add the same conjuncts (`GET /api/library` returns the principal's slice; `GET /api/chats` the chats they own or were shared). Agent tools take `&Principal` as a parameter; there is no constructor of `retrieval::Request` without one (`access` goes from `Option<Vec<String>>` to required in the retrieval layer; the wire type keeps `Option` for compatibility and the handler fills it from the principal, refusing a caller-supplied value that is not a subset of their own).

### 2.3 Humans and agents

- A person's effective read set is their expansion. An admin is not omniscient: `org_role` gates *administration* (groups, invitations, agents), not reading; an admin who wants to read a private KB cannot. Owner-level export exists (`margin-service export`, on the Mac, logged) for legal reasons — a deliberate, visible path, not an API.
- An agent's `paths` = owner's expansion ∩ ceiling, computed the same way. An agent token is never a device token: it cannot open the events socket as a device, cannot run device tools, cannot administer. Margin's own agent (a chat turn) runs under the **person who sent the message** with the chat's attached agent ceiling if the chat is bound to an agent; `TurnInput` gains `principal`. Jobs started by a tool (lessons, videos) carry the starting principal and the derived audience.
- The voice agent (§6) is an `Agent { kind: Voice, owner: the speaker }` with a short-lived token minted per conversation.

### 2.4 Audience of derived content

`audience_of(sources) -> Audience`: when all sources' `access` arrays are identical, that array; otherwise the **greatest lower bound** where it is expressible as paths (common ancestors only, e.g. `company/engineering` ∧ `company/engineering/platform` = `company/engineering/platform`; `company/sales` ∧ `company/engineering` = nothing in common below `company`, and `company` is *not* the answer because it would widen); where no path array expresses the intersection, the derived row falls back to **private to its creator** with a notice in the UI ("Visible only to you: it draws on documents with different audiences. Publish to … to share."). `deny` = union of the sources' denies. This rule is conservative and simple; an exact "for all sources" check per reader would need `array<array<string>>` and a per-row loop in the query, which would break the pre-filter rule. The retrieval cache stores the principal hash, so derived caches never cross principals; traces store `hit_ids` (ids, not text) and are readable only by admins and the eval runner.

**Publishing** is `PATCH /api/documents/{id}/access { access, deny }` by the owner (or admin for org documents), journaled (§2.5), re-stamped onto the document's passages in one `UPDATE passage SET access = $a, deny = $d WHERE document_id = $id` (indexed), with `Event::DocumentChanged` so other agents' change feeds see it. Derived rows are not re-derived automatically when a source is published (they keep their stored audience; the UI offers "Recompute audience").

### 2.5 Audit trail

A new time-series table `audit:[kind, at, ulid]` (same pattern as `trace`): `kind ∈ { access_change, membership_change, agent_change, read_private, export, meeting_record }`, `actor`, `on_behalf_of`, `subject` (document/person/group/agent id), `before`/`after` (ids and paths, never content), `device_id`. "Who could have seen what" is answerable from `audit` + `trace` (`principal` is added to `trace`; `hit_ids` already exist). Retention: audit rows are never auto-deleted. Admins read `GET /api/audit?kind&from&to`.

### 2.6 What the UI shows

On every document, note, chat, lesson and meeting: an **Audience** control (owner only): "Only me · `@group` picker · people picker · Deny list", with the computed audience shown read-only on derived content and a "Publish…" action on private rows. In Settings → Organisation (admins): People, Groups (tree), Agents (owner, ceiling, token rotate/revoke), Audit. Every control functional (owner rule).

### 2.7 Permission sync from source systems (later)

`trait AccessSource { fn name(); fn groups() -> Tree; fn members(group) -> Vec<ExternalId>; }` behind a registry like the pipeline's: `manual` (default), `github-org` (teams → paths `company/<team-slug>`, members by GitHub login — Margin already knows logins), `google-workspace` (groups by email). A sync job writes `group`/`membership` rows with `source = <name>` and never edits rows whose source is `manual`. Document audiences themselves are never synced from a source system in the first version (a GitHub repo's collaborators → the repository document's audience is the first candidate, in the repository-source phase).

---

## 3. Accounts and multi-user auth

**From today:** `DeviceRow { github_login }` and `allowed_github_login`. **To:** a `person` table (`id, github_login, github_id, handle, name, avatar_url, org_role, state, created_at`), `DeviceRow.person_id` (migration: every existing device → the one person created from `allowed_github_login`, who becomes `owner`), `membership { person_id, group_path, source }`, `group { path, name, source, created_at }`, `agent { id, owner_id, name, kind, token_hash, ceiling: Vec<String>, created_at, last_used_at }`, `invitation { id, github_login | email, groups, invited_by, expires_at, accepted_at }`.

**Sign-in stays GitHub OAuth, browser redirect, app polls** (the flow in `auth.rs` is kept; only the admission check changes): the callback admits a login that is (a) an active person, or (b) a pending invitation for that login (the person is created, invitation consumed, groups applied), else "not invited". `allowed_github_login` stays as the bootstrap owner (first start creates the owner person). Invitations by GitHub login first (we know the identity at callback); by email only when a mail sender exists (Q6). Org-membership auto-admission (`github-org` AccessSource: anyone in the org's GitHub organisation may sign in and lands in `company`) is the second step and makes onboarding one click for engineering-heavy companies.

**Tokens.** Device tokens unchanged (per device, per person, hashed, revocable in Settings → Devices, which becomes per person). Agent tokens: `POST /api/agents/{id}/token` returns a token once, prefix `mga_`, hashed; `require_principal` accepts either and builds the `Principal`. Short-lived voice tokens (§6) are agent tokens with `expires_at` ≤ 2 h, never listed.

**Events become per-principal.** Today one `broadcast` channel feeds every socket everything. With two people this leaks (a lesson delta from a private course reaches every device). `events::run` keeps the broadcast, but each socket **filters** with its principal: every `Event` gains an `audience: Vec<String>` (set where the event is emitted, from the row's `access`, or `["person/<id>"]` for personal events; the `DeviceChanged`/`SettingsChanged` events become per-person), and the socket sends an event only if `audience CONTAINSANY principal.paths`. Directed frames (tool requests) are unchanged. Chat turns keep running where they run; stop/continue are allowed to the chat's audience, confirmations only to its owner.

**Settings split.** `Settings` today is one row for the owner. It splits into org settings (admins: providers and keys, pipeline, retention, LiveKit, ElevenLabs), and per-person settings (default provider for *their* chats, voice, appearance, reasoning effort). Keys stay in the Mac's Keychain; one org, one set of keys; per-person spend is reported from traces (`principal` column).

**Rate limits and abuse.** `/auth/start` and `/auth/poll` limits exist; add per-person limits on `/api/context/search` for agent tokens (config), and a per-agent daily spend cap on Margin-run agents (config, default off).

---

## 4. Inbox and @mentions

**Model.** `notification { id, recipient: "person/<id>", kind, actor, entity: { kind, id }, text, created_at, read_at, response: Option<bool>, audience_hint }`, kind ∈ `mention | invitation | call_incoming | meeting_transcript_ready | access_granted | agent_done | agent_needs_confirmation | system`. Zed's shape (recipient, kind, entity, content, is_read, response; duplicate suppression by `(recipient, kind, entity)` while unread; pagination `before_id`) is the right one and is re-implemented, not copied. Delivery: written under the write lock, then `Event::Notification` with audience `["person/<id>"]` to that person's sockets (an "Inbox" badge), plus a push notification later (APNs/FCM; Q7). Read/unread and "respond" (accept/decline an invitation or a call) are `PATCH /api/notifications/{id}`. Retention 180 days, range-deleted like traces (ids `notification:[recipient, at, ulid]` so one person's inbox is a range scan).

**Mentions.** `@handle` for people, `@agent-name` for agents, `@group-path` for groups (expands to members; capped at 50 recipients, else refused with a notice), in: chat messages, notes, meeting chat, comments on a document (comments are a small new row type on any document; they are the natural place for humans and agents to talk *about* a document). The parser is shared (`margin_shared::mentions`), tested on handles with dots/dashes, code spans (not a mention), and emails (not a mention). A mention is only delivered if the mentioned principal **can see the row** (audience check at write time; otherwise the composer shows "@bob can't see this note — share it with bob?" and offers to add them). This is the one place where the owner's "tagging each other" and "access controlled" meet, and it has to be in the composer, not after the fact.

**Mentioning an agent** creates a turn in a chat *between the mentioner and that agent* (a new chat if none), with the mention's row as attached context, running under the **mentioner's** principal ∩ the agent's ceiling ∩ the agent owner's access (so a person cannot use someone else's agent to read what neither can read). The agent's answer is posted back as a comment on the row (audience = intersection) and as an `agent_done` notification. Agents never post unprompted; a scheduled or event-driven agent (later) posts only to its owner.

**UI.** An Inbox section in the sidebar (badge), a list grouped by day with inline actions (Open, Accept/Decline, Mark read, Mute this thread), keyboard and touch. Voice: "What's in my inbox?" is a voice-agent tool (`list_inbox`, `mark_read`).

---

## 5. Meetings

### 5.1 What Zed does (behaviour to re-implement, from `call`, `collab`, `channel`, `livekit_*`)

- **Rooms** are server rows with a LiveKit room name (random, 30 chars) and an optional **channel** (a persistent group room that stays open when empty); non-channel rooms are deleted when the last participant leaves. The server keeps **participants** with `answering_connection` (which socket serves them), `answering_connection_lost`, `calling_user` (who invited), a stable `participant_index` (lowest free, used for colours), a **role** (`admin | member | talker | guest | banned`) and a **location** (which shared project they are in, or "external"). Membership, roles, projects, locations and follow state go over the app's own RPC; **audio, video, screen share, active speakers, mute state and connection quality go over LiveKit**, participants matched by user id.
- **Calling**: inviting creates a *pending* participant row (no answering connection); the callee's every connection gets the incoming call; accept joins the room, decline/cancel deletes the pending row; if no connection of the callee could be reached the call fails. Only contacts can be called.
- **Tokens**: the server mints the LiveKit JWT (HS256 with the API secret, `iss` = key, `sub` and `jti` = identity, `nbf` = now, TTL 6 h) with a video grant `room`, `roomJoin`, `canSubscribe: true`, `canPublish` from the role (guests cannot publish; admins/members/talkers can). Role changes push updated permissions to LiveKit through its server API; guests demoted mid-call lose their mic and unshare.
- **Reconnection**: on socket loss the server marks the participant's connection lost but keeps the row for a **30 s reconnect window**; the client enters `Rejoining` and makes up to three rejoin attempts within 30 s, re-sending the state it needs restored; if the window passes, the server's `connection_lost` path leaves the room for that session (and declines pending calls when the user has no other online connection). A LiveKit-side disconnect makes the client leave the room. Stale rows from a previous server instance are cleaned at startup. The app quitting without leaving is handled by cleaning the stale connection on the next join.
- **Mic/deafen/screen share**: the mic track is opened at join even if "mute on join", so the audio device switches early; deafen mutes playback and the mic unless the mic was muted by hand; screen share goes `pending → published` with a publish id so stale attempts can be cancelled; sounds mark join/leave/share.
- **Channels** form a tree (`parent_path` of ancestor ids, ordered siblings); membership and roles are stored on the **root** channel and inherited down; visibility `public | members` with the rule "public only under a public parent"; invitations are membership rows with `accepted = false` that also create a notification; joining a public channel grants `guest`.
- **Notifications**: a row per recipient with kind, entity, content, read and response flags; sent to every connection of the user; paginated newest-first.

### 5.2 Margin's meetings on LiveKit (all Apache-2.0: server `livekit/livekit`, `livekit/rust-sdks`, `livekit/client-sdk-swift`, `livekit-client` JS, Android SDK)

**Server.** A self-hosted LiveKit server (single binary, YAML config with `keys:`, ports 7880 HTTP/WS, 7881 TCP, UDP 50000–60000, optional TURN 443) runs beside `margin-service` — on the service Mac for the first version, as a `launchd` job the Settings → Tools pane detects like `manimgx` (install command to copy, never run). Media does **not** go through the Cloudflare Tunnel (the tunnel carries only HTTP; WebRTC needs UDP or TURN/TCP): LiveKit's signalling can go through the tunnel on a second hostname (`live.margin.…`), media needs either a public UDP range on the Mac's network or TURN over TCP 443 on a host with a certificate. This is the first real infrastructure question (Q10): for a company, LiveKit on a small Linux VM (or LiveKit Cloud) is the honest default; the Mac-only setup works on one LAN.

**Rooms in Margin** (`room`, `room_participant` tables; routes `POST /api/rooms` (ad hoc or for a group → "channel-backed"), `POST /api/rooms/{id}/join|leave|invite|cancel|decline`, `PATCH …/participants/{person}/role`, `GET /api/rooms` (the ones the principal may see)). A room has an audience like a document (`access`): group rooms are visible to the group; ad hoc rooms to invitees. Roles: `host` (admin), `member`, `guest` (listen only), `banned`; `canPublish` follows the role in the token Margin mints (`livekit-api` crate, Apache-2.0, or a 40-line HS256 signer on `jsonwebtoken`; the claims are LiveKit's documented ones: `iss`, `sub`, `jti`, `nbf`, `exp` ≤ 6 h, `video { room, roomJoin, canPublish, canSubscribe, canPublishData }`). Presence: `answering_device` + `lost` flag, a **30 s rejoin window** on the events socket (the `DeviceHub` already knows when a socket closes; `turns` already survives it), stale cleanup at startup, incoming calls as pending participants delivered to every device of the callee (`call_incoming` notification with ring UI on the device in front). Mute/deafen/screen share semantics as above.

**Clients.** The Dioxus app is a webview on every platform. First version: `livekit-client` (JS) inside the webview for audio and screen share, driven from Rust through the existing JS bridge: WebRTC `getUserMedia` works in WKWebView (macOS, iOS 14.5+) and Android WebView; **`getDisplayMedia` (screen share) does not exist on iOS WKWebView and is limited on macOS WKWebView** — the macOS desktop app should capture the screen natively (ScreenCaptureKit via `objc2`) and publish through the Rust `livekit` crate, which is what Zed does in Rust (it uses the `livekit` and `libwebrtc` crates on a Tokio runtime; heavy native build, needs libwebrtc). Recommendation: JS client for audio everywhere and screen share on macOS Chrome-class webviews where it works; native Rust `livekit` for macOS screen share as its own area (4a-style, desktop only is *not* a scope narrowing here: iOS/Android can view screen shares, they cannot capture them in a webview, and ReplayKit on iOS is a later native area). Q11 asks the owner.

**Recording and transcription → KB.** Two options, both without LiveKit Egress (Egress needs a separate GStreamer + Chrome service; self-hosted it is the heaviest piece in the stack):
1. **Recorder participant** (recommended): `margin-service` joins the room as a hidden participant with the Rust `livekit` SDK (`hidden: true` grant), subscribes to every audio track, and writes **one file per participant** (`meeting/{id}/{person}.ogg`). Because tracks are per participant, **speaker identity is exact without diarization**. After the meeting (or every 5 min for a live transcript), each file is transcribed with the pipeline Margin already has (FluidAudio Parakeet, local, free; word timestamps) — or ElevenLabs Scribe v2 batch ($0.22/h, 90+ languages, word timestamps) as a config switch behind `trait Transcriber`. The merged, speaker-labelled transcript becomes a document (`Origin::Meeting { room_id }`) chunked by speaker turn and time window (the audio chunker exists), audience = consenting attendees (+ the room's group when the host says so), owner = host; "Summary and decisions" is a derived note on the `Write` route with the same audience, posted to the attendees' inboxes.
2. Egress room-composite MP4 for a watchable recording: later, if asked.

**Consent.** Recording is off by default; the host starts it; every participant sees a persistent "Recording" indicator and a one-time prompt "Allow Margin to record and transcribe your audio in this meeting? (Allow / Join without recording)". A participant who declines is **not recorded** (their track is not subscribed by the recorder; their words that others answer are of course in the others' tracks — the prompt says so). The consent decision is an `audit` row (`meeting_record`). A room's recording state is in `Event::RoomChanged`. Live captions (Scribe v2 Realtime, ~150 ms, no diarization in realtime — not needed, one stream per track) are a later option.

**What Margin does not copy from Zed**: project sharing, following, buffers (Zed's collaborative editing is a code-editor feature). Margin's "location" equivalent is **"what I'm looking at"** (a lesson, a document page), shared as presence so the voice agent and the other participants can say "the page Krishna has open".

---

## 6. Voice-first with ElevenLabs

### 6.1 Facts checked (docs, 2026-10-14)

- Conversations run over **WebRTC** (voice; the SDKs' default) or **WebSocket** (text or fallback). Private agents need either a **signed URL** (`GET /v1/convai/conversation/get-signed-url`, WebSocket, valid 15 min to *start*) or a **conversation token** (`GET /v1/convai/conversation/token`, WebRTC); both are fetched server-side with the API key, which must never reach the client. Signed URLs do not identify users: the app must authenticate first and request one per session. Allowlists (origin hostnames) are the alternative and don't apply to a native webview.
- SDKs: JavaScript (`@elevenlabs/client`: `connectionType`, `conversationToken`/`signedUrl`, `clientTools`, `dynamicVariables`, `overrides`, `sendUserMessage`, `sendContextualUpdate`, `sendUserActivity`), Swift (iOS 14+/macOS 11+, `auth: .conversationToken`, client tools with `expectsResponse` / `sendToolResult`), Kotlin, React Native. Whether `@elevenlabs/client` works inside WKWebView/Android WebView over WebRTC is **unverified**; the WebSocket path with PCM 16 kHz in / 44.1 kHz out (`conversation_initiation_metadata` reports the formats) is the fallback and is fully documented.
- **Tools**: server tools (webhooks: URL, method, path/query/body parameters the LLM fills, custom headers, secrets, auth connections; a tool response can set dynamic variables), client tools (run in the app; "wait for response" returns a value to the LLM), **MCP servers** (SSE and streamable HTTP; secret token header; approval modes always-ask / fine-grained / none; off by default per workspace; unavailable under zero-retention/HIPAA), system tools (end call, language, transfer, skip turn). Client events include `user_transcript`, `agent_response`, `agent_response_correction`, `client_tool_call`, `agent_tool_response` (+ `_full_payload`, 64 KB cap), `mcp_tool_call`, `vad_score`, `agent_chat_response_part`, `agent_response_complete`, `interruption`, `ping`. Post-call webhooks deliver the full transcript with tool calls, metadata (duration, cost, termination), dynamic variables, and optionally the MP3 (HMAC-signed `ElevenLabs-Signature`).
- **LLM**: hosted choices include Claude Opus/Sonnet/Haiku 5.5-class, GPT-6.x, Gemini; or a **custom LLM** that must speak OpenAI Chat Completions or Responses over SSE, publicly reachable, with function calling for system tools; slow models get a buffer phrase; reasoning must be returned separately. Dynamic variables (`{{name}}`, `secret__` prefix: used only in tool headers, never shown to the LLM, redacted in transcripts), overrides (system prompt, first message, voice, language, LLM, tools; each must be enabled in the agent's security settings). Turn-taking: `turn_timeout` 1–30 s, eagerness eager/normal/patient, soft-timeout filler, interruptions on/off; max duration default 600 s, up to 7 200 s. Built-in RAG exists (per-workspace limits 1 MB–1 GB, ~250 ms) — **not used**: Margin's retrieval is the product.
- **Latency**: ElevenLabs' own guide (reported second-hand) puts median time-to-first-audio around 680 ms, p95 ~1.5 s, with LLM time-to-first-token and endpointing the largest items; Flash TTS ≈ 75 ms. Scribe v2 Realtime ≈ 150 ms. Pricing: $0.08/min standard, $0.16/min burst, LLM tokens billed on top (provider rates; Claude/Gemini +10 % via Vertex); Scribe v2 $0.22/h batch, $0.39/h realtime.

### 6.2 Architecture

```
App (Dioxus webview)                         margin-service (Mac)                       ElevenLabs
┌──────────────────────┐  POST /api/voice/start  ┌─────────────────────────┐  token API  ┌────────────┐
│ Talk button, live    │ ───────────────────────▶│ mint voice agent token  │───────────▶│ Agents     │
│ transcript, cards    │ ◀── {conversation_token,│ (person-scoped, 2 h),   │◀───────────│ platform   │
│                      │      voice_token, vars} │ request ElevenLabs token│            │            │
│ @elevenlabs/client   │ ═══ WebRTC audio ═══════╪═════════════════════════╪═══════════▶│ ASR→LLM→TTS│
│ client tools:        │ ◀── client_tool_call ───┼─────────────────────────┼────────────│            │
│  navigate, show_     │                         │ /api/context/search ◀───┼── server tools (webhooks, Authorization: Bearer {{secret__margin_token}})
│  passage, open_      │                         │ /api/context/documents  │            │            │
│  citation, confirm   │                         │ /api/voice/tools/*      │            │            │
└──────────────────────┘                         │ post-call webhook ◀─────┼────────────│ transcript │
                                                 └─────────────────────────┘            └────────────┘
```

- **One agent definition per org** (created by Margin through the ElevenLabs API at setup; the key lives in the Keychain as `elevenlabs-api-key`), with overrides enabled for system prompt and first message, server tools pointing at the service's public URL, and client tools registered by the app. **Per conversation**, the service mints a `voice` agent token for the signed-in person (`Principal` = that person ∩ the voice agent's ceiling, which the person sets in Settings → Voice: "My voice assistant may read: everything I can / only these groups / not my private notes"), fetches the ElevenLabs conversation token, and returns both with dynamic variables: `secret__margin_token` (into the tools' `Authorization` header; never seen by the LLM, redacted in transcripts), `person_name`, `handle`, `now`, `looking_at` (presence), and a system-prompt override that states the person, their groups, and the rules ("quote `n` from search results; say when you searched; don't invent documents").
- **The context API is the tool.** Server tools: `search_context(query, scope?, k≤6)` → hits with `n`, title, section, ≤ 400 chars each (voice needs short); `read_passage(document_id, ord)`; `document_outline`; `list_inbox` / `mark_read`; `create_note(title, body, audience=me)`; `start_meeting(with)`; `send_message(to, text)` (mention). Each is one handler in `voice_tools.rs` that calls the same functions as the agent tools and the context API (one code path, one access rule, one trace). Client tools: `navigate`, `show_passage`, `open_citation`, `toggle_sidebar` (the existing app-control device tools, so the voice agent "drives" the GUI the same way the chat agent does), and `confirm(question)` (destructive actions render the existing Confirmation card; "wait for response" returns the answer).
- **LLM choice.** First version: **ElevenLabs-hosted LLM** (Claude Sonnet 5.5-class or GPT-6.1 Sol; config), because the tool loop then runs next to ASR/TTS and the custom-LLM hop (ElevenLabs → tunnel → Mac → provider → back) adds a full round trip to every token. Second version, behind a Settings switch: **custom LLM = Margin's own loop** exposed as `/v1/responses` SSE (`voice_llm.rs`), so the voice agent *is* the chat agent (same tools, confirmations, traces, provider switch, history in a Margin chat). The eval (§6.4) decides whether the latency cost is worth the single loop. Keep both behind `trait VoiceBrain`.
- **The conversation is a Margin chat** (`Chat.provider = Voice`, items from `user_transcript`/`agent_response`/`agent_tool_response` events as they arrive over the client SDK, finalised from the post-call webhook, with tool results linked to Passages cards); so the transcript is searchable context the moment it ends, under the speaker's audience, and the GUI shows it like any chat (voice-first, not voice-only: the composer still types; `sendUserMessage` feeds typed text into the same conversation).
- **Speaker identity.** A conversation belongs to exactly one signed-in person (the token proves it; the agent prompt names them; tools act as them). Margin never tries to identify a speaker by voice. In a meeting (§5), the voice agent is a later feature: it would join as a LiveKit participant and receive per-participant audio, so identity stays exact; whether ElevenLabs can join a third-party LiveKit room is **unverified** (Q13). Until then, "ask the agent" during a meeting is a private side conversation on the asker's device.
- **Privacy**: audio and transcripts go to ElevenLabs (their retention settings apply; MCP and some features are off under zero-retention). The tool payloads (passages) also go to their LLM. The Audience control and the voice ceiling are the only guards; the org admin must accept this in Settings → Voice with the retention mode shown. Secret dynamic variables keep the Margin token out of the model.

### 6.3 Latency budget (end of speech → first audio), with a tool call

| Stage | Budget | Measured by |
|---|---|---|
| Endpointing (VAD + turn eagerness "normal") | 300–500 ms | ElevenLabs turn metrics (post-call payload) |
| ASR final | ~150 ms | same |
| LLM TTFT (hosted, tool call decided) | 300–600 ms | same |
| Server tool: tunnel + `search_context` | ≤ 250 ms (retrieval p95 400 ms target in R1 is too slow for voice: voice uses `k_dense = k_lex = 20`, no rerank, the cache warm) | `trace` rows with `principal`, `route: voice` |
| LLM second TTFT with results | 300–500 ms | ElevenLabs |
| TTS first chunk (Flash) | ≤ 100 ms | ElevenLabs |
| **Target** | **p50 ≤ 1.3 s with a tool, ≤ 0.8 s without; p95 ≤ 2.5 s** | `evals/voice` suite reading both |

Mitigations that are config: soft-timeout filler at 2.5 s ("Let me check…"), tool-call sounds, `sendContextualUpdate` of presence instead of tools for "what am I looking at", short tool outputs, prompt caching on the hosted side (constant prefix first, person block last).

### 6.4 Evals for voice

A `voice` suite in `crates/evals`: recorded conversations (text mode over WebSocket, so CI needs no audio) against the fake service; metrics: tool-call precision (did it search when it should), answer faithfulness to the returned passages (same judge rubric as lessons), latency per stage from post-call payloads (opt-in runner only), cost per minute. Both `VoiceBrain`s run on the same set.

### 6.5 UI direction

Voice-first changes the shell, not the data: a persistent **Talk** control (push-to-talk and hands-free, mute, "Agent is listening/thinking/speaking" state), a **live transcript** pane that is the chat, agent-pushed **cards** (passages, a lesson, a document page, an inbox item) that the agent navigates to through client tools, and every screen reachable by voice *and* by hand (the owner's "every element functional" rule applies to voice: every tool the agent has is also a visible control). Mobile: the Talk control is the primary button; desktop: ⌘-Space-style shortcut. The sidebar grows Inbox, People, Meetings; documents grow Audience. Mockups follow the owner's "the UI will change": a new option in `mockups/` before the voice phase, reviewed like Option D was. Nothing is rebuilt before the model exists (phases L1–L2).

---

## 7. Phased build plan

Shape as before: a shared first commit on `claude/agent-build` by the orchestrator, areas in worktrees `agent/<phase>-<area>` with exclusive files, integrate, e2e, two reviewers, a fixer; the Fable architect reviews each brief first. Tests on fakes only (a fake LiveKit signalling server is *not* built: the room state machine is tested against a fake `LiveKitApi` trait and the JS client is exercised by the app's render tests; a fake ElevenLabs token/webhook server is an axum fake like the Responses one).

**Current queue** (unchanged): R1 + phase 4 → providers → Q (quizzes) → R2 → phase 5.

**What should move earlier.** (a) **L1 (accounts and principals) belongs before R2**: R2's MCP adapter needs per-agent tokens and a `Principal`, which L1 defines; building R2 on the device token and redoing it is throwaway. (b) **Q (quizzes) should be re-examined** against the new vision (Q1 below): quiz attempts and grades are per person, so Q needs L1 too, and quizzes are a study-app feature the company-context product may not lead with. Recommended order: **R1+4 → providers → L1 → R2 (with MCP on agent tokens) → L2 → phase 5 → Q → L3 → L4 → L5 → L6**. (c) Two small things go into **R1/phase 5 now** so they aren't redone: `deny` beside `access` on `passage`/`document` (one field and one index line, R1-store), and `audience` on `Event` (phase 5, when the events socket is hardened), both additive.

### L1 — Accounts, principals, agents, per-principal events

- **First commit**: shared `Person`, `Group`, `Membership`, `Agent`, `Invitation`, `Principal` wire types, `OrgRole`, `Event::{PersonChanged, GroupChanged, AgentChanged, AccessChanged, Notification}` (Notification unused until L3), `Event.audience` (if phase 5 hasn't), `Me.person`, routes (`/api/people`, `/api/groups`, `/api/memberships`, `/api/agents`, `/api/agents/{id}/token`, `/api/invitations`), `Settings` split into `OrgSettings`/`PersonSettings` (wire), `Chat.owner`, `Note.owner`, `access`/`deny` on `Chat`, `Note`, `Lesson`, `Topic` (`#[serde(default)]` = private to the migrated owner); service `principal.rs` with `Principal`, `require_principal` replacing `require_device` (stub: every device → the bootstrap person), `people.rs`/`groups.rs`/`agents.rs` handlers `unimplemented!`; `db.rs` tables, `doc.access.*`/`doc.deny.*` indexes, the migration entry; contract doc rows.
- **Areas**: **L1-auth** (`auth.rs`, `principal.rs`, `people.rs`, `invitations`, admission at the callback, migration of devices → owner person, `tests/auth.rs`); **L1-groups** (`groups.rs`, `agents.rs`, expansion cache, `AccessSource` trait + `manual`, ceiling math, agent tokens, `tests/groups.rs`); **L1-events** (`events.rs`, `chats.rs`, `library.rs`, `turns.rs`: audience on every emit, per-socket filter, per-principal `GET /api/library` and `/api/chats`, `TurnInput.principal`); **L1-app** (`ui/settings.rs` Organisation pane: People, Groups tree, Agents, Invitations; `ui/shell.rs` person switcher in the account row; `fake.rs`; render tests).
- **E2E**: two people, two devices: a note created by A is invisible to B's library and socket; invitation flow end to end against the fake GitHub; an agent token with a ceiling narrower than its owner; revoke cascades.

### L2 — Access enforcement in retrieval, audience UI, derived audiences, audit

- **First commit**: `retrieval::Request.principal: Principal` (required), `audience_of()` in shared, `Audit` wire type and routes, `PATCH /api/documents/{id}/access`, `Event::AuditAppended` (admins), contract.
- **Areas**: **L2-retrieve** (`retrieval.rs`, `search.rs`, `agent_tools.rs`, `context_api.rs`: both conjuncts in every statement, cache key, `EXPLAIN` tests asserting the predicate inside `KnnScan`/the FULLTEXT scan, refusal of caller-supplied `access` outside the principal); **L2-write** (`ingest.rs`, `ask.rs`, `library.rs`, `sources.rs`, `workspaces.rs`: owner + audience at write, derived audience on proposals/lessons/notes, re-stamp on publish, folder files private to the holder by default); **L2-audit** (`audit.rs` time series, `trace.principal`, retention, `GET /api/audit`, instrumentation of access/membership/agent changes); **L2-app** (Audience control on documents/notes/chats/lessons, "Publish…", the derived-audience notice, Settings → Organisation → Audit; `citations.rs` viewer respects audience; `fake.rs`).
- **E2E**: the leak tests: B searches a term only A's private doc contains → zero hits, zero highlights, count 0; a lesson from `company/engineering` + `company` is engineering-only; publish widens and the change feed reports it; agent under a ceiling cannot see its owner's private KB; audit rows exist for each; no statement over `passage` without `$paths` (a test that greps the retrieval module).

### L3 — Inbox and mentions

- **First commit**: `Notification` wire type and kinds, `Comment` on documents, `mentions` parser in shared, routes (`/api/notifications`, `PATCH …/{id}`, `/api/documents/{id}/comments`), `Event::Notification` audience, contract.
- **Areas**: **L3-service** (`notifications.rs`, `comments.rs`, delivery, dedupe, pagination, retention, mention resolution with the audience check and the "share with" refusal, agent-mention → turn under the mentioner's principal ∩ ceiling); **L3-agent** (`crates/agent`: a `Mention` turn input, the reply-as-comment sink); **L3-app** (Inbox section with badge, list and actions, mention autocomplete in every composer with the "can't see this" notice, comments on documents; `fake.rs`).
- **E2E**: A mentions B in a note B can't see → refused with the share offer; shared → B's inbox gets one notification, delivered only to B's sockets; mentioning an agent produces a comment with the intersected audience and an `agent_done` row; pagination.

### L4 — Meetings

- **First commit**: `Room`, `RoomParticipant`, `RoomRole`, `Presence` ("looking at"), routes, `Event::{RoomChanged, CallIncoming, CallCanceled}`, `OrgSettings.livekit { url, api_key_ref }` (secret in Keychain), `trait LiveKitApi` (token mint, update participant permissions, remove participant, delete room) with a fake, `trait Transcriber` with the Parakeet impl behind it, `Origin::Meeting`, contract.
- **Areas**: **L4-rooms** (`rooms.rs`: state machine, pending calls, rejoin window on the socket, stale cleanup, roles → grants, tokens via `livekit-api` or a local HS256 signer, tests on the fake); **L4-recorder** (`recorder.rs` on the Rust `livekit` crate: hidden participant, per-track files, consent gating, transcription job → document with attendee audience, summary note on `Write`, inbox rows; the native build is the risk, so this area starts with a build spike); **L4-tools** (Settings → Tools detects the LiveKit binary and shows install/config commands, like `manimgx`); **L4-app** (Meetings section, room UI: participants, mute/deafen, screen share, recording indicator and consent prompt, incoming-call sheet, `livekit-client` JS in the webview through the bridge, presence; **L4-mac** native screen capture publish, if Q11 says so).
- **E2E**: join/leave/rejoin within and past 30 s on the fake; guest cannot publish (token grant); role change pushes permissions; consent declined → no file for that participant; transcript document audience = consenting attendees; the host publishes it to the group.

### L5 — Voice

- **First commit**: `Chat.provider = Voice`, `OrgSettings.elevenlabs`, `PersonSettings.voice { ceiling, hands_free }`, routes (`/api/voice/start`, `/api/voice/webhook`, `/api/voice/tools/*`), `VoiceToken`, `trait VoiceBrain` (`Hosted`, `MarginLoop`), client-tool names, contract, the `voice` eval suite skeleton.
- **Areas**: **L5-service** (`voice.rs`: agent definition sync through the ElevenLabs API, token minting, tools handlers over the shared functions, webhook verification and transcript finalisation, traces with `route: voice`); **L5-brain** (`voice_llm.rs`: the `/v1/responses` SSE adapter over `margin_agent` for the custom-LLM option; fake ElevenLabs in tests); **L5-app** (Talk control, live transcript, client tools wired to the app-control runner, Settings → Voice; the `@elevenlabs/client` bridge; the WebSocket PCM fallback if WebRTC fails in the webview — a spike first, owner runs it by hand like SIWC); **L5-evals**.
- **E2E**: start → token → fake conversation (text mode) → a `search_context` tool call hits the service under the person's principal (a private doc of another person is not returned) → transcript finalised from the signed webhook → the chat is searchable by its owner only.

### L6 — Permission sync and the repository source

`github-org` AccessSource (teams → groups, org members may sign in), the GitHub repository source (already planned after phase 5; its documents' audience from the repo's collaborators), `google-workspace` when asked.

---

## 8. Where the direction is risky (be blunt)

1. **One Mac is not a company backend.** Keychain, Core ML, PDFKit, `sandbox-exec`, `launchd`, the Cloudflare Tunnel and a global write lock are fine for one person; for a company they mean one machine, one home network, one point of failure, and no horizontal path. The seams exist (`Embedder`, `SourceTools`, `SecretStore`), but "sell this" needs a Linux build decision (§5.2-6 of the RAG doc) before L4; LiveKit media alone forces it.
2. **Access control is only as good as the last query.** The rule is simple (two conjuncts, inside the scan) but it has to hold in *every* statement forever, including ⌘K, list routes, events, caches, traces and the app's local state. The review-lens grep and the leak e2e are the defence; a single new `list(table)` call is a breach.
3. **Events today broadcast everything.** Until L1-events, a second person on the service is a privacy incident. L1 must precede any second account, including a test account.
4. **Derived-audience intersection is lossy** (§2.4): cross-group content falls back to private, which users will experience as "Margin hides my lesson". It is the safe default; the alternative is leaks.
5. **Voice ships the company's data to ElevenLabs** (audio, transcripts, retrieved passages into their hosted LLM). The hosted path is the fast one; the custom-LLM path keeps the loop in Margin but still sends passages as tool results through ElevenLabs' transcript. Some customers will say no; keep voice optional per org and per person, and keep text first-class.
6. **WebRTC inside webviews** is the least certain technical bet here: `getDisplayMedia` is missing on iOS, the ElevenLabs JS client over WebRTC in WKWebView is unverified, and the Rust `livekit` crate drags libwebrtc into the build. Two spikes (ElevenLabs-in-webview; `livekit` crate build on the service) run before L4/L5 are briefed, like SIWC.
7. **Scope.** Accounts, access, inbox, meetings, voice and a UI redesign are each a product. With quizzes, R2 and phase 5 still queued, the honest estimate is that L1–L2 are the only phases that can start this quarter without starving the queue; L3–L5 are sequenced but should not be briefed until L2 ships.
8. **GPL hygiene.** Zed's crates are GPL-3.0-or-later; this plan uses them as a specification of behaviour only. Reviewers should check that no area's code resembles Zed's (names like `answering_connection_lost` are fine as vocabulary; copied structs or functions are not).

---

## 9. Owner questions (one line each, with a recommendation)

1. **Does Q (serious quizzes) still come before the Live-context phases?** Recommend: after L2 (quizzes need per-person attempts from L1 anyway, and company context is the pivot).
2. **First principals: GitHub-login invitations, or GitHub-org auto-admission?** Recommend: invitations by GitHub login in L1, org auto-admission in L6.
3. **Is an org admin allowed to read private KBs?** Recommend: no API path; a logged owner-only export on the Mac for legal cases.
4. **Derived content whose sources have different audiences: private-to-creator fallback (safe) or widest-common-ancestor (leaky)?** Recommend: private, with "Publish…" one click away.
5. **Agent ceiling default: the owner's full access, or private-KB excluded by default?** Recommend: full access for Margin's own agent, private-KB *excluded* by default for external and voice agents.
6. **Invite by email (needs a mail sender) or GitHub login only?** Recommend: GitHub login only until a mail provider is chosen.
7. **Push notifications (APNs/FCM) for the inbox and incoming calls in L3, or socket + badge only?** Recommend: socket + badge in L3; push with L4 (calls need it).
8. **Mentions of groups: cap at 50 recipients, or allow any size?** Recommend: cap 50 with a notice.
9. **Meeting transcription engine: local Parakeet (free, on the Mac) or ElevenLabs Scribe v2 (paid, 90+ languages)?** Recommend: Parakeet default, Scribe as a config switch behind `Transcriber`.
10. **Where LiveKit runs: on the service Mac (LAN-only media without TURN) or a small Linux VM / LiveKit Cloud?** Recommend: Linux VM with TURN on 443 for anything beyond one office LAN; decide before L4.
11. **Screen share capture: JS-in-webview only (macOS where it works; iOS cannot) or a native macOS capture area in L4?** Recommend: native macOS area in L4; iOS/Android view only until ReplayKit is its own phase.
12. **Recording consent: per-participant opt-out (unrecorded track) or host-only consent?** Recommend: per-participant opt-out, always indicated.
13. **Voice brain first: ElevenLabs-hosted LLM with Margin server tools, or Margin's loop as the custom LLM?** Recommend: hosted first, Margin-loop behind the switch, the eval decides.
14. **May the voice agent join meetings (as a LiveKit participant), or is it per-person only?** Recommend: per-person only in L5; meeting agent after the feasibility check.
15. **Is sending audio, transcripts and retrieved passages to ElevenLabs acceptable org-wide, or opt-in per person?** Recommend: org admin enables, each person opts in, retention mode shown.
16. **Linux build of the service before selling (different embedder behind `Embedder`), or the Mac stays the product?** Recommend: decide now, build with L4; the Mac stays the dev box.
17. **The UI redesign: a new mockup option reviewed before L5, or incremental changes per phase?** Recommend: incremental through L1–L4 (new sections only), one reviewed mockup before L5.
