//! L2 in the fake (`docs/l2-brief.md` D3, D7, D9): who reads what over the library,
//! folders and agents' documents; `PUT`/`GET routes::audience` with the cascade, the
//! audit row and `AudienceRevoked` to who lost access; `GET routes::AUDIT`; the change
//! feed of agents' documents and deleting one; and which events each person's socket
//! gets.
//!
//! Rows the fake made without an audience follow what they belong to (a lesson, note or
//! source its topic, a sub-topic its parent, a folder file its folder); rows without an
//! owner are the organisation owner's (Alice), as before `people-v1`.

use super::*;
use margin_shared::context::{
    AuditKind, AuditRow, AuditSubject, ContextChanges, Document, DocumentStatus, Origin,
};
use serde_json::{Value, json};

/// A row's owner: Alice when the fake made it without one.
pub fn owner_of(owner: &Option<Id>) -> Id {
    owner.clone().unwrap_or_else(|| ALICE.to_string())
}

/// What a row with its own audience says: empty `access` is private to its owner.
fn own(owner: &str, access: &[String], deny: &[String]) -> Audience {
    crate::org::audience_of(Some(owner), access, deny)
}

/// Who an audience change concerns, for the audit row: ids and paths only.
fn paths(a: &Audience) -> Value {
    json!({ "access": a.access, "deny": a.deny })
}

/// What changed, with the events to publish: to everyone who reads the rows now, and
/// `AudienceRevoked` to each person who lost access.
pub struct Published {
    pub stored: Audience,
    pub events: Vec<Event>,
    pub revoked: Vec<(Vec<Id>, Event)>,
}

/// Agents' sample documents: two of Alice's claude-code (one private, one shared with
/// Engineering) and one of Bob's triage-bot, private to Bob.
pub fn sample_documents() -> Vec<Document> {
    let doc =
        |id: &str, agent: &str, owner: &str, name: &str, access: &[&str], generation| Document {
            id: id.into(),
            project_id: "default".into(),
            origin: Origin::Agent { name: agent.into() },
            name: name.into(),
            kind: SourceKind::Text,
            status: DocumentStatus::Ready { passages: 4 },
            generation,
            bytes: 3_200,
            actor: format!("agent:{agent}"),
            access: access.iter().map(|s| s.to_string()).collect(),
            deny: Vec::new(),
            owner: Some(owner.into()),
            created_at: ts(50 + generation as i64),
            updated_at: ts(50 + generation as i64),
        };
    vec![
        doc(
            "doc-arch",
            "claude-code",
            ALICE,
            "margin-architecture.md",
            &["person/p-alice"],
            1,
        ),
        doc(
            "doc-runbook",
            "claude-code",
            ALICE,
            "deploy-runbook.md",
            &["person/p-alice", "company/engineering"],
            2,
        ),
        doc(
            "doc-triage",
            "triage-bot",
            BOB,
            "triage-notes.md",
            &["person/p-bob"],
            3,
        ),
    ]
}

/// The audit rows a fresh fake has, at `now`: Alice shared Signals & Systems with
/// Engineering, put Bob in Engineering and made claude-code's token; Bob made his
/// triage document private again.
pub fn sample_audit(now: DateTime<Utc>) -> Vec<AuditRow> {
    let row = |kind, hours: i64, actor: &str, subject: (&str, &str), before, after| AuditRow {
        kind,
        at: now - chrono::Duration::hours(hours),
        actor: actor.into(),
        on_behalf_of: None,
        subject: AuditSubject {
            kind: subject.0.into(),
            id: subject.1.into(),
        },
        before,
        after,
        device_id: None,
        subject_name: None,
    };
    vec![
        row(
            AuditKind::MembershipChange,
            5,
            "person/p-alice",
            ("person", BOB),
            json!({ "groups": ["company"] }),
            json!({ "groups": ["company", "company/engineering"] }),
        ),
        row(
            AuditKind::AudienceChange,
            4,
            "person/p-alice",
            ("topic", "signals"),
            json!({ "access": ["person/p-alice"], "deny": [] }),
            json!({ "access": ["person/p-alice", "company/engineering"], "deny": [] }),
        ),
        row(
            AuditKind::AgentToken,
            3,
            "person/p-alice",
            ("agent", "a-claude"),
            json!({}),
            json!({ "prefix": "mga_7f3a" }),
        ),
        row(
            AuditKind::AudienceChange,
            2,
            "person/p-bob",
            ("document", "doc-triage"),
            json!({ "access": ["person/p-bob", "company/engineering"], "deny": [] }),
            json!({ "access": ["person/p-bob"], "deny": [] }),
        ),
    ]
}

