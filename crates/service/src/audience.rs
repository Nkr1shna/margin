//! Audiences (roadmap L2, `docs/l2-brief.md` D2, D3, D4): what stamps a document, the
//! `passages-access-v1` migration, publishing (`PUT routes::audience(unit, id)`) with its
//! cascade down to passages, and the audience of a lesson derived from what it drew on.
//!
//! The units of audience are a topic (with its sub-topics, lessons, notes, videos and
//! sources), an attached folder (with its files), a source without a topic, and an
//! agent's document. Everything else follows its unit: a lesson or note its topic (a
//! lesson may be narrower, by derivation, never wider), a file its folder; a chat stays
//! private to its owner. A document carries the audience of the row that owns it
//! (`of_document`), and its passages carry the document's.

use std::collections::HashSet;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::Utc;
use margin_shared::context::{Document, LibraryKind, Origin};
use margin_shared::{
    AGENT_PREFIX, Audience, AudienceUnit, AudienceView, Chat, Derived, Event, Id, Note,
    PERSON_PREFIX, PersonState, Source, Topic, Workspace, access_code, derive, is_group_path,
    person_id,
};

use crate::db;
use crate::error::{ApiJson, AppError, AppResult};
use crate::ingest;
use crate::library;
use crate::pipeline::Reader;
use crate::principal::{BOOTSTRAP_PERSON_ID, CurrentPrincipal, Principal};
use crate::retrieval::{document_access, reads_document};
use crate::state::{
    AppState, Audience as To, Emitted, Owned, Ownership, StoredLesson, StoredNote, check_owner,
    not_owner, readable,
};

/// Recorded in `SettingsRow.migrations` once every document and its passages carry the
/// audience of the row that owns it.
pub const ACCESS_MIGRATION: &str = "passages-access-v1";

/// What `create_note` (and the other note-writing tools) answer when the chat quoted
/// passages some of the note's readers can't read (D4.3).
pub const NOTE_REFUSAL: &str =
    "The note would quote documents the topic's readers can't see; file it in a private topic.";

/// A row's audience as the wire body: its effective access and its denials.
pub fn audience_of(row: &(impl Owned + ?Sized)) -> Audience {
    Audience {
        access: row.audience(),
        deny: row.deny().to_vec(),
    }
}

/// A document's audience as stored (read empty, private to its owner).
pub fn document_audience(doc: &Document) -> Audience {
    Audience {
        access: document_access(doc),
        deny: doc.deny.clone(),
    }
}

/// What a document stamps itself with when no row owns it (an agent's document, or one
/// whose owning row is gone): what it has.
fn own_stamp(doc: &Document) -> Ownership {
    Ownership {
        owner: doc
            .owner
            .clone()
            .unwrap_or_else(|| BOOTSTRAP_PERSON_ID.to_string()),
        access: document_access(doc),
        deny: doc.deny.clone(),
    }
}

/// The row a document indexes, as its ownership: a lesson's, note's or chat's; a
/// source's or folder file's source row; `None` for an agent's document or a row that
/// is gone.
async fn owning_row(state: &AppState, doc: &Document) -> anyhow::Result<Option<Ownership>> {
    Ok(match &doc.origin {
        Origin::Library { kind, id } => match kind {
            LibraryKind::Lesson => state
                .store
                .get::<StoredLesson>(db::LESSON, id)
                .await?
                .map(|r| Ownership::of(&r)),
            LibraryKind::Note => state
                .store
                .get::<Note>(db::NOTE, id)
                .await?
                .map(|r| Ownership::of(&r)),
            LibraryKind::Chat => state
                .store
                .get::<Chat>(db::CHAT, id)
                .await?
                .map(|r| Ownership::of(&r)),
        },
        Origin::Source { source_id } => state
            .store
            .get::<Source>(db::SOURCE, source_id)
            .await?
            .map(|s| Ownership::of(&s)),
        // A folder file's document has its source's id.
        Origin::FolderFile { .. } => state
            .store
            .get::<Source>(db::SOURCE, &doc.id)
            .await?
            .map(|s| Ownership::of(&s)),
        Origin::Agent { .. } => None,
    })
}

/// The audience a document takes from the row that owns it: a lesson's, note's or
/// chat's; a source's; a folder file's source row (the folder's); an agent document's
/// own (`[person/<owner>]` unless ingested or published with one). A document whose
/// owning row is gone keeps what it has.
pub async fn of_document(state: &AppState, doc: &Document) -> anyhow::Result<Ownership> {
    Ok(owning_row(state, doc)
        .await?
        .unwrap_or_else(|| own_stamp(doc)))
}