/// The 400 for a row whose audience can't be set by hand, saying where to set it.
fn not_a_unit(message: &str) -> Response {
    bad_request(access_code::NOT_A_UNIT, message)
}

#[allow(clippy::result_large_err)]
fn parse_unit(segment: &str) -> Result<AudienceUnit, Response> {
    AudienceUnit::parse(segment).ok_or_else(|| {
        not_a_unit(match segment {
            "lesson" | "note" => {
                "Lessons and notes follow their topic's audience; change it on the topic."
            }
            "chat" => "Chats stay private to you; there's no audience to change.",
            _ => "That can't have an audience. Change it on its topic, folder or source.",
        })
    })
}

impl FakeState {
    /// What `person_id` reads: themselves, `company`, their groups and ancestors.
    pub fn expansion_of(&self, person_id: &str) -> Vec<String> {
        match self.person(person_id) {
            Some(p) if p.state == PersonState::Active => crate::org::expansion(p),
            _ => Vec::new(),
        }
    }

    pub fn person_reads(&self, person_id: &str, audience: &Audience) -> bool {
        let paths = self.expansion_of(person_id);
        margin_shared::reads(&paths, &paths, audience)
    }

    /// A topic's owner and audience: its own, else its parent's.
    pub fn topic_audience(&self, id: &str) -> Option<(Id, Audience)> {
        let mut current = self.library.topics.iter().find(|t| t.id == id)?;
        for _ in 0..64 {
            if !current.access.is_empty() {
                break;
            }
            match current
                .parent_id
                .as_ref()
                .and_then(|p| self.library.topics.iter().find(|t| t.id == *p))
            {
                Some(parent) => current = parent,
                None => break,
            }
        }
        let owner = owner_of(&current.owner);
        Some((owner.clone(), own(&owner, &current.access, &current.deny)))
    }

    /// A row under a topic: its own audience, else its topic's.
    fn under_topic(
        &self,
        owner: &Option<Id>,
        access: &[String],
        deny: &[String],
        topic_id: &str,
    ) -> (Id, Audience) {
        if access.is_empty()
            && let Some((topic_owner, a)) = self.topic_audience(topic_id)
        {
            return (owner.clone().unwrap_or(topic_owner), a);
        }
        let owner = owner_of(owner);
        let a = own(&owner, access, deny);
        (owner, a)
    }

    pub fn lesson_audience(&self, l: &Lesson) -> (Id, Audience) {
        self.under_topic(&l.owner, &l.access, &l.deny, &l.topic_id)
    }

    pub fn note_audience(&self, n: &Note) -> (Id, Audience) {
        self.under_topic(&n.owner, &n.access, &n.deny, &n.topic_id)
    }

    pub fn workspace_audience(&self, w: &Workspace) -> (Id, Audience) {
        let owner = owner_of(&w.owner);
        let a = own(&owner, &w.access, &w.deny);
        (owner, a)
    }

    /// A source: its own audience, else its topic's or its folder's.
    pub fn source_audience(&self, s: &Source) -> (Id, Audience) {
        if s.access.is_empty() {
            if let Some(t) = &s.topic_id {
                return self.under_topic(&s.owner, &s.access, &s.deny, t);
            }
            if let Some(w) = s
                .workspace_id
                .as_ref()
                .and_then(|w| self.workspaces.iter().find(|x| x.id == *w))
            {
                return self.workspace_audience(w);
            }
        }
        let owner = owner_of(&s.owner);
        let a = own(&owner, &s.access, &s.deny);
        (owner, a)
    }

    pub fn document_audience(&self, d: &Document) -> (Id, Audience) {
        let owner = owner_of(&d.owner);
        let a = own(&owner, &d.access, &d.deny);
        (owner, a)
    }