/// Stamps `doc`, its row and every passage of it, with `own`'s audience when that isn't
/// what it has (the generation moves, so caches and the change feed see it). Call with
/// the write lock held. Whether anything changed.
pub async fn stamp_locked(
    state: &AppState,
    doc: &mut Document,
    own: &Ownership,
) -> anyhow::Result<bool> {
    if document_access(doc) == own.access && doc.deny == own.deny && !doc.access.is_empty() {
        return Ok(false);
    }
    force_stamp_locked(state, doc, own).await?;
    Ok(true)
}

/// `stamp_locked` without the comparison: the passages are re-stamped whatever the
/// row says (the migration, where rows and passages may disagree).
async fn force_stamp_locked(
    state: &AppState,
    doc: &mut Document,
    own: &Ownership,
) -> anyhow::Result<()> {
    if let Some(generation) = state
        .store
        .set_document_audience(&doc.id, &own.access, &own.deny)
        .await?
    {
        doc.generation = generation;
    }
    doc.access = own.access.clone();
    doc.deny = own.deny.clone();
    state.retrieval_cache.invalidate(&doc.id);
    Ok(())
}

/// `of_document`, then `stamp_locked`: a document re-stamped from the row that owns it.
/// Call with the write lock held.
pub async fn restamp_locked(state: &AppState, doc: &mut Document) -> anyhow::Result<bool> {
    let own = of_document(state, doc).await?;
    stamp_locked(state, doc, &own).await
}

/// Re-stamps the document of `id` (a source's, a folder file's, an agent's) or of a
/// library item from its owning row, when it has one; the `DocumentChanged` to send
/// when it changed. Call with the write lock held.
async fn restamp_doc_locked(
    state: &AppState,
    doc: Option<Document>,
) -> anyhow::Result<Option<Emitted>> {
    let Some(mut doc) = doc else {
        return Ok(None);
    };
    Ok(restamp_locked(state, &mut doc)
        .await?
        .then(|| document_changed(doc)))
}

/// The document of a library item, if it was indexed.
async fn library_document(
    state: &AppState,
    kind: LibraryKind,
    id: &str,
) -> anyhow::Result<Option<Document>> {
    ingest::find_by_origin(
        state,
        &Origin::Library {
            kind,
            id: id.to_string(),
        },
        "",
        None,
    )
    .await
}

/// Re-stamps a library item's document from the item (a lesson whose audience was
/// derived again, a note that follows its topic). Call with the write lock held.
pub async fn restamp_library_locked(
    state: &AppState,
    kind: LibraryKind,
    id: &str,
) -> anyhow::Result<Option<Emitted>> {
    let doc = library_document(state, kind, id).await?;
    restamp_doc_locked(state, doc).await
}

fn document_changed(doc: Document) -> Emitted {
    Emitted {
        audience: ingest::document_audience(&doc),
        event: Event::DocumentChanged { document: doc },
    }
}

// ---------------------------------------------------------------------------
// Derived audiences (D4)
// ---------------------------------------------------------------------------

/// The audiences of the documents `ids` as they are now; documents that are gone are
/// skipped (nobody reads them any more).
async fn documents_audiences(state: &AppState, ids: &[Id]) -> anyhow::Result<Vec<Audience>> {
    let mut out = Vec::new();
    for id in ids {
        if let Some(doc) = ingest::document(state, id).await? {
            out.push(document_audience(&doc));
        }
    }
    Ok(out)
}

/// The audience of something `creator` writes under a row with audience `under` from
/// the documents `drew_on` (a lesson from its topic and its packed passages; a
/// proposal's course from the parent topic and the survey): `derive` over them.
/// `narrowed` only when the documents made it private: under a row that is already
/// private to its creator nothing was narrowed.
pub async fn derive_under(
    state: &AppState,
    creator: &str,
    under: &Audience,
    drew_on: &[Id],
) -> anyhow::Result<Derived> {
    let creator = person_id(creator);
    let mut sources = vec![under.clone()];
    sources.extend(documents_audiences(state, drew_on).await?);
    let derived = derive(&creator, &sources);
    let alone = derive(&creator, std::slice::from_ref(under));
    Ok(Derived {
        narrowed: derived.narrowed && !alone.narrowed,
        audience: derived.audience,
    })
}

/// The lesson's audience from its topic's (`topic`) and what it drew on (its own
/// document excluded): the lesson is never wider than its topic.
pub async fn derive_lesson(
    state: &AppState,
    row: &StoredLesson,
    topic: &Audience,
) -> anyhow::Result<Derived> {
    derive_under(state, row.lesson.owner_id(), topic, &row.drew_on).await
}

/// The documents `hits` come from, once each, in order, without `except` (a lesson's
/// own document: what it drew on is what it was written from).
pub fn documents_of<'a>(
    hits: impl IntoIterator<Item = &'a crate::pipeline::Hit>,
    except: Option<&str>,
) -> Vec<Id> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for h in hits {
        let id = &h.passage.document_id;
        if Some(id.as_str()) != except && seen.insert(id.clone()) {
            out.push(id.clone());
        }
    }
    out
}

/// The id of a lesson's own document, if it was indexed.
pub async fn lesson_document_id(state: &AppState, lesson_id: &str) -> anyhow::Result<Option<Id>> {
    Ok(library_document(state, LibraryKind::Lesson, lesson_id)
        .await?
        .map(|d| d.id))
}

/// Sets a lesson's audience to `d` (and `audience_narrowed`), stores it and re-stamps
/// its document. Call with the write lock held.
pub async fn store_lesson_locked(
    state: &AppState,
    row: &mut StoredLesson,
    d: Derived,
) -> anyhow::Result<()> {
    row.lesson.access = d.audience.access;
    row.lesson.deny = d.audience.deny;
    row.lesson.audience_narrowed = d.narrowed;
    state.store.put(db::LESSON, &row.lesson.id, &*row).await?;
    restamp_library_locked(state, LibraryKind::Lesson, &row.lesson.id).await?;
    Ok(())
}

/// What a lesson whose audience was `before` and is now `row`'s sends: `LessonChanged`
/// (and its `VideoChanged`, its document's `DocumentChanged`) to its readers, and
/// `LessonDeleted` to whoever read it before and doesn't now.
pub async fn lesson_events(
    state: &AppState,
    row: &StoredLesson,
    before: Audience,
) -> anyhow::Result<Vec<Emitted>> {
    let after = audience_of(row);
    let mut events = vec![Emitted::row(
        &row.lesson,
        Event::LessonChanged {
            lesson: state.public_lesson(&row.lesson),
        },
    )];
    if let Some(video) = library::video_for_lesson(state, &row.lesson.id).await? {
        events.push(Emitted::row(
            &row.lesson,
            Event::VideoChanged {
                video: state.public_video(&video),
            },
        ));
    }
    if let Some(doc) = library_document(state, LibraryKind::Lesson, &row.lesson.id).await? {
        events.push(document_changed(doc));
    }
    if before != after {
        events.push(Emitted {
            audience: To::Lost { before, after },
            event: Event::LessonDeleted {
                id: row.lesson.id.clone(),
            },
        });
    }
    Ok(events)
}

/// `store_lesson_locked`, then `lesson_events`. Call with the write lock held.
pub async fn apply_lesson_locked(
    state: &AppState,
    row: &mut StoredLesson,
    d: Derived,
) -> anyhow::Result<Vec<Emitted>> {
    let before = audience_of(row);
    store_lesson_locked(state, row, d).await?;
    lesson_events(state, row, before).await
}

/// Why a note-writing tool of the agent must not write: the chat's Passages cards quote
/// documents some reader of the note's topic can't read (D4.3). `tool` and `args` are
/// the call's; `None` for any other tool, a topic or note the chat's owner can't write,
/// or a chat that quoted nothing narrower.
pub async fn note_refusal(
    state: &AppState,
    chat_id: &str,
    tool: &str,
    args: &serde_json::Value,
) -> anyhow::Result<Option<String>> {
    let field = |name: &str| args.get(name).and_then(|v| v.as_str()).map(str::to_string);
    let target: Option<(String, Audience)> = match tool {
        "create_note" => match field("topic_id") {
            Some(id) => state
                .store
                .get::<Topic>(db::TOPIC, &id)
                .await?
                .map(|t| (t.owner_id().to_string(), audience_of(&t))),
            None => None,
        },
        "append_to_note" | "replace_note_body" => match field("note_id") {
            Some(id) => state
                .store
                .get::<StoredNote>(db::NOTE, &id)
                .await?
                .map(|n| (n.owner_id().to_string(), audience_of(&n))),
            None => None,
        },
        _ => None,
    };
    let Some((owner, under)) = target else {
        return Ok(None);
    };
    let quoted = crate::chats::quoted_documents(state, chat_id).await?;
    if quoted.is_empty() {
        return Ok(None);
    }
    let creator = person_id(&owner);
    let with = derive_under(state, &owner, &under, &quoted).await?;
    let alone = derive(&creator, std::slice::from_ref(&under));
    let same = |a: &[String], b: &[String]| {
        a.iter().collect::<HashSet<_>>() == b.iter().collect::<HashSet<_>>()
    };
    Ok((!same(&with.audience.access, &alone.audience.access)
        || !same(&with.audience.deny, &alone.audience.deny))
    .then(|| NOTE_REFUSAL.to_string()))
}