    fn proposal_audience(&self, p: &Proposal) -> (Id, Audience) {
        let owner = owner_of(&p.owner);
        let a = own(&owner, &p.access, &p.deny);
        (owner, a)
    }

    /// Whose chat it is (chats stay private to their owner).
    fn chat_owner(&self, chat_id: &str) -> Option<Id> {
        self.chats
            .iter()
            .find(|c| c.id == chat_id)
            .map(|c| owner_of(&c.owner))
    }

    fn resolved_topic(&self, mut t: Topic) -> Topic {
        if let Some((owner, a)) = self.topic_audience(&t.id) {
            t.owner = Some(t.owner.unwrap_or(owner));
            t.access = a.access;
            t.deny = a.deny;
        }
        t
    }

    fn resolved_lesson(&self, mut l: Lesson) -> Lesson {
        let (owner, a) = self.lesson_audience(&l);
        (l.owner, l.access, l.deny) = (Some(owner), a.access, a.deny);
        l
    }

    fn resolved_note(&self, mut n: Note) -> Note {
        let (owner, a) = self.note_audience(&n);
        (n.owner, n.access, n.deny) = (Some(owner), a.access, a.deny);
        n
    }

    pub fn resolved_source(&self, mut s: Source) -> Source {
        let (owner, a) = self.source_audience(&s);
        (s.owner, s.access, s.deny) = (Some(owner), a.access, a.deny);
        s
    }

    pub fn resolved_workspace(&self, mut w: Workspace) -> Workspace {
        let (owner, a) = self.workspace_audience(&w);
        (w.owner, w.access, w.deny) = (Some(owner), a.access, a.deny);
        w
    }

    pub fn reads_workspace(&self, person_id: &str, w: &Workspace) -> bool {
        self.person_reads(person_id, &self.workspace_audience(w).1)
    }

    pub fn reads_source(&self, person_id: &str, s: &Source) -> bool {
        self.person_reads(person_id, &self.source_audience(s).1)
    }

    fn reads_lesson_id(&self, person_id: &str, lesson_id: &str) -> bool {
        self.library
            .lessons
            .iter()
            .find(|l| l.id == lesson_id)
            .is_some_and(|l| self.person_reads(person_id, &self.lesson_audience(l).1))
    }

    /// The library as `person_id` reads it, every row with its owner and audience.
    pub fn library_for(&self, person_id: &str) -> Library {
        let lib = &self.library;
        let topics = lib
            .topics
            .iter()
            .filter(|t| {
                self.topic_audience(&t.id)
                    .is_some_and(|(_, a)| self.person_reads(person_id, &a))
            })
            .map(|t| self.resolved_topic(t.clone()))
            .collect();
        let lessons: Vec<Lesson> = lib
            .lessons
            .iter()
            .filter(|l| self.person_reads(person_id, &self.lesson_audience(l).1))
            .map(|l| self.resolved_lesson(l.clone()))
            .collect();
        let notes = lib
            .notes
            .iter()
            .filter(|n| self.person_reads(person_id, &self.note_audience(n).1))
            .map(|n| self.resolved_note(n.clone()))
            .collect();
        let videos = lib
            .videos
            .iter()
            .filter(|v| lessons.iter().any(|l| l.id == v.lesson_id))
            .cloned()
            .collect();
        let sources = lib
            .sources
            .iter()
            .filter(|s| self.reads_source(person_id, s))
            .map(|s| self.resolved_source(s.clone()))
            .collect();
        Library {
            topics,
            lessons,
            notes,
            videos,
            sources,
        }
    }