// ---------------------------------------------------------------------------
// Setting an audience (D3)
// ---------------------------------------------------------------------------

/// The shape of an audience (`margin_shared::validate`): 400 `audience_too_large` past
/// `AUDIENCE_MAX`, else 400 `bad_access`, with the sentence to show.
pub fn check_shape(a: &Audience) -> AppResult<()> {
    margin_shared::validate(a).map_err(|message| {
        let code = if a.access.len() > margin_shared::AUDIENCE_MAX
            || a.deny.len() > margin_shared::AUDIENCE_MAX
        {
            access_code::AUDIENCE_TOO_LARGE
        } else {
            access_code::BAD_ACCESS
        };
        AppError::new(StatusCode::BAD_REQUEST, code, message)
    })
}

/// Every entry names something that exists: a group (400 `unknown_group`), an active
/// person (400 `unknown_person`), one of `p`'s own agents (400 `unknown_agent`).
pub async fn check_names(state: &AppState, p: &Principal, a: &Audience) -> AppResult<()> {
    for entry in a.access.iter().chain(&a.deny) {
        if is_group_path(entry) {
            if !crate::groups::group_exists(&state.store, entry).await? {
                return Err(AppError::new(
                    StatusCode::BAD_REQUEST,
                    access_code::UNKNOWN_GROUP,
                    format!("There's no group “{entry}”. Pick one from the list."),
                ));
            }
        } else if let Some(id) = entry.strip_prefix(PERSON_PREFIX) {
            let active = crate::people::person_row(state, id)
                .await?
                .is_some_and(|r| r.person.state == PersonState::Active);
            if !active {
                return Err(AppError::new(
                    StatusCode::BAD_REQUEST,
                    access_code::UNKNOWN_PERSON,
                    format!(
                        "“{entry}” isn't an active person here. Pick someone from the directory."
                    ),
                ));
            }
        } else if let Some(id) = entry.strip_prefix(AGENT_PREFIX) {
            let mine = state
                .store
                .get::<crate::agents::AgentRow>(db::AGENT, id)
                .await?
                .is_some_and(|r| r.agent.owner_id == p.person_id);
            if !mine {
                return Err(AppError::new(
                    StatusCode::BAD_REQUEST,
                    access_code::UNKNOWN_AGENT,
                    format!("“{entry}” isn't one of your agents. Pick one from My agents."),
                ));
            }
        }
    }
    Ok(())
}

/// What is stored for an audience `a` set by `owner`: `access = [person/<owner>] ∪
/// a.access` (the owner is never locked out), `deny = a.deny − {person/<owner>}`. A
/// denial naming a group the owner is in would lock them out: 400 `bad_access`.
pub fn stored(owner: &Principal, a: &Audience) -> AppResult<Audience> {
    let me = person_id(&owner.person_id);
    let mut access = vec![me.clone()];
    access.extend(a.access.iter().filter(|x| **x != me).cloned());
    let mut deny = Vec::new();
    for d in a.deny.iter().filter(|d| **d != me) {
        if owner.deny_ids.contains(d) {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                access_code::BAD_ACCESS,
                format!(
                    "You're in “{d}”, so denying it would lock you out too. Deny the people or groups inside it instead."
                ),
            ));
        }
        deny.push(d.clone());
    }
    Ok(Audience { access, deny })
}

/// What one `set` changed, collected under the write lock and sent after it.
struct Change {
    before: Audience,
    after: Audience,
    events: Vec<Emitted>,
    /// The units whose readers who lost access hear `AudienceRevoked`, each with the
    /// audience it had.
    revoked: Vec<(AudienceUnit, Id, Audience)>,
}