    /// The event as `person_id`'s socket gets it: rows only to who reads them (with
    /// their owner and audience), chats to their owner, audit rows and traces to admins.
    pub fn visible_event(&self, person_id: &str, event: Event) -> Option<Event> {
        let reads = |a: &Audience| self.person_reads(person_id, a);
        let admin = self
            .person(person_id)
            .is_some_and(|p| p.org_role.is_admin());
        match event {
            Event::TopicChanged { topic } => {
                let topic = self.resolved_topic(topic);
                let a = own(&owner_of(&topic.owner), &topic.access, &topic.deny);
                reads(&a).then_some(Event::TopicChanged { topic })
            }
            Event::LessonChanged { lesson } => {
                let lesson = self.resolved_lesson(lesson);
                reads(&Audience {
                    access: lesson.access.clone(),
                    deny: lesson.deny.clone(),
                })
                .then_some(Event::LessonChanged { lesson })
            }
            Event::NoteChanged { note } => {
                let note = self.resolved_note(note);
                reads(&Audience {
                    access: note.access.clone(),
                    deny: note.deny.clone(),
                })
                .then_some(Event::NoteChanged { note })
            }
            Event::SourceChanged { source } => {
                let source = self.resolved_source(source);
                reads(&Audience {
                    access: source.access.clone(),
                    deny: source.deny.clone(),
                })
                .then_some(Event::SourceChanged { source })
            }
            Event::WorkspaceChanged { workspace } => {
                let workspace = self.resolved_workspace(workspace);
                reads(&self.workspace_audience(&workspace).1)
                    .then_some(Event::WorkspaceChanged { workspace })
            }
            Event::LessonDelta { ref lesson_id, .. } => {
                self.reads_lesson_id(person_id, lesson_id).then_some(event)
            }
            Event::VideoChanged { ref video } => self
                .reads_lesson_id(person_id, &video.lesson_id)
                .then_some(event),
            Event::ProposalChanged { ref proposal } => {
                reads(&self.proposal_audience(proposal).1).then_some(event)
            }
            Event::ChatChanged { ref chat } => {
                (owner_of(&chat.owner) == person_id).then_some(event)
            }
            Event::ChatItemChanged { ref item } => self
                .chat_owner(&item.chat_id)
                .is_none_or(|o| o == person_id)
                .then_some(event),
            Event::ChatItemDelta { ref chat_id, .. } => self
                .chat_owner(chat_id)
                .is_none_or(|o| o == person_id)
                .then_some(event),
            Event::DocumentChanged { ref document } => {
                reads(&self.document_audience(document).1).then_some(event)
            }
            Event::AuditAppended { row } => admin.then(|| Event::AuditAppended {
                row: self.with_subject_name(person_id, row),
            }),
            Event::Trace { .. } => admin.then_some(event),
            other => Some(other),
        }
    }

    /// Journals a change (the service's `audit::record`).
    pub fn record_audit(
        &mut self,
        kind: AuditKind,
        actor: &str,
        subject: (&str, &str),
        before: Value,
        after: Value,
        device_id: Option<Id>,
    ) -> AuditRow {
        let row = AuditRow {
            kind,
            at: Utc::now(),
            actor: actor.into(),
            on_behalf_of: None,
            subject: AuditSubject {
                kind: subject.0.into(),
                id: subject.1.into(),
            },
            before,
            after,
            device_id,
            subject_name: None,
        };
        self.audit.push(row.clone());
        row
    }

    /// The row as `admin` reads it: the subject's name only when they can read it ("a
    /// topic of @alice" otherwise).
    pub fn with_subject_name(&self, admin: &str, mut row: AuditRow) -> AuditRow {
        let id = row.subject.id.as_str();
        let handle = |owner: &str| {
            self.person(owner)
                .map(|p| format!("@{}", p.handle))
                .unwrap_or_else(|| "someone".into())
        };
        let named = |name: String, owner: &str, a: &Audience, what: &str| {
            if self.person_reads(admin, a) {
                name
            } else {
                format!("{what} of {}", handle(owner))
            }
        };
        row.subject_name = match row.subject.kind.as_str() {
            "topic" => self
                .library
                .topics
                .iter()
                .find(|t| t.id == id)
                .and_then(|t| {
                    let (owner, a) = self.topic_audience(id)?;
                    Some(named(t.title.clone(), &owner, &a, "a topic"))
                }),
            "workspace" => self.workspaces.iter().find(|w| w.id == id).map(|w| {
                let (owner, a) = self.workspace_audience(w);
                named(w.name.clone(), &owner, &a, "a folder")
            }),
            "source" => self
                .library
                .sources
                .iter()
                .chain(self.folder_sources.iter())
                .find(|s| s.id == id)
                .map(|s| {
                    let (owner, a) = self.source_audience(s);
                    named(s.name.clone(), &owner, &a, "a file")
                }),
            "document" => self.documents.iter().find(|d| d.id == id).map(|d| {
                let (owner, a) = self.document_audience(d);
                named(d.name.clone(), &owner, &a, "a document")
            }),
            "person" => self.person(id).map(|p| format!("@{}", p.handle)),
            "group" => self
                .groups
                .iter()
                .find(|g| g.path == id)
                .map(|g| g.name.clone()),
            "agent" => self
                .agents
                .iter()
                .find(|a| a.id == id)
                .map(|a| a.name.clone()),
            _ => None,
        };
        row
    }

    /// `PUT routes::audience(unit, id)` as the service does it (D3).
    #[allow(clippy::result_large_err)]
    pub fn set_audience(
        &mut self,
        caller: &Caller,
        segment: &str,
        id: &str,
        asked: Audience,
    ) -> Result<Published, Response> {
        let unit = parse_unit(segment)?;
        if let Err(message) = margin_shared::validate(&asked) {
            let code = if asked.access.len() > margin_shared::AUDIENCE_MAX
                || asked.deny.len() > margin_shared::AUDIENCE_MAX
            {
                access_code::AUDIENCE_TOO_LARGE
            } else {
                access_code::BAD_ACCESS
            };
            return Err(bad_request(code, &message));
        }
        let me = caller.person.id.clone();
        let (owner, before) = match unit {
            AudienceUnit::Topic => self.topic_audience(id).ok_or_else(not_found)?,
            AudienceUnit::Workspace => {
                let w = self
                    .workspaces
                    .iter()
                    .find(|w| w.id == id)
                    .ok_or_else(not_found)?;
                self.workspace_audience(w)
            }
            AudienceUnit::Source => {
                if let Some(s) = self.folder_sources.iter().find(|s| s.id == id) {
                    if !self.reads_source(&me, s) {
                        return Err(not_found());
                    }
                    return Err(not_a_unit(
                        "A folder's files follow the folder's audience; change it on the folder.",
                    ));
                }
                let s = self
                    .library
                    .sources
                    .iter()
                    .find(|s| s.id == id)
                    .ok_or_else(not_found)?;
                if s.topic_id.is_some() {
                    if !self.reads_source(&me, s) {
                        return Err(not_found());
                    }
                    return Err(not_a_unit(
                        "This file follows its topic's audience; change it on the topic.",
                    ));
                }
                self.source_audience(s)
            }
            AudienceUnit::Document => {
                let d = self
                    .documents
                    .iter()
                    .find(|d| d.id == id)
                    .ok_or_else(not_found)?;
                self.document_audience(d)
            }
        };
        if !self.person_reads(&me, &before) {
            return Err(not_found());
        }
        if owner != me {
            return Err(api_error(
                StatusCode::FORBIDDEN,
                access_code::NOT_OWNER,
                "Only its owner changes who can read it.",
            ));
        }
        for entry in asked.access.iter().chain(&asked.deny) {
            if is_group_path(entry) {
                if !self.groups.iter().any(|g| g.path == *entry) {
                    return Err(bad_request(
                        access_code::UNKNOWN_GROUP,
                        &format!("There's no group {entry} any more. Pick one from the list."),
                    ));
                }
            } else if let Some(pid) = entry.strip_prefix(PERSON_PREFIX) {
                if !self
                    .person(pid)
                    .is_some_and(|p| p.state == PersonState::Active)
                {
                    return Err(bad_request(
                        access_code::UNKNOWN_PERSON,
                        "That person isn't an active member of this Margin. Pick someone from the list.",
                    ));
                }
            } else if let Some(aid) = entry.strip_prefix(AGENT_PREFIX)
                && !self.agents.iter().any(|a| a.id == aid && a.owner_id == me)
            {
                return Err(bad_request(
                    access_code::UNKNOWN_AGENT,
                    "Only your own agents can be named. Pick one of yours from the list.",
                ));
            }
        }
        let stored = crate::org::stored_audience(Some(&owner), &asked);
        let (access, deny) = (stored.access.clone(), stored.deny.clone());
        let now = Utc::now();
        let mut events = Vec::new();
        match unit {
            AudienceUnit::Topic => {
                let mut subtree: Vec<Id> = vec![id.to_string()];
                loop {
                    let more: Vec<Id> = self
                        .library
                        .topics
                        .iter()
                        .filter(|t| {
                            t.parent_id.as_ref().is_some_and(|p| subtree.contains(p))
                                && !subtree.contains(&t.id)
                        })
                        .map(|t| t.id.clone())
                        .collect();
                    if more.is_empty() {
                        break;
                    }
                    subtree.extend(more);
                }
                // Narrowed lessons derive again from the topic's new audience and what
                // they cite.
                let derived: Vec<(Id, margin_shared::Derived)> = self
                    .library
                    .lessons
                    .iter()
                    .filter(|l| subtree.contains(&l.topic_id) && l.audience_narrowed)
                    .map(|l| {
                        let mut sources = vec![stored.clone()];
                        for c in &l.citations {
                            if let Some(s) = self
                                .library
                                .sources
                                .iter()
                                .chain(self.folder_sources.iter())
                                .find(|s| s.id == c.source_id)
                            {
                                sources.push(self.source_audience(s).1);
                            }
                        }
                        (
                            l.id.clone(),
                            margin_shared::derive(&person_id(&owner), &sources),
                        )
                    })
                    .collect();
                for t in self
                    .library
                    .topics
                    .iter_mut()
                    .filter(|t| subtree.contains(&t.id))
                {
                    (t.access, t.deny, t.updated_at) = (access.clone(), deny.clone(), now);
                    if t.owner.is_none() {
                        t.owner = Some(owner.clone());
                    }
                    events.push(Event::TopicChanged { topic: t.clone() });
                }
                for l in self
                    .library
                    .lessons
                    .iter_mut()
                    .filter(|l| subtree.contains(&l.topic_id))
                {
                    match derived.iter().find(|(lid, _)| *lid == l.id) {
                        Some((_, d)) => {
                            l.access = d.audience.access.clone();
                            l.deny = d.audience.deny.clone();
                            l.audience_narrowed = d.narrowed;
                        }
                        None => (l.access, l.deny) = (access.clone(), deny.clone()),
                    }
                    events.push(Event::LessonChanged { lesson: l.clone() });
                }
                for n in self
                    .library
                    .notes
                    .iter_mut()
                    .filter(|n| subtree.contains(&n.topic_id))
                {
                    (n.access, n.deny) = (access.clone(), deny.clone());
                    events.push(Event::NoteChanged { note: n.clone() });
                }
                for s in self
                    .library
                    .sources
                    .iter_mut()
                    .filter(|s| s.topic_id.as_ref().is_some_and(|t| subtree.contains(t)))
                {
                    (s.access, s.deny) = (access.clone(), deny.clone());
                    events.push(Event::SourceChanged { source: s.clone() });
                }
            }
            AudienceUnit::Workspace => {
                if let Some(w) = self.workspaces.iter_mut().find(|w| w.id == id) {
                    (w.access, w.deny) = (access.clone(), deny.clone());
                    events.push(Event::WorkspaceChanged {
                        workspace: w.clone(),
                    });
                }
                for s in self
                    .folder_sources
                    .iter_mut()
                    .filter(|s| s.workspace_id.as_deref() == Some(id))
                {
                    (s.access, s.deny) = (access.clone(), deny.clone());
                    events.push(Event::SourceChanged { source: s.clone() });
                }
            }
            AudienceUnit::Source => {
                if let Some(s) = self.library.sources.iter_mut().find(|s| s.id == id) {
                    (s.access, s.deny) = (access.clone(), deny.clone());
                    events.push(Event::SourceChanged { source: s.clone() });
                }
            }
            AudienceUnit::Document => {
                self.generation += 1;
                let generation = self.generation;
                if let Some(d) = self.documents.iter_mut().find(|d| d.id == id) {
                    (d.access, d.deny) = (access.clone(), deny.clone());
                    (d.generation, d.updated_at) = (generation, now);
                    events.push(Event::DocumentChanged {
                        document: d.clone(),
                    });
                }
            }
        }
        let row = self.record_audit(
            AuditKind::AudienceChange,
            &person_id(&me),
            (unit.as_str(), id),
            paths(&before),
            paths(&stored),
            Some(caller.device_id.clone()),
        );
        events.push(Event::AuditAppended { row });
        let lost: Vec<Id> = self
            .people
            .iter()
            .filter(|p| self.person_reads(&p.id, &before) && !self.person_reads(&p.id, &stored))
            .map(|p| p.id.clone())
            .collect();
        let revoked = if lost.is_empty() {
            Vec::new()
        } else {
            vec![(
                lost,
                Event::AudienceRevoked {
                    unit,
                    ids: vec![id.to_string()],
                },
            )]
        };
        Ok(Published {
            stored,
            events,
            revoked,
        })
    }