/// Sets the audience of a unit `p` owns (admins are not exempt): validates it, stores
/// `access = [person/<owner>] ∪ a.access` and `deny = a.deny − {person/<owner>}`,
/// cascades to everything under the unit and its documents' passages under the write
/// lock, journals an `AuditKind::AudienceChange` row (`audit::record`), then emits the
/// rows' `*Changed`, `DocumentChanged` to the new audience and `AudienceRevoked` to who
/// lost access. Answers the stored audience.
pub async fn set(
    state: &AppState,
    p: &Principal,
    unit: AudienceUnit,
    id: &str,
    a: Audience,
) -> AppResult<Audience> {
    check_shape(&a)?;
    check_names(state, p, &a).await?;
    let change = {
        let _w = state.write_lock.lock().await;
        let change = match unit {
            AudienceUnit::Topic => set_topic_locked(state, p, id, &a).await?,
            AudienceUnit::Workspace => set_workspace_locked(state, p, id, &a).await?,
            AudienceUnit::Source => set_source_locked(state, p, id, &a).await?,
            AudienceUnit::Document => set_document_locked(state, p, id, &a).await?,
        };
        crate::audit::record(
            state,
            crate::audit::audience_change(p, unit, id, &change.before, &change.after),
        );
        change
    };
    let Change {
        after,
        events,
        revoked,
        ..
    } = change;
    state.emit_all(events);
    for (unit, id, before) in revoked {
        if before != after {
            state.emit_to(
                To::Lost {
                    before,
                    after: after.clone(),
                },
                Event::AudienceRevoked {
                    unit,
                    ids: vec![id],
                },
            );
        }
    }
    Ok(after)
}

async fn set_topic_locked(
    state: &AppState,
    p: &Principal,
    id: &str,
    a: &Audience,
) -> AppResult<Change> {
    let topic: Topic = readable(p, state.store.get(db::TOPIC, id).await?, "topic")?;
    check_owner(p, &topic)?;
    let after = stored(p, a)?;
    let before = audience_of(&topic);
    let mut events = Vec::new();
    let mut revoked = Vec::new();
    let all: Vec<Topic> = state.store.list(db::TOPIC).await?;
    let ids = library::subtree(&all, id);
    let now = Utc::now();
    for mut t in all.into_iter().filter(|t| ids.contains(&t.id)) {
        revoked.push((AudienceUnit::Topic, t.id.clone(), audience_of(&t)));
        t.access = after.access.clone();
        t.deny = after.deny.clone();
        t.updated_at = now;
        state.store.put(db::TOPIC, &t.id, &t).await?;
        events.push(Emitted::row(&t, Event::TopicChanged { topic: t.clone() }));
    }
    // Notes and sources take the topic's audience exactly.
    for tid in &ids {
        let notes: Vec<StoredNote> = state.store.list_where(db::NOTE, "topic_id", tid).await?;
        for mut n in notes {
            n.note.access = after.access.clone();
            n.note.deny = after.deny.clone();
            state.store.put(db::NOTE, &n.note.id, &n).await?;
            events.push(Emitted::row(
                &n.note,
                Event::NoteChanged {
                    note: n.note.clone(),
                },
            ));
            events.extend(restamp_library_locked(state, LibraryKind::Note, &n.note.id).await?);
        }
        let sources: Vec<Source> = state.store.list_where(db::SOURCE, "topic_id", tid).await?;
        for mut s in sources {
            s.access = after.access.clone();
            s.deny = after.deny.clone();
            state.store.put(db::SOURCE, &s.id, &s).await?;
            events.push(Emitted::row(
                &s,
                Event::SourceChanged {
                    source: state.public_source(&s),
                },
            ));
            let doc = ingest::document(state, &s.id).await?;
            events.extend(restamp_doc_locked(state, doc).await?);
        }
    }
    // Lessons derive again from the topic's new audience and what they drew on (D4), so
    // publishing can un-narrow a lesson whose sources were the topic's own and never
    // widens one past them. A lesson that drew on a sibling lesson follows it: passes
    // until nothing moves (bounded by the number of lessons).
    let mut lessons = Vec::new();
    for tid in &ids {
        lessons.extend(library::topic_lessons(state, tid).await?);
    }
    let originals: Vec<Audience> = lessons.iter().map(audience_of).collect();
    for pass in 0..=lessons.len() {
        let mut moved = false;
        for row in lessons.iter_mut() {
            let d = derive_lesson(state, row, &after).await?;
            let unchanged = pass > 0
                && d.audience.access == row.lesson.access
                && d.audience.deny == row.lesson.deny
                && d.narrowed == row.lesson.audience_narrowed;
            if !unchanged {
                moved = true;
                store_lesson_locked(state, row, d).await?;
            }
        }
        if !moved {
            break;
        }
    }
    for (row, before) in lessons.iter().zip(originals) {
        events.extend(lesson_events(state, row, before).await?);
    }
    Ok(Change {
        before,
        after,
        events,
        revoked,
    })
}