    /// `GET routes::audience(unit, id)`: the stored audience, for anyone who reads it; a
    /// topic's source says it follows the topic.
    #[allow(clippy::result_large_err)]
    pub fn audience_view(
        &self,
        caller: &Caller,
        segment: &str,
        id: &str,
    ) -> Result<AudienceView, Response> {
        let unit = parse_unit(segment)?;
        let me = &caller.person.id;
        let (audience, inherited_from) = match unit {
            AudienceUnit::Topic => {
                let (_, a) = self.topic_audience(id).ok_or_else(not_found)?;
                let parent = self
                    .library
                    .topics
                    .iter()
                    .find(|t| t.id == id)
                    .filter(|t| t.access.is_empty())
                    .and_then(|t| t.parent_id.clone())
                    .map(|p| (AudienceUnit::Topic, p));
                (a, parent)
            }
            AudienceUnit::Workspace => {
                let w = self
                    .workspaces
                    .iter()
                    .find(|w| w.id == id)
                    .ok_or_else(not_found)?;
                (self.workspace_audience(w).1, None)
            }
            AudienceUnit::Source => {
                let s = self
                    .library
                    .sources
                    .iter()
                    .chain(self.folder_sources.iter())
                    .find(|s| s.id == id)
                    .ok_or_else(not_found)?;
                let from = match (&s.topic_id, &s.workspace_id) {
                    (Some(t), _) => Some((AudienceUnit::Topic, t.clone())),
                    (None, Some(w)) => Some((AudienceUnit::Workspace, w.clone())),
                    _ => None,
                };
                (self.source_audience(s).1, from)
            }
            AudienceUnit::Document => {
                let d = self
                    .documents
                    .iter()
                    .find(|d| d.id == id)
                    .ok_or_else(not_found)?;
                (self.document_audience(d).1, None)
            }
        };
        if !self.person_reads(me, &audience) {
            return Err(not_found());
        }
        Ok(AudienceView {
            audience,
            inherited_from,
        })
    }
}