async fn set_workspace_locked(
    state: &AppState,
    p: &Principal,
    id: &str,
    a: &Audience,
) -> AppResult<Change> {
    let mut ws: Workspace = readable(p, state.store.get(db::WORKSPACE, id).await?, "folder")?;
    check_owner(p, &ws)?;
    let after = stored(p, a)?;
    let before = audience_of(&ws);
    ws.access = after.access.clone();
    ws.deny = after.deny.clone();
    state.store.put(db::WORKSPACE, &ws.id, &ws).await?;
    let mut events = vec![Emitted::row(
        &ws,
        Event::WorkspaceChanged {
            workspace: ws.clone(),
        },
    )];
    let files: Vec<Source> = state
        .store
        .list_where(db::SOURCE, "workspace_id", id)
        .await?;
    for mut s in files {
        s.access = after.access.clone();
        s.deny = after.deny.clone();
        state.store.put(db::SOURCE, &s.id, &s).await?;
        events.push(Emitted::row(
            &s,
            Event::SourceChanged {
                source: state.public_source(&s),
            },
        ));
        let doc = ingest::document(state, &s.id).await?;
        events.extend(restamp_doc_locked(state, doc).await?);
    }
    Ok(Change {
        revoked: vec![(AudienceUnit::Workspace, id.to_string(), before.clone())],
        before,
        after,
        events,
    })
}

fn follows(message: &str) -> AppError {
    AppError::new(StatusCode::BAD_REQUEST, access_code::NOT_A_UNIT, message)
}

async fn set_source_locked(
    state: &AppState,
    p: &Principal,
    id: &str,
    a: &Audience,
) -> AppResult<Change> {
    let mut s: Source = readable(p, state.store.get(db::SOURCE, id).await?, "source")?;
    check_owner(p, &s)?;
    if s.topic_id.is_some() {
        return Err(follows(
            "This source follows its topic's audience; change it on the topic.",
        ));
    }
    if s.workspace_id.is_some() {
        return Err(follows(
            "This file follows its folder's audience; change it on the folder.",
        ));
    }
    let after = stored(p, a)?;
    let before = audience_of(&s);
    s.access = after.access.clone();
    s.deny = after.deny.clone();
    state.store.put(db::SOURCE, &s.id, &s).await?;
    let mut events = vec![Emitted::row(
        &s,
        Event::SourceChanged {
            source: state.public_source(&s),
        },
    )];
    let doc = ingest::document(state, id).await?;
    events.extend(restamp_doc_locked(state, doc).await?);
    Ok(Change {
        revoked: vec![(AudienceUnit::Source, id.to_string(), before.clone())],
        before,
        after,
        events,
    })
}

async fn set_document_locked(
    state: &AppState,
    p: &Principal,
    id: &str,
    a: &Audience,
) -> AppResult<Change> {
    let Some(mut doc) = ingest::document(state, id)
        .await?
        .filter(|d| reads_document(&Reader::of(p), d))
    else {
        return Err(AppError::not_found("document"));
    };
    if !matches!(doc.origin, Origin::Agent { .. }) {
        return Err(follows(match doc.origin {
            Origin::Source { .. } => "Change this document's audience on its source.",
            Origin::FolderFile { .. } => {
                "This file follows its folder's audience; change it on the folder."
            }
            _ => {
                "This document follows its lesson's, note's or chat's audience; change it on the topic."
            }
        }));
    }
    let owner = doc.owner.as_deref().unwrap_or(BOOTSTRAP_PERSON_ID);
    if owner != p.person_id {
        return Err(not_owner());
    }
    let after = stored(p, a)?;
    let before = document_audience(&doc);
    let own = Ownership {
        owner: owner.to_string(),
        access: after.access.clone(),
        deny: after.deny.clone(),
    };
    force_stamp_locked(state, &mut doc, &own).await?;
    Ok(Change {
        revoked: vec![(AudienceUnit::Document, id.to_string(), before.clone())],
        before,
        after,
        events: vec![document_changed(doc)],
    })
}

/// The stored audience of a unit `p` can read, and where it follows another from.
pub async fn view(
    state: &AppState,
    p: &Principal,
    unit: AudienceUnit,
    id: &str,
) -> AppResult<AudienceView> {
    Ok(match unit {
        AudienceUnit::Topic => {
            let t: Topic = readable(p, state.store.get(db::TOPIC, id).await?, "topic")?;
            AudienceView {
                audience: audience_of(&t),
                inherited_from: None,
            }
        }
        AudienceUnit::Workspace => {
            let ws: Workspace = readable(p, state.store.get(db::WORKSPACE, id).await?, "folder")?;
            AudienceView {
                audience: audience_of(&ws),
                inherited_from: None,
            }
        }
        AudienceUnit::Source => {
            let s: Source = readable(p, state.store.get(db::SOURCE, id).await?, "source")?;
            let inherited_from = match (&s.topic_id, &s.workspace_id) {
                (Some(t), _) => Some((AudienceUnit::Topic, t.clone())),
                (None, Some(w)) => Some((AudienceUnit::Workspace, w.clone())),
                (None, None) => None,
            };
            AudienceView {
                audience: audience_of(&s),
                inherited_from,
            }
        }
        AudienceUnit::Document => {
            let Some(doc) = ingest::document(state, id)
                .await?
                .filter(|d| reads_document(&Reader::of(p), d))
            else {
                return Err(AppError::not_found("document"));
            };
            let inherited_from = match &doc.origin {
                Origin::Agent { .. } => None,
                Origin::Source { source_id } => Some((AudienceUnit::Source, source_id.clone())),
                Origin::FolderFile { workspace_id, .. } => {
                    Some((AudienceUnit::Workspace, workspace_id.clone()))
                }
                Origin::Library { kind, id } => library_topic(state, *kind, id)
                    .await?
                    .map(|t| (AudienceUnit::Topic, t)),
            };
            AudienceView {
                audience: document_audience(&doc),
                inherited_from,
            }
        }
    })
}

/// The topic a lesson or note is filed under; `None` for a chat or a row that is gone.
async fn library_topic(
    state: &AppState,
    kind: LibraryKind,
    id: &str,
) -> anyhow::Result<Option<Id>> {
    Ok(match kind {
        LibraryKind::Lesson => state
            .store
            .get::<StoredLesson>(db::LESSON, id)
            .await?
            .map(|l| l.lesson.topic_id),
        LibraryKind::Note => state
            .store
            .get::<Note>(db::NOTE, id)
            .await?
            .map(|n| n.topic_id),
        LibraryKind::Chat => None,
    })
}

// ---------------------------------------------------------------------------
// The migration (D2)
// ---------------------------------------------------------------------------

/// What the migration logs when the dogfood setup goes blind: one person, and agents
/// whose ceiling leaves out their owner's private knowledge base (owner Q4).
pub const BLIND_AGENTS: &str = "passages-access-v1: everything in the index is now private \
     to its owner unless published; agents whose ceiling leaves out “My private notes and \
     chats” see nothing until you widen it in Organisation → Agents or publish a topic";

/// `passages-access-v1`: re-stamps every document and its passages from the row that
/// owns it (`of_document`, `Store::set_document_audience`), after `people-v1`, under
/// the write lock, once. An agent's document from before L2, or one whose owning row is
/// gone, becomes private to its owner: nothing says `company` unless it was published.
pub async fn migrate(state: &AppState) -> anyhow::Result<()> {
    let _w = state.write_lock.lock().await;
    let mut settings = state.settings_row().await?;
    if settings.migrations.iter().any(|m| m == ACCESS_MIGRATION) {
        return Ok(());
    }
    let docs: Vec<Document> = state.store.list(db::DOCUMENT).await?;
    let (mut owned, mut private) = (0usize, 0usize);
    for mut doc in docs {
        let own = match owning_row(state, &doc).await? {
            Some(own) => {
                owned += 1;
                own
            }
            None => {
                private += 1;
                Ownership::private_to(doc.owner.as_deref().unwrap_or(BOOTSTRAP_PERSON_ID))
            }
        };
        force_stamp_locked(state, &mut doc, &own).await?;
    }
    settings.migrations.push(ACCESS_MIGRATION.to_string());
    state
        .store
        .put(db::SETTINGS, crate::state::SETTINGS_ID, &settings)
        .await?;
    tracing::info!(
        "{ACCESS_MIGRATION}: {owned} document(s) took their row's audience, {private} agent or orphaned document(s) became private to their owner"
    );
    if blind_agents(state).await? {
        tracing::warn!("{BLIND_AGENTS}");
    }
    Ok(())
}