/// Sends events to everyone who reads them, and each revocation to who lost access.
pub fn publish_audience(fake: &Fake, published: &Published) {
    for event in &published.events {
        publish(fake, event.clone());
    }
    for (people, event) in &published.revoked {
        let _ = fake.targeted.send((people.clone(), event.clone()));
    }
}

pub async fn put_audience(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path((unit, id)): Path<(String, String)>,
    Json(asked): Json<Audience>,
) -> Response {
    let published = {
        let mut s = fake.state.lock().unwrap();
        let caller = match caller(&s, &headers) {
            Ok(c) => c,
            Err(r) => return r,
        };
        match s.set_audience(&caller, &unit, &id, asked) {
            Ok(p) => p,
            Err(r) => return r,
        }
    };
    publish_audience(&fake, &published);
    Json(published.stored).into_response()
}

pub async fn get_audience(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path((unit, id)): Path<(String, String)>,
) -> Response {
    let s = fake.state.lock().unwrap();
    let caller = match caller(&s, &headers) {
        Ok(c) => c,
        Err(r) => return r,
    };
    match s.audience_view(&caller, &unit, &id) {
        Ok(view) => Json(view).into_response(),
        Err(r) => r,
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct AuditQuery {
    kind: Option<AuditKind>,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    subject: Option<String>,
}

/// `GET routes::AUDIT` (admins): JSON lines, oldest first; `to` defaults to now and
/// `from` to a day before it.
pub async fn audit(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Query(q): Query<AuditQuery>,
) -> Response {
    let s = fake.state.lock().unwrap();
    let admin = match admin(&s, &headers) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let to = q.to.unwrap_or_else(Utc::now);
    let from = q.from.unwrap_or(to - chrono::Duration::days(1));
    if from > to {
        return bad_request("bad_range", "The start of the range is after its end.");
    }
    let mut rows: Vec<AuditRow> = s
        .audit
        .iter()
        .filter(|r| r.at >= from && r.at <= to)
        .filter(|r| q.kind.is_none_or(|k| r.kind == k))
        .filter(|r| q.subject.as_ref().is_none_or(|id| r.subject.id == *id))
        .map(|r| s.with_subject_name(&admin.person.id, r.clone()))
        .collect();
    rows.sort_by_key(|r| r.at);
    let body: String = rows
        .iter()
        .map(|r| serde_json::to_string(r).unwrap() + "\n")
        .collect();
    ([(header::CONTENT_TYPE, "application/x-ndjson")], body).into_response()
}

#[derive(Debug, serde::Deserialize)]
pub struct ChangesQuery {
    since: Option<u64>,
    limit: Option<usize>,
}

/// `GET routes::CONTEXT_CHANGES`: agents' documents the caller reads, changed or
/// deleted since `since`.
pub async fn context_changes(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Query(q): Query<ChangesQuery>,
) -> Response {
    let s = fake.state.lock().unwrap();
    let caller = match caller(&s, &headers) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let limit = q
        .limit
        .unwrap_or(margin_shared::context::CHANGES_PAGE_DEFAULT);
    if !(1..=margin_shared::context::CHANGES_PAGE_MAX).contains(&limit) {
        return bad_request(
            "bad_request",
            &format!(
                "limit is 1 to {}.",
                margin_shared::context::CHANGES_PAGE_MAX
            ),
        );
    }
    let since = q.since.unwrap_or(0);
    let me = &caller.person.id;
    let mut changed: Vec<Document> = s
        .documents
        .iter()
        .filter(|d| d.generation > since)
        .filter(|d| s.person_reads(me, &s.document_audience(d).1))
        .cloned()
        .collect();
    changed.sort_by_key(|d| d.generation);
    let more = changed.len() > limit;
    changed.truncate(limit);
    let generation = if more {
        changed.last().map_or(since, |d| d.generation)
    } else {
        s.generation
    };
    let deleted = s
        .doc_tombstones
        .iter()
        .filter(|(_, generation, a)| *generation > since && s.person_reads(me, a))
        .map(|(id, _, _)| id.clone())
        .collect();
    Json(ContextChanges {
        generation,
        changed,
        deleted,
        more,
    })
    .into_response()
}

/// `DELETE routes::context_document(id)`: an agent's document, by its owner.
pub async fn delete_document(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let mut s = fake.state.lock().unwrap();
    let caller = match caller(&s, &headers) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let Some(i) = s.documents.iter().position(|d| d.id == id) else {
        return not_found();
    };
    let (owner, a) = s.document_audience(&s.documents[i]);
    if !s.person_reads(&caller.person.id, &a) {
        return not_found();
    }
    if owner != caller.person.id {
        return api_error(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Only its owner deletes it.",
        );
    }
    s.documents.remove(i);
    s.generation += 1;
    let generation = s.generation;
    s.doc_tombstones.push((id, generation, a));
    StatusCode::NO_CONTENT.into_response()
}

impl FakeState {
    /// Journals a change `caller` made (the L1 routes' `audit::record` calls) and
    /// answers the event that tells admins.
    pub fn journal(
        &mut self,
        caller: &Caller,
        kind: AuditKind,
        subject: (&str, &str),
        before: Value,
        after: Value,
    ) -> Event {
        let row = self.record_audit(
            kind,
            &person_id(&caller.person.id),
            subject,
            before,
            after,
            Some(caller.device_id.clone()),
        );
        Event::AuditAppended { row }
    }
}