/// Whether the organisation has one active person and at least one agent whose ceiling
/// leaves out its owner's private knowledge base.
async fn blind_agents(state: &AppState) -> anyhow::Result<bool> {
    let people: Vec<crate::people::PersonRow> = state.store.list(db::PERSON).await?;
    let active = people
        .iter()
        .filter(|r| r.person.state == PersonState::Active)
        .count();
    if active != 1 {
        return Ok(false);
    }
    let agents: Vec<crate::agents::AgentRow> = state.store.list(db::AGENT).await?;
    Ok(agents
        .iter()
        .any(|r| !r.agent.ceiling.contains(&person_id(&r.agent.owner_id))))
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// The unit a path segment names, else 400 `not_a_unit` saying where to change it.
pub fn unit(segment: &str) -> AppResult<AudienceUnit> {
    AudienceUnit::parse(segment).ok_or_else(|| {
        let message = match segment {
            "lesson" | "note" => {
                "Lessons and notes follow their topic's audience; change it on the topic."
            }
            "chat" => "Chats stay private to you; there's no audience to change.",
            _ => "That can't have an audience. Change it on its topic, folder or source.",
        };
        AppError::new(StatusCode::BAD_REQUEST, access_code::NOT_A_UNIT, message)
    })
}

/// `PUT routes::audience(unit, id)`.
pub async fn put_audience(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    Path((segment, id)): Path<(String, String)>,
    ApiJson(a): ApiJson<Audience>,
) -> AppResult<Json<Audience>> {
    let unit = unit(&segment)?;
    Ok(Json(set(&state, &p, unit, &id, a).await?))
}

/// `GET routes::audience(unit, id)`.
pub async fn get_audience(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    Path((segment, id)): Path<(String, String)>,
) -> AppResult<Json<AudienceView>> {
    let unit = unit(&segment)?;
    Ok(Json(view(&state, &p, unit, &id).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use margin_shared::OrgRole;

    #[test]
    fn only_units_have_an_audience() {
        assert_eq!(unit("topic").unwrap(), AudienceUnit::Topic);
        for segment in ["lesson", "note", "chat", "x"] {
            let e = unit(segment).unwrap_err();
            assert_eq!(e.code, access_code::NOT_A_UNIT);
            assert_eq!(e.status, StatusCode::BAD_REQUEST);
        }
        assert!(unit("lesson").unwrap_err().message.contains("topic"));
    }

    fn a(access: &[&str], deny: &[&str]) -> Audience {
        Audience {
            access: access.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn the_owner_is_never_locked_out() {
        let mut alice = crate::state::test_principal("alice", OrgRole::Owner);
        alice.deny_ids = vec![
            "person/alice".into(),
            "company".into(),
            "company/engineering".into(),
        ];
        let s = stored(&alice, &a(&["company"], &["person/alice", "person/bob"])).unwrap();
        assert_eq!(s.access, ["person/alice", "company"]);
        assert_eq!(s.deny, ["person/bob"]);
        // Named again, the owner stays first and once.
        let s = stored(&alice, &a(&["company/sales", "person/alice"], &[])).unwrap();
        assert_eq!(s.access, ["person/alice", "company/sales"]);
        // Only me.
        assert_eq!(
            stored(&alice, &Audience::default()).unwrap().access,
            ["person/alice"]
        );
        // A group the owner is in can't be denied.
        let e = stored(&alice, &a(&["company"], &["company/engineering"])).unwrap_err();
        assert_eq!(e.code, access_code::BAD_ACCESS);
        assert!(e.message.contains("lock you out"));
    }

    #[test]
    fn shapes_are_refused_with_their_code() {
        let many: Vec<String> = (0..=margin_shared::AUDIENCE_MAX)
            .map(|i| format!("person/p{i}"))
            .collect();
        let e = check_shape(&Audience {
            access: many,
            deny: Vec::new(),
        })
        .unwrap_err();
        assert_eq!(e.code, access_code::AUDIENCE_TOO_LARGE);
        let e = check_shape(&a(&["nobody"], &[])).unwrap_err();
        assert_eq!(e.code, access_code::BAD_ACCESS);
        assert!(check_shape(&a(&["company", "agent/a1"], &["person/bob"])).is_ok());
    }
}
