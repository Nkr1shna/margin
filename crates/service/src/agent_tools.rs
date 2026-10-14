//! The service's tools for the agent: the library, courses, sources and jobs, run
//! in-process through the same functions the HTTP handlers use (same validation,
//! `write_lock`, events). Device tools come from `margin_agent::device_tools`.
//!
//! By phase (`docs/agent-contract.md`):
//! - 1, Read: `search_library`, `list_topics`, `get_topic`, `read_lesson`, `read_note`,
//!   `list_notes`, `search_sources` (passages card), `list_sources`.
//! - 2, Write: `create_topic`, `rename_topic`, `move_topic`, `edit_lesson_title`,
//!   `create_note`, `append_to_note`, `replace_note_body` (confirms when the note is the
//!   owner's own writing), `propose_course` (outline card), `accept_proposal` (result
//!   card); Destructive: `delete_topic`, `delete_lesson`, `delete_note`.
//! - 3, Write: `write_lesson`, `regenerate_lesson` (confirms when the lesson body was
//!   edited by the owner), `make_video` (result cards), `attach_sources` (files attached
//!   to the chat go into a topic).
//!
//! Outputs are compact JSON the model reads; library text (lesson bodies, notes, source
//! passages) only ever reaches the model this way, never in the developer prompt.
//!
//! Write and Destructive tools change the library through the same functions as the
//! routes (`library::new_topic`, `ask::accept_proposal`, …) and run each change to its
//! end even if the turn is stopped meanwhile (`to_the_end`, which registers it with
//! `Turns::register_write` so Stop records its real result). Who wrote a note or a
//! lesson body (`state::Author`) decides whether overwriting it asks first.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use margin_agent::{
    BoxFuture, ConfirmationPrompt, ToolContext, ToolKind, ToolOutput, ToolRegistry, Typed,
    TypedTool,
};
use margin_shared::{
    AcceptProposal, AskRequest, Card, Chat, Citation, LessonPatch, LessonStatus, Level, NewNote,
    NewTopic, Note, NotePatch, Outline, OutlineCard, OutlineChips, OutlineLesson, PassagesCard,
    Proposal, ProposalStatus, ResultCard, ResultTarget, Source, SourceStatus, Topic, TopicPatch,
    VideoStatus,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::error::{AppError, AppResult};
use crate::library::{self, sort_topics};
use crate::state::{AppState, Author, StoredLesson, StoredVideo};
use crate::{ask, db, embedding, search, sources, video};

/// How long `search_sources` waits for sources still being read before searching what
/// is ready.
pub const SOURCE_SEARCH_WAIT: Duration = Duration::from_secs(15);

/// Every tool, service tools first then `margin_agent::device_tools()`, in a fixed order
/// so the `tools` array is identical on every call. Phase 3 registers the app-control
/// tools (the turn offers them while the origin device is connected); the folder tools
/// join in phase 4.
pub fn registry(state: &AppState) -> ToolRegistry {
    let mut r = ToolRegistry::new();
    r.register(Arc::new(Typed(SearchLibrary(state.clone()))));
    r.register(Arc::new(Typed(ListTopics(state.clone()))));
    r.register(Arc::new(Typed(GetTopic(state.clone()))));
    r.register(Arc::new(Typed(ReadLesson(state.clone()))));
    r.register(Arc::new(Typed(ReadNote(state.clone()))));
    r.register(Arc::new(Typed(ListNotes(state.clone()))));
    r.register(Arc::new(Typed(SearchSources(state.clone()))));
    r.register(Arc::new(Typed(ListSources(state.clone()))));
    r.register(Arc::new(Typed(CreateTopic(state.clone()))));
    r.register(Arc::new(Typed(RenameTopic(state.clone()))));
    r.register(Arc::new(Typed(MoveTopic(state.clone()))));
    r.register(Arc::new(Typed(EditLessonTitle(state.clone()))));
    r.register(Arc::new(Typed(CreateNote(state.clone()))));
    r.register(Arc::new(Typed(AppendToNote(state.clone()))));
    r.register(Arc::new(Typed(ReplaceNoteBody(state.clone()))));
    r.register(Arc::new(Typed(ProposeCourse(state.clone()))));
    r.register(Arc::new(Typed(AcceptProposalTool(state.clone()))));
    r.register(Arc::new(Typed(WriteLesson(state.clone()))));
    r.register(Arc::new(Typed(RegenerateLesson(state.clone()))));
    r.register(Arc::new(Typed(MakeVideo(state.clone()))));
    r.register(Arc::new(Typed(AttachSources(state.clone()))));
    r.register(Arc::new(Typed(DeleteTopic(state.clone()))));
    r.register(Arc::new(Typed(DeleteLesson(state.clone()))));
    r.register(Arc::new(Typed(DeleteNote(state.clone()))));
    for tool in margin_agent::device_tools() {
        if margin_agent::APP_CONTROL_TOOLS.contains(&tool.name()) {
            r.register(tool);
        }
    }
    r
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A failed lookup as the model reads it.
fn failed(e: AppError) -> ToolOutput {
    ToolOutput::error(e.message)
}

fn internal(e: anyhow::Error) -> ToolOutput {
    tracing::error!("an agent tool failed: {e:#}");
    ToolOutput::error("The service couldn't read the library just now.")
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// `“text”` cut to a short label.
fn quoted(text: &str) -> String {
    let t = text.trim();
    let mut short: String = t.chars().take(60).collect();
    if t.chars().count() > 60 {
        short.push('…');
    }
    format!("“{short}”")
}

/// "Subject / Topic" for a topic id (the topic itself included).
fn topic_path(by_id: &HashMap<&str, &Topic>, topic_id: &str) -> String {
    let mut parts = Vec::new();
    let mut cur = by_id.get(topic_id).copied();
    while let Some(t) = cur {
        if parts.len() > 32 {
            break;
        }
        parts.push(t.title.as_str());
        cur = t.parent_id.as_deref().and_then(|p| by_id.get(p).copied());
    }
    parts.reverse();
    parts.join(" / ")
}

/// `topic_id` and every topic under it.
fn subtree(topics: &[Topic], topic_id: &str) -> HashSet<String> {
    let mut out: HashSet<String> = HashSet::from([topic_id.to_string()]);
    loop {
        let before = out.len();
        for t in topics {
            if t.parent_id.as_ref().is_some_and(|p| out.contains(p)) {
                out.insert(t.id.clone());
            }
        }
        if out.len() == before {
            return out;
        }
    }
}

fn lesson_status(status: &LessonStatus) -> String {
    match status {
        LessonStatus::Queued => "queued".into(),
        LessonStatus::Generating => "being written".into(),
        LessonStatus::Ready => "ready".into(),
        LessonStatus::Failed { error } => format!("failed: {error}"),
    }
}

fn video_status(video: Option<&StoredVideo>) -> String {
    let Some(v) = video else {
        return "none".into();
    };
    match &v.video.status {
        VideoStatus::Queued => "queued".into(),
        VideoStatus::Scripting => "scripting".into(),
        VideoStatus::Voicing { .. } => "voicing".into(),
        VideoStatus::Rendering { .. } => "rendering".into(),
        VideoStatus::Ready => "ready".into(),
        VideoStatus::Failed { error } => format!("failed: {error}"),
    }
}

fn source_status(status: &SourceStatus) -> String {
    match status {
        SourceStatus::Queued => "waiting to be read".into(),
        SourceStatus::Processing { .. } => "being read".into(),
        SourceStatus::Ready { warning: None, .. } => "ready".into(),
        SourceStatus::Ready {
            warning: Some(w), ..
        } => format!("ready, with a warning: {w}"),
        SourceStatus::Failed { error } => format!("failed: {error}"),
    }
}

/// The library at the moment of the call.
struct Snapshot {
    topics: Vec<Topic>,
    lessons: Vec<StoredLesson>,
    notes: Vec<Note>,
}

impl Snapshot {
    async fn load(state: &AppState) -> anyhow::Result<Self> {
        let mut topics: Vec<Topic> = state.store.list(db::TOPIC).await?;
        sort_topics(&mut topics);
        let mut lessons: Vec<StoredLesson> = state.store.list(db::LESSON).await?;
        lessons.sort_by(|a, b| {
            a.lesson
                .topic_id
                .cmp(&b.lesson.topic_id)
                .then(a.lesson.position.cmp(&b.lesson.position))
                .then(a.lesson.created_at.cmp(&b.lesson.created_at))
        });
        let mut notes: Vec<Note> = state.store.list(db::NOTE).await?;
        notes.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.id.cmp(&b.id)));
        Ok(Self {
            topics,
            lessons,
            notes,
        })
    }

    fn by_id(&self) -> HashMap<&str, &Topic> {
        self.topics.iter().map(|t| (t.id.as_str(), t)).collect()
    }

    fn lesson_title(&self, id: &str) -> Option<&str> {
        self.lessons
            .iter()
            .find(|l| l.lesson.id == id)
            .map(|l| l.lesson.title.as_str())
    }
}

/// Arguments of a tool that takes none.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct NoArgs {}

// ---------------------------------------------------------------------------
// search_library
// ---------------------------------------------------------------------------

pub struct SearchLibrary(pub AppState);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchLibraryArgs {
    /// Words to look for in topic titles, lesson titles and bodies, and notes
    /// (case-insensitive substring match, so prefer one or two distinctive words).
    pub query: String,
}

impl TypedTool for SearchLibrary {
    type Args = SearchLibraryArgs;
    const NAME: &'static str = "search_library";
    const DESCRIPTION: &'static str = "Search the owner's library (topics, lessons and notes) for words. Returns up to 30 hits, each with its kind, id, title, where it is (breadcrumb) and a snippet around the match. Use the ids with get_topic, read_lesson or read_note.";
    const KIND: ToolKind = ToolKind::Read;

    fn label(&self, args: &Self::Args) -> String {
        format!("Searching the library for {}", quoted(&args.query))
    }

    fn run<'a>(&'a self, _ctx: &'a ToolContext, args: Self::Args) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let query = args.query.trim();
            if query.is_empty() {
                return ToolOutput::error("Give some words to search for.");
            }
            let lib = match Snapshot::load(&self.0).await {
                Ok(l) => l,
                Err(e) => return internal(e),
            };
            let hits = search::run(query, &lib.topics, &lib.lessons, &lib.notes);
            let summary = match hits.len() {
                0 => "No matches".to_string(),
                n => plural(n, "match", "matches"),
            };
            let output = json!({ "query": query, "hits": hits });
            ToolOutput::ok(output.to_string(), summary)
        })
    }
}

// ---------------------------------------------------------------------------
// list_topics
// ---------------------------------------------------------------------------

pub struct ListTopics(pub AppState);

impl TypedTool for ListTopics {
    type Args = NoArgs;
    const NAME: &'static str = "list_topics";
    const DESCRIPTION: &'static str = "List every topic in the library as a tree (each topic with its id, parent_id and depth, in reading order: a subject is followed by the topics under it), with how many lessons and notes each has.";
    const KIND: ToolKind = ToolKind::Read;

    fn label(&self, _args: &Self::Args) -> String {
        "Listing the topics".into()
    }

    fn run<'a>(&'a self, _ctx: &'a ToolContext, _args: Self::Args) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let lib = match Snapshot::load(&self.0).await {
                Ok(l) => l,
                Err(e) => return internal(e),
            };
            let mut lessons: HashMap<&str, usize> = HashMap::new();
            for l in &lib.lessons {
                *lessons.entry(l.lesson.topic_id.as_str()).or_default() += 1;
            }
            let mut notes: HashMap<&str, usize> = HashMap::new();
            for n in &lib.notes {
                *notes.entry(n.topic_id.as_str()).or_default() += 1;
            }
            let mut children: HashMap<Option<&str>, Vec<&Topic>> = HashMap::new();
            for t in &lib.topics {
                children.entry(t.parent_id.as_deref()).or_default().push(t);
            }
            let known: HashSet<&str> = lib.topics.iter().map(|t| t.id.as_str()).collect();
            // Depth first from the roots; a topic whose parent is gone counts as a root.
            let mut stack: Vec<(&Topic, usize)> = lib
                .topics
                .iter()
                .filter(|t| t.parent_id.as_deref().is_none_or(|p| !known.contains(p)))
                .rev()
                .map(|t| (t, 0))
                .collect();
            let mut rows = Vec::new();
            let mut seen = HashSet::new();
            while let Some((t, depth)) = stack.pop() {
                if !seen.insert(t.id.as_str()) {
                    continue;
                }
                rows.push(json!({
                    "id": t.id,
                    "parent_id": t.parent_id,
                    "depth": depth,
                    "title": t.title,
                    "lessons": lessons.get(t.id.as_str()).copied().unwrap_or(0),
                    "notes": notes.get(t.id.as_str()).copied().unwrap_or(0),
                }));
                if let Some(kids) = children.get(&Some(t.id.as_str())) {
                    stack.extend(kids.iter().rev().map(|k| (*k, depth + 1)));
                }
            }
            let summary = match rows.len() {
                0 => "The library is empty".to_string(),
                n => plural(n, "topic", "topics"),
            };
            ToolOutput::ok(json!({ "topics": rows }).to_string(), summary)
        })
    }
}

// ---------------------------------------------------------------------------
// get_topic
// ---------------------------------------------------------------------------

pub struct GetTopic(pub AppState);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TopicArgs {
    /// The topic's id (from list_topics or search_library).
    pub topic_id: String,
}

impl TypedTool for GetTopic {
    type Args = TopicArgs;
    const NAME: &'static str = "get_topic";
    const DESCRIPTION: &'static str = "Get one topic: its path, the topics directly under it, its lessons in order (id, title, summary, status, video status), its notes (id, title) and its sources (id, name, kind, status).";
    const KIND: ToolKind = ToolKind::Read;

    fn label(&self, _args: &Self::Args) -> String {
        "Opening a topic".into()
    }

    fn run<'a>(&'a self, _ctx: &'a ToolContext, args: Self::Args) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let state = &self.0;
            let topic = match library::topic(state, &args.topic_id).await {
                Ok(t) => t,
                Err(e) => return failed(e),
            };
            let result = async {
                let lib = Snapshot::load(state).await?;
                let videos: Vec<StoredVideo> = state.store.list(db::VIDEO).await?;
                let mut topic_sources: Vec<Source> = state
                    .store
                    .list_where(db::SOURCE, "topic_id", &topic.id)
                    .await?;
                topic_sources.sort_by_key(|s| s.created_at);
                anyhow::Ok((lib, videos, topic_sources))
            };
            let (lib, videos, topic_sources) = match result.await {
                Ok(r) => r,
                Err(e) => return internal(e),
            };
            let by_id = lib.by_id();
            let lessons: Vec<Value> = lib
                .lessons
                .iter()
                .filter(|l| l.lesson.topic_id == topic.id)
                .map(|l| {
                    let video = videos.iter().find(|v| v.video.lesson_id == l.lesson.id);
                    json!({
                        "id": l.lesson.id,
                        "title": l.lesson.title,
                        "summary": l.lesson.summary,
                        "status": lesson_status(&l.lesson.status),
                        "video": video_status(video),
                    })
                })
                .collect();
            let notes: Vec<Value> = lib
                .notes
                .iter()
                .filter(|n| n.topic_id == topic.id)
                .map(|n| json!({ "id": n.id, "title": n.title }))
                .collect();
            let subtopics: Vec<Value> = lib
                .topics
                .iter()
                .filter(|t| t.parent_id.as_deref() == Some(topic.id.as_str()))
                .map(|t| json!({ "id": t.id, "title": t.title }))
                .collect();
            let source_rows: Vec<Value> = topic_sources
                .iter()
                .map(|s| {
                    json!({
                        "id": s.id,
                        "name": s.name,
                        "kind": s.kind,
                        "status": source_status(&s.status),
                    })
                })
                .collect();
            let summary = format!(
                "{}: {}, {}",
                topic.title,
                plural(lessons.len(), "lesson", "lessons"),
                plural(notes.len(), "note", "notes")
            );
            let output = json!({
                "id": topic.id,
                "title": topic.title,
                "path": topic_path(&by_id, &topic.id),
                "parent_id": topic.parent_id,
                "subtopics": subtopics,
                "lessons": lessons,
                "notes": notes,
                "sources": source_rows,
            });
            ToolOutput::ok(output.to_string(), summary)
        })
    }
}

// ---------------------------------------------------------------------------
// read_lesson
// ---------------------------------------------------------------------------

pub struct ReadLesson(pub AppState);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LessonArgs {
    /// The lesson's id.
    pub lesson_id: String,
}

impl TypedTool for ReadLesson {
    type Args = LessonArgs;
    const NAME: &'static str = "read_lesson";
    const DESCRIPTION: &'static str = "Read a lesson: its title, topic path, status, whole Markdown body (footnote markers [^n] refer to its citations) and citations (n, source, page or time).";
    const KIND: ToolKind = ToolKind::Read;

    fn label(&self, _args: &Self::Args) -> String {
        "Reading a lesson".into()
    }

    fn run<'a>(&'a self, _ctx: &'a ToolContext, args: Self::Args) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let state = &self.0;
            let row = match library::lesson(state, &args.lesson_id).await {
                Ok(l) => l,
                Err(e) => return failed(e),
            };
            let topics: Vec<Topic> = match state.store.list(db::TOPIC).await {
                Ok(t) => t,
                Err(e) => return internal(e),
            };
            let by_id: HashMap<&str, &Topic> = topics.iter().map(|t| (t.id.as_str(), t)).collect();
            let l = &row.lesson;
            let citations: Vec<Value> = l.citations.iter().map(citation_json).collect();
            let output = json!({
                "id": l.id,
                "title": l.title,
                "topic_id": l.topic_id,
                "topic": topic_path(&by_id, &l.topic_id),
                "status": lesson_status(&l.status),
                "summary": l.summary,
                "level": l.level,
                "minutes": l.minutes,
                "body_md": l.body_md,
                "citations": citations,
            });
            ToolOutput::ok(output.to_string(), format!("Read {}", quoted(&l.title)))
        })
    }
}

fn citation_json(c: &Citation) -> Value {
    json!({
        "n": c.n,
        "source_id": c.source_id,
        "source": c.source_name,
        "page": c.page,
        "start_ms": c.start_ms,
        "end_ms": c.end_ms,
    })
}

// ---------------------------------------------------------------------------
// read_note
// ---------------------------------------------------------------------------

pub struct ReadNote(pub AppState);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NoteArgs {
    /// The note's id.
    pub note_id: String,
}

impl TypedTool for ReadNote {
    type Args = NoteArgs;
    const NAME: &'static str = "read_note";
    const DESCRIPTION: &'static str = "Read a note: its title, topic path, the lesson it belongs to (if any), the lesson passage it annotates (quote, for margin notes) and its Markdown body.";
    const KIND: ToolKind = ToolKind::Read;

    fn label(&self, _args: &Self::Args) -> String {
        "Reading a note".into()
    }

    fn run<'a>(&'a self, _ctx: &'a ToolContext, args: Self::Args) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let state = &self.0;
            let note = match library::note(state, &args.note_id).await {
                Ok(n) => n,
                Err(e) => return failed(e),
            };
            let lib = match Snapshot::load(state).await {
                Ok(l) => l,
                Err(e) => return internal(e),
            };
            let title = if note.title.is_empty() {
                "Untitled note".to_string()
            } else {
                note.title.clone()
            };
            let output = json!({
                "id": note.id,
                "title": title,
                "topic_id": note.topic_id,
                "topic": topic_path(&lib.by_id(), &note.topic_id),
                "lesson_id": note.lesson_id,
                "lesson": note.lesson_id.as_deref().and_then(|id| lib.lesson_title(id)),
                "quote": note.quote,
                "body_md": note.body_md,
                "updated_at": note.updated_at,
            });
            ToolOutput::ok(output.to_string(), format!("Read {}", quoted(&title)))
        })
    }
}

// ---------------------------------------------------------------------------
// list_notes
// ---------------------------------------------------------------------------

pub struct ListNotes(pub AppState);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListNotesArgs {
    /// Only notes in this topic and the topics under it; null for every note.
    pub topic_id: Option<String>,
}

impl TypedTool for ListNotes {
    type Args = ListNotesArgs;
    const NAME: &'static str = "list_notes";
    const DESCRIPTION: &'static str = "List notes, most recently edited first: id, title, topic path, lesson title, updated_at. Optionally only those in a topic (including the topics under it). Read one with read_note.";
    const KIND: ToolKind = ToolKind::Read;

    fn label(&self, args: &Self::Args) -> String {
        match args.topic_id {
            Some(_) => "Listing a topic's notes".into(),
            None => "Listing notes".into(),
        }
    }

    fn run<'a>(&'a self, _ctx: &'a ToolContext, args: Self::Args) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let state = &self.0;
            if let Some(t) = &args.topic_id
                && let Err(e) = library::topic(state, t).await
            {
                return failed(e);
            }
            let lib = match Snapshot::load(state).await {
                Ok(l) => l,
                Err(e) => return internal(e),
            };
            let within = args.topic_id.as_deref().map(|t| subtree(&lib.topics, t));
            let by_id = lib.by_id();
            let rows: Vec<Value> = lib
                .notes
                .iter()
                .filter(|n| within.as_ref().is_none_or(|w| w.contains(&n.topic_id)))
                .map(|n| {
                    json!({
                        "id": n.id,
                        "title": if n.title.is_empty() { "Untitled note" } else { n.title.as_str() },
                        "topic": topic_path(&by_id, &n.topic_id),
                        "lesson": n.lesson_id.as_deref().and_then(|id| lib.lesson_title(id)),
                        "updated_at": n.updated_at,
                    })
                })
                .collect();
            let summary = match rows.len() {
                0 => "No notes".to_string(),
                n => plural(n, "note", "notes"),
            };
            ToolOutput::ok(json!({ "notes": rows }).to_string(), summary)
        })
    }
}

// ---------------------------------------------------------------------------
// search_sources
// ---------------------------------------------------------------------------

pub struct SearchSources(pub AppState);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchSourcesArgs {
    /// What to look for, as a question or a few words; matched by meaning.
    pub query: String,
    /// Only the sources of this topic (and the topics under it); null to not limit by
    /// topic.
    pub topic_id: Option<String>,
    /// Only these sources; null (or empty) to not limit by source.
    pub source_ids: Option<Vec<String>>,
}

impl TypedTool for SearchSources {
    type Args = SearchSourcesArgs;
    const NAME: &'static str = "search_sources";
    const DESCRIPTION: &'static str = "Find the passages of the owner's sources (uploaded files: text, code, PDFs, images, audio, video) most relevant to a query, best first (at most 8). Searches the given source_ids, else the topic's sources, else the files attached to this chat and every other source. Each passage has a number n, its source, page or time, and text; cite them by source name and page or time. The passages are shown to the owner as a card.";
    const KIND: ToolKind = ToolKind::Read;

    fn label(&self, args: &Self::Args) -> String {
        format!("Searching sources for {}", quoted(&args.query))
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: Self::Args) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let state = &self.0;
            let query = args.query.trim().to_string();
            if query.is_empty() {
                return ToolOutput::error("Give something to search for.");
            }
            let scope = match search_scope(state, ctx, &args).await {
                Ok(Ok(ids)) => ids,
                Ok(Err(out)) => return out,
                Err(e) => return internal(e),
            };
            if scope.is_empty() {
                return ToolOutput::error(
                    "There are no sources to search. The owner can attach files to this chat or add them to a topic.",
                );
            }
            if !embedding::will_be_available(state).await {
                return ToolOutput::error(
                    "Searching sources needs the local embedding model, which isn't downloaded. The owner can download it in Settings → Sources.",
                );
            }
            let found =
                match sources::passages_within(state, &scope, &query, SOURCE_SEARCH_WAIT).await {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!("search_sources failed: {e:#}");
                        return ToolOutput::error(format!("The sources couldn't be searched: {e}"));
                    }
                };
            // Sources still not read after the wait, so the model can say so.
            let mut unread = Vec::new();
            for id in &scope {
                if let Ok(Some(s)) = state.store.get::<Source>(db::SOURCE, id).await
                    && sources::is_pending(&s.status)
                {
                    unread.push(s.name);
                }
            }
            let mut citations = Vec::new();
            let mut rows = Vec::new();
            let mut from = HashSet::new();
            for (i, r) in found.iter().enumerate() {
                let n = i as u32 + 1;
                from.insert(r.citation.source_id.clone());
                rows.push(json!({
                    "n": n,
                    "source_id": r.citation.source_id,
                    "source": r.citation.source_name,
                    "page": r.citation.page,
                    "start_ms": r.citation.start_ms,
                    "end_ms": r.citation.end_ms,
                    "text": r.passage.text,
                    "picture": r.passage.image.is_some(),
                }));
                // `image_url` stays a bare media file name; chats sign it on the way out.
                citations.push(Citation {
                    n,
                    ..r.citation.clone()
                });
            }
            let summary = if rows.is_empty() {
                "No passages found".to_string()
            } else {
                format!(
                    "{} from {}",
                    plural(rows.len(), "passage", "passages"),
                    plural(from.len(), "source", "sources")
                )
            };
            let mut output = json!({ "query": query, "passages": rows });
            if !unread.is_empty() {
                output["still_being_read"] = json!(unread);
            }
            let mut out = ToolOutput::ok(output.to_string(), summary);
            if !citations.is_empty() {
                out = out.with_card(Card::Passages(PassagesCard {
                    query,
                    passages: citations,
                }));
            }
            out
        })
    }
}

/// The sources `search_sources` looks in: the given ids, else the topic's (and its
/// subtopics'), else the chat's files followed by every other source. `Ok(Err(..))` is
/// an error output for the model.
async fn search_scope(
    state: &AppState,
    ctx: &ToolContext,
    args: &SearchSourcesArgs,
) -> anyhow::Result<Result<Vec<String>, ToolOutput>> {
    let mut all: Vec<Source> = state.store.list(db::SOURCE).await?;
    all.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    if let Some(ids) = args.source_ids.as_ref().filter(|ids| !ids.is_empty()) {
        let known: Vec<String> = ids
            .iter()
            .filter(|id| all.iter().any(|s| &s.id == *id))
            .cloned()
            .collect();
        if known.is_empty() {
            return Ok(Err(ToolOutput::error(
                "None of those sources exist. Use list_sources to see them.",
            )));
        }
        return Ok(Ok(known));
    }
    if let Some(topic_id) = &args.topic_id {
        if let Err(e) = library::topic(state, topic_id).await {
            return Ok(Err(failed(e)));
        }
        let topics: Vec<Topic> = state.store.list(db::TOPIC).await?;
        let within = subtree(&topics, topic_id);
        return Ok(Ok(all
            .into_iter()
            .filter(|s| s.topic_id.as_ref().is_some_and(|t| within.contains(t)))
            .map(|s| s.id)
            .collect()));
    }
    let chat: Option<Chat> = state.store.get(db::CHAT, &ctx.chat_id).await?;
    let mut ids: Vec<String> = chat.map(|c| c.source_ids).unwrap_or_default();
    for s in all {
        if !ids.contains(&s.id) {
            ids.push(s.id);
        }
    }
    Ok(Ok(ids))
}

// ---------------------------------------------------------------------------
// list_sources
// ---------------------------------------------------------------------------

pub struct ListSources(pub AppState);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListSourcesArgs {
    /// Only the sources of this topic (and the topics under it); null for every source.
    pub topic_id: Option<String>,
}

impl TypedTool for ListSources {
    type Args = ListSourcesArgs;
    const NAME: &'static str = "list_sources";
    const DESCRIPTION: &'static str = "List the owner's sources (uploaded files): id, name, kind, status, the topic it belongs to (null when it isn't in a topic) and whether it is attached to this chat. Optionally only a topic's.";
    const KIND: ToolKind = ToolKind::Read;

    fn label(&self, args: &Self::Args) -> String {
        match args.topic_id {
            Some(_) => "Listing a topic's sources".into(),
            None => "Listing sources".into(),
        }
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: Self::Args) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let state = &self.0;
            if let Some(t) = &args.topic_id
                && let Err(e) = library::topic(state, t).await
            {
                return failed(e);
            }
            let loaded = async {
                let mut all: Vec<Source> = state.store.list(db::SOURCE).await?;
                all.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
                let topics: Vec<Topic> = state.store.list(db::TOPIC).await?;
                let chat: Option<Chat> = state.store.get(db::CHAT, &ctx.chat_id).await?;
                anyhow::Ok((all, topics, chat))
            };
            let (all, topics, chat) = match loaded.await {
                Ok(l) => l,
                Err(e) => return internal(e),
            };
            let in_chat: HashSet<String> = chat
                .map(|c| c.source_ids.into_iter().collect())
                .unwrap_or_default();
            let within = args.topic_id.as_deref().map(|t| subtree(&topics, t));
            let by_id: HashMap<&str, &Topic> = topics.iter().map(|t| (t.id.as_str(), t)).collect();
            let rows: Vec<Value> = all
                .iter()
                .filter(|s| match &within {
                    Some(w) => s.topic_id.as_ref().is_some_and(|t| w.contains(t)),
                    None => true,
                })
                .map(|s| {
                    json!({
                        "id": s.id,
                        "name": s.name,
                        "kind": s.kind,
                        "status": source_status(&s.status),
                        "topic_id": s.topic_id,
                        "topic": s.topic_id.as_deref().map(|t| topic_path(&by_id, t)),
                        "attached_to_this_chat": in_chat.contains(&s.id),
                    })
                })
                .collect();
            let summary = match rows.len() {
                0 => "No sources".to_string(),
                n => plural(n, "source", "sources"),
            };
            ToolOutput::ok(json!({ "sources": rows }).to_string(), summary)
        })
    }
}

// ---------------------------------------------------------------------------
// Write helpers
// ---------------------------------------------------------------------------

/// Runs a change to the library to its end even when the turn is stopped meanwhile
/// (Stop drops the tool's future): a topic, an accepted course or a delete is never
/// left half done. Like a job a tool started, it carries on after Stop.
///
/// The change runs as its own task, registered as this call with
/// `Turns::register_write` before it starts: Stop awaits it and records its real result
/// (so the model doesn't make the change again). The task hands the tool a copy of
/// that result.
async fn to_the_end<F>(state: &AppState, ctx: &ToolContext, work: F) -> ToolOutput
where
    F: Future<Output = ToolOutput> + Send + 'static,
{
    let (start, started) = oneshot::channel::<()>();
    let (done, result) = oneshot::channel::<ToolOutput>();
    let handle = tokio::spawn(async move {
        // Registered first, so Stop never misses a change that ran.
        let _ = started.await;
        let out = work.await;
        let _ = done.send(out.clone());
        out
    });
    state
        .turns
        .register_write(&ctx.chat_id, &ctx.call_id, handle);
    let _ = start.send(());
    match result.await {
        Ok(out) => out,
        Err(_) => {
            tracing::error!("an agent tool's change failed (its task ended without a result)");
            ToolOutput::error("The service failed while making this change. Check its log.")
        }
    }
}

/// "a, b and c".
fn and_list(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// "2 lessons and 1 note go with it." / "1 note goes with it."
fn go_with_it(parts: &[String]) -> String {
    let one = matches!(parts, [only] if only.starts_with("1 ") || only == "its video");
    let verb = if one { "goes" } else { "go" };
    format!("{} {verb} with it.", and_list(parts))
}

fn result_card(target: ResultTarget, title: &str) -> Card {
    Card::Result(ResultCard {
        target,
        title: title.to_string(),
    })
}

fn note_title(note: &Note) -> &str {
    if note.title.is_empty() {
        "Untitled note"
    } else {
        &note.title
    }
}

/// The first `max` characters of `text` on one line, "…" when cut.
fn excerpt(text: &str, max: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut short: String = one_line.chars().take(max).collect();
    if one_line.chars().count() > max {
        short.push('…');
    }
    short
}

/// A course level as the model writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LevelArg {
    Intro,
    Intermediate,
    Advanced,
}

impl From<LevelArg> for Level {
    fn from(level: LevelArg) -> Self {
        match level {
            LevelArg::Intro => Level::Intro,
            LevelArg::Intermediate => Level::Intermediate,
            LevelArg::Advanced => Level::Advanced,
        }
    }
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::Intro => "intro",
        Level::Intermediate => "intermediate",
        Level::Advanced => "advanced",
    }
}

// ---------------------------------------------------------------------------
// create_topic
// ---------------------------------------------------------------------------

pub struct CreateTopic(pub AppState);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateTopicArgs {
    /// The new topic's title.
    pub title: String,
    /// The topic to put it under; null for a new subject at the top level.
    pub parent_id: Option<String>,
}

impl TypedTool for CreateTopic {
    type Args = CreateTopicArgs;
    const NAME: &'static str = "create_topic";
    const DESCRIPTION: &'static str = "Create an empty topic, last under parent_id (or at the top level as a new subject when parent_id is null). For a course with lessons use propose_course instead.";
    const KIND: ToolKind = ToolKind::Write;

    fn label(&self, args: &Self::Args) -> String {
        format!("Creating the topic {}", quoted(&args.title))
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: Self::Args) -> BoxFuture<'a, ToolOutput> {
        let state = self.0.clone();
        Box::pin(to_the_end(&self.0, ctx, async move {
            let req = NewTopic {
                parent_id: args.parent_id,
                title: args.title,
            };
            let topic = match library::new_topic(&state, req).await {
                Ok(t) => t,
                Err(e) => return failed(e),
            };
            let output = json!({
                "id": topic.id,
                "title": topic.title,
                "parent_id": topic.parent_id,
            });
            ToolOutput::ok(
                output.to_string(),
                format!("Created {}", quoted(&topic.title)),
            )
            .with_card(result_card(
                ResultTarget::Topic {
                    topic_id: topic.id.clone(),
                },
                &topic.title,
            ))
        }))
    }
}

// ---------------------------------------------------------------------------
// rename_topic
// ---------------------------------------------------------------------------

pub struct RenameTopic(pub AppState);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RenameTopicArgs {
    /// The topic's id.
    pub topic_id: String,
    /// Its new title.
    pub title: String,
}

impl TypedTool for RenameTopic {
    type Args = RenameTopicArgs;
    const NAME: &'static str = "rename_topic";
    const DESCRIPTION: &'static str = "Rename a topic (a subject or a topic under one).";
    const KIND: ToolKind = ToolKind::Write;

    fn label(&self, args: &Self::Args) -> String {
        format!("Renaming a topic to {}", quoted(&args.title))
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: Self::Args) -> BoxFuture<'a, ToolOutput> {
        let state = self.0.clone();
        Box::pin(to_the_end(&self.0, ctx, async move {
            let before = match library::topic(&state, &args.topic_id).await {
                Ok(t) => t,
                Err(e) => return failed(e),
            };
            let patch = TopicPatch {
                title: Some(args.title),
                ..TopicPatch::default()
            };
            let topic = match library::update_topic(&state, &args.topic_id, patch).await {
                Ok(t) => t,
                Err(e) => return failed(e),
            };
            let output = json!({
                "id": topic.id,
                "title": topic.title,
                "previous_title": before.title,
            });
            ToolOutput::ok(
                output.to_string(),
                format!("Renamed to {}", quoted(&topic.title)),
            )
        }))
    }
}

// ---------------------------------------------------------------------------
// move_topic
// ---------------------------------------------------------------------------

pub struct MoveTopic(pub AppState);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MoveTopicArgs {
    /// The topic to move (with everything under it).
    pub topic_id: String,
    /// The topic to move it under; null to make it a subject at the top level.
    pub parent_id: Option<String>,
    /// Where among its new siblings, counting from 0 (0 = first); null to put it last.
    pub position: Option<u32>,
}

impl TypedTool for MoveTopic {
    type Args = MoveTopicArgs;
    const NAME: &'static str = "move_topic";
    const DESCRIPTION: &'static str = "Move a topic, with its lessons, notes and the topics under it, under another topic or to the top level, and/or reorder it among its siblings. To only reorder, pass its current parent_id.";
    const KIND: ToolKind = ToolKind::Write;

    fn label(&self, args: &Self::Args) -> String {
        match args.parent_id {
            Some(_) => "Moving a topic".into(),
            None => "Moving a topic to the top level".into(),
        }
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: Self::Args) -> BoxFuture<'a, ToolOutput> {
        let state = self.0.clone();
        Box::pin(to_the_end(&self.0, ctx, async move {
            let index = args.position.map(|p| p as usize);
            let topic =
                match library::place_topic(&state, &args.topic_id, args.parent_id, index).await {
                    Ok(t) => t,
                    Err(e) => return failed(e),
                };
            let topics: Vec<Topic> = match state.store.list(db::TOPIC).await {
                Ok(t) => t,
                Err(e) => return internal(e),
            };
            let by_id: HashMap<&str, &Topic> = topics.iter().map(|t| (t.id.as_str(), t)).collect();
            let summary = match topic.parent_id.as_deref().and_then(|p| by_id.get(p)) {
                Some(parent) => format!(
                    "Moved {} under {}",
                    quoted(&topic.title),
                    quoted(&parent.title)
                ),
                None => format!("Moved {} to the top level", quoted(&topic.title)),
            };
            let output = json!({
                "id": topic.id,
                "title": topic.title,
                "parent_id": topic.parent_id,
                "path": topic_path(&by_id, &topic.id),
                "position": topic.position,
            });
            ToolOutput::ok(output.to_string(), summary)
        }))
    }
}

// ---------------------------------------------------------------------------
// edit_lesson_title
// ---------------------------------------------------------------------------

pub struct EditLessonTitle(pub AppState);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditLessonTitleArgs {
    /// The lesson's id.
    pub lesson_id: String,
    /// Its new title.
    pub title: String,
}

impl TypedTool for EditLessonTitle {
    type Args = EditLessonTitleArgs;
    const NAME: &'static str = "edit_lesson_title";
    const DESCRIPTION: &'static str = "Change a lesson's title. The body is left as it is.";
    const KIND: ToolKind = ToolKind::Write;

    fn label(&self, args: &Self::Args) -> String {
        format!("Retitling a lesson {}", quoted(&args.title))
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: Self::Args) -> BoxFuture<'a, ToolOutput> {
        let state = self.0.clone();
        Box::pin(to_the_end(&self.0, ctx, async move {
            let before = match library::lesson(&state, &args.lesson_id).await {
                Ok(l) => l,
                Err(e) => return failed(e),
            };
            let patch = LessonPatch {
                title: Some(args.title),
                ..LessonPatch::default()
            };
            let lesson =
                match library::update_lesson(&state, &args.lesson_id, patch, Author::Agent).await {
                    Ok(l) => l,
                    Err(e) => return failed(e),
                };
            let output = json!({
                "id": lesson.id,
                "title": lesson.title,
                "previous_title": before.lesson.title,
            });
            ToolOutput::ok(
                output.to_string(),
                format!("Retitled {}", quoted(&lesson.title)),
            )
        }))
    }
}

// ---------------------------------------------------------------------------
// create_note
// ---------------------------------------------------------------------------

pub struct CreateNote(pub AppState);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateNoteArgs {
    /// The topic the note belongs to.
    pub topic_id: String,
    /// A lesson of that topic to attach it to; null for a note on the topic itself.
    pub lesson_id: Option<String>,
    /// The note's title.
    pub title: String,
    /// The note in Markdown ($…$ for math).
    pub body_md: String,
}

impl TypedTool for CreateNote {
    type Args = CreateNoteArgs;
    const NAME: &'static str = "create_note";
    const DESCRIPTION: &'static str = "Write a new note in a topic, optionally attached to one of its lessons. The note is marked as yours (the assistant's), so you may later replace its text without asking.";
    const KIND: ToolKind = ToolKind::Write;

    fn label(&self, args: &Self::Args) -> String {
        format!("Writing the note {}", quoted(&args.title))
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: Self::Args) -> BoxFuture<'a, ToolOutput> {
        let state = self.0.clone();
        Box::pin(to_the_end(&self.0, ctx, async move {
            let title = match library::clean_title(&args.title, "note") {
                Ok(t) => t,
                Err(e) => return failed(e),
            };
            let req = NewNote {
                topic_id: args.topic_id,
                lesson_id: args.lesson_id,
                title,
                body_md: args.body_md,
                quote: None,
            };
            let note = match library::new_note(&state, req, Author::Agent).await {
                Ok(n) => n,
                Err(e) => return failed(e),
            };
            let output = json!({
                "id": note.id,
                "title": note.title,
                "topic_id": note.topic_id,
                "lesson_id": note.lesson_id,
            });
            ToolOutput::ok(output.to_string(), format!("Wrote {}", quoted(&note.title))).with_card(
                result_card(
                    ResultTarget::Note {
                        note_id: note.id.clone(),
                    },
                    &note.title,
                ),
            )
        }))
    }
}

// ---------------------------------------------------------------------------
// append_to_note
// ---------------------------------------------------------------------------

pub struct AppendToNote(pub AppState);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AppendToNoteArgs {
    /// The note's id.
    pub note_id: String,
    /// Markdown to add at the end, after a blank line.
    pub text: String,
}

impl TypedTool for AppendToNote {
    type Args = AppendToNoteArgs;
    const NAME: &'static str = "append_to_note";
    const DESCRIPTION: &'static str = "Add text at the end of a note (any note, the owner's too); what is already there is kept. Prefer this to replace_note_body when adding to a note.";
    const KIND: ToolKind = ToolKind::Write;

    fn label(&self, _args: &Self::Args) -> String {
        "Adding to a note".into()
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: Self::Args) -> BoxFuture<'a, ToolOutput> {
        let state = self.0.clone();
        Box::pin(to_the_end(&self.0, ctx, async move {
            let text = args.text.trim();
            if text.is_empty() {
                return ToolOutput::error("Give some text to add.");
            }
            let note = match library::append_to_note(&state, &args.note_id, text).await {
                Ok(n) => n,
                Err(e) => return failed(e),
            };
            let title = note_title(&note).to_string();
            let output = json!({
                "id": note.id,
                "title": title,
                "body_bytes": note.body_md.len(),
            });
            ToolOutput::ok(output.to_string(), format!("Added to {}", quoted(&title)))
        }))
    }
}

// ---------------------------------------------------------------------------
// replace_note_body
// ---------------------------------------------------------------------------

pub struct ReplaceNoteBody(pub AppState);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReplaceNoteBodyArgs {
    /// The note's id.
    pub note_id: String,
    /// The note's whole new text in Markdown; the old text is gone afterwards.
    pub body_md: String,
}

impl TypedTool for ReplaceNoteBody {
    type Args = ReplaceNoteBodyArgs;
    const NAME: &'static str = "replace_note_body";
    const DESCRIPTION: &'static str = "Replace a note's whole text (its title stays). Read the note first. When the owner wrote the note's text, the app asks them to confirm first; notes you wrote, and empty notes, change right away.";
    const KIND: ToolKind = ToolKind::Write;

    fn label(&self, _args: &Self::Args) -> String {
        "Rewriting a note".into()
    }

    fn confirmation<'a>(
        &'a self,
        _ctx: &'a ToolContext,
        args: &'a Self::Args,
    ) -> BoxFuture<'a, Result<Option<ConfirmationPrompt>, String>> {
        Box::pin(async move {
            let row = library::stored_note(&self.0, &args.note_id)
                .await
                .map_err(|e| e.message)?;
            // An empty body has none of the owner's writing to overwrite.
            let words = row.note.body_md.split_whitespace().count();
            if row.author != Author::Owner || words == 0 {
                return Ok(None);
            }
            let detail = format!(
                "You wrote this note. Its text ({}) would be replaced by the assistant's; this can't be undone.",
                plural(words, "word", "words")
            );
            Ok(Some(ConfirmationPrompt {
                title: format!(
                    "Replace the text of your note {}?",
                    quoted(note_title(&row.note))
                ),
                detail,
                approve_label: "Replace".into(),
            }))
        })
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: Self::Args) -> BoxFuture<'a, ToolOutput> {
        let state = self.0.clone();
        Box::pin(to_the_end(&self.0, ctx, async move {
            let patch = NotePatch {
                title: None,
                body_md: Some(args.body_md),
            };
            let note = match library::update_note(&state, &args.note_id, patch, Author::Agent).await
            {
                Ok(n) => n,
                Err(e) => return failed(e),
            };
            let title = note_title(&note).to_string();
            let output = json!({
                "id": note.id,
                "title": title,
                "body_bytes": note.body_md.len(),
            });
            ToolOutput::ok(output.to_string(), format!("Rewrote {}", quoted(&title)))
        }))
    }
}

// ---------------------------------------------------------------------------
// propose_course
// ---------------------------------------------------------------------------

pub struct ProposeCourse(pub AppState);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OutlineLessonArg {
    /// The lesson's title.
    pub title: String,
    /// One line on what it covers.
    pub summary: String,
    /// Estimated reading time in minutes.
    pub minutes: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProposeCourseArgs {
    /// What the owner wants to learn, in their own words (with what they said about
    /// their background and goal); the lesson writer reads it.
    pub prompt: String,
    /// The new topic's title, e.g. "Fourier series".
    pub topic_title: String,
    /// The subject (a top-level topic) to file the course under when parent_topic_id
    /// is null; an existing subject with the same title is reused (see list_topics).
    /// Ignored when parent_topic_id is given.
    pub subject_title: String,
    /// 1 to 12 lessons, in reading order.
    pub lessons: Vec<OutlineLessonArg>,
    /// How advanced the lessons are.
    pub level: LevelArg,
    /// Whether each lesson also gets a narrated explainer video once it is written.
    pub with_videos: bool,
    /// Whether each lesson ends with a short "Check yourself" quiz.
    pub with_quiz: bool,
    /// An existing topic to file the course under instead of a subject; null to use
    /// subject_title.
    pub parent_topic_id: Option<String>,
    /// Sources the lessons draw on (ids from list_sources or the chat's attached
    /// files): files not yet in a topic, or the parent topic's. Accepting the course
    /// files them under it. Empty for none.
    pub source_ids: Vec<String>,
}

impl TypedTool for ProposeCourse {
    type Args = ProposeCourseArgs;
    const NAME: &'static str = "propose_course";
    const DESCRIPTION: &'static str = "Propose a course: an outline of lessons shown to the owner as an editable card in the chat, where they can change titles and the level, videos and quiz options, then press Create lessons. Nothing is written until the course is accepted (by that button, or by accept_proposal when the owner tells you to go ahead). Write the outline yourself; when the course should draw on the owner's sources, search them first and pass their ids. Infer the options the owner didn't state from what they said (their background for the level, whether they want videos or a quiz); when one matters and can't be inferred, ask them in one short question instead of proposing.";
    const KIND: ToolKind = ToolKind::Write;

    fn label(&self, args: &Self::Args) -> String {
        format!("Proposing the course {}", quoted(&args.topic_title))
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: Self::Args) -> BoxFuture<'a, ToolOutput> {
        let state = self.0.clone();
        let chat_id = ctx.chat_id.clone();
        Box::pin(to_the_end(&self.0, ctx, async move {
            let request = AskRequest {
                prompt: args.prompt,
                lesson_count: args.lessons.len() as u32,
                level: args.level.into(),
                with_videos: args.with_videos,
                with_quiz: args.with_quiz,
                parent_topic_id: args.parent_topic_id,
                source_ids: args.source_ids,
            };
            let outline = Outline {
                topic_title: args.topic_title,
                subject_title: args.subject_title,
                lessons: args
                    .lessons
                    .into_iter()
                    .map(|l| OutlineLesson {
                        title: l.title,
                        summary: l.summary,
                        minutes: l.minutes,
                    })
                    .collect(),
            };
            let proposal = match ask::propose(&state, &chat_id, request, outline).await {
                Ok(p) => p,
                Err(e) => return failed(e),
            };
            let Some(outline) = &proposal.outline else {
                return ToolOutput::error("The outline wasn't kept.");
            };
            let filed_under = match &proposal.request.parent_topic_id {
                Some(parent) => {
                    let topics: Vec<Topic> = state.store.list(db::TOPIC).await.unwrap_or_default();
                    let by_id: HashMap<&str, &Topic> =
                        topics.iter().map(|t| (t.id.as_str(), t)).collect();
                    topic_path(&by_id, parent)
                }
                None => outline.subject_title.clone(),
            };
            let req = &proposal.request;
            let output = json!({
                "proposal_id": proposal.id,
                "topic_title": outline.topic_title,
                "filed_under": filed_under,
                "lessons": outline.lessons.iter().map(|l| &l.title).collect::<Vec<_>>(),
                "level": level_name(req.level),
                "with_videos": req.with_videos,
                "with_quiz": req.with_quiz,
                "source_ids": req.source_ids,
                "next": "The owner sees the outline card. They can edit it and press Create lessons, or ask you to go ahead (then call accept_proposal with this proposal_id).",
            });
            let summary = format!(
                "Proposed {} · {}",
                quoted(&outline.topic_title),
                plural(outline.lessons.len(), "lesson", "lessons")
            );
            ToolOutput::ok(output.to_string(), summary).with_card(Card::Outline(OutlineCard {
                proposal_id: proposal.id.clone(),
            }))
        }))
    }
}

// ---------------------------------------------------------------------------
// accept_proposal
// ---------------------------------------------------------------------------

pub struct AcceptProposalTool(pub AppState);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AcceptProposalArgs {
    /// The proposal_id propose_course returned.
    pub proposal_id: String,
}

impl TypedTool for AcceptProposalTool {
    type Args = AcceptProposalArgs;
    const NAME: &'static str = "accept_proposal";
    const DESCRIPTION: &'static str = "Create the lessons of a course proposed in this chat, as it stands on its card (the same as the owner pressing Create lessons): the topic is created, its lessons are queued and written one after another, and videos follow when the course has them. Only when the owner said to go ahead.";
    const KIND: ToolKind = ToolKind::Write;

    fn label(&self, _args: &Self::Args) -> String {
        "Creating the lessons".into()
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: Self::Args) -> BoxFuture<'a, ToolOutput> {
        let state = self.0.clone();
        let chat_id = ctx.chat_id.clone();
        Box::pin(to_the_end(&self.0, ctx, async move {
            let proposal: Option<Proposal> =
                match state.store.get(db::PROPOSAL, &args.proposal_id).await {
                    Ok(p) => p,
                    Err(e) => return internal(e),
                };
            let Some(proposal) = proposal else {
                return ToolOutput::error("That proposal doesn't exist.");
            };
            if proposal.chat_id.as_deref() != Some(chat_id.as_str()) {
                return ToolOutput::error(
                    "That proposal wasn't made in this chat; propose the course here first.",
                );
            }
            if let ProposalStatus::Superseded { by } = &proposal.status {
                return ToolOutput::error(format!(
                    "A newer outline replaced this one (proposal_id {by}); only the newest outline in this chat can be accepted."
                ));
            }
            let Some(outline) = proposal.outline.clone() else {
                return ToolOutput::error("That proposal has no outline yet.");
            };
            let req = AcceptProposal {
                outline,
                chips: OutlineChips::default(),
            };
            let (topic, accepted) = match ask::accept_proposal(&state, &proposal.id, req).await {
                Ok(t) => t,
                Err(e) => return failed(e),
            };
            let lessons: Vec<Value> = match library::topic_lessons(&state, &topic.id).await {
                Ok(rows) => rows
                    .iter()
                    .map(|l| json!({ "id": l.lesson.id, "title": l.lesson.title }))
                    .collect(),
                Err(e) => return internal(e),
            };
            let mut output = json!({
                "topic_id": topic.id,
                "topic_title": topic.title,
                "parent_id": topic.parent_id,
                "lessons": lessons,
                "level": level_name(accepted.request.level),
                "with_videos": accepted.request.with_videos,
                "with_quiz": accepted.request.with_quiz,
                "status": "The lessons are queued and being written now (a few at a time); their progress shows on the card.",
            });
            // The topic it was proposed under was deleted since: say where it went.
            if proposal.request.parent_topic_id.is_some()
                && accepted.request.parent_topic_id.is_none()
            {
                let subject = accepted
                    .outline
                    .as_ref()
                    .map(|o| o.subject_title.clone())
                    .unwrap_or_default();
                output["note"] = json!(format!(
                    "The topic it was to go under was deleted; filed under the subject “{subject}” instead."
                ));
            }
            let summary = format!(
                "Created {} · {}",
                quoted(&topic.title),
                plural(lessons.len(), "lesson", "lessons")
            );
            ToolOutput::ok(output.to_string(), summary).with_card(result_card(
                ResultTarget::Topic {
                    topic_id: topic.id.clone(),
                },
                &topic.title,
            ))
        }))
    }
}

// ---------------------------------------------------------------------------
// write_lesson
// ---------------------------------------------------------------------------

pub struct WriteLesson(pub AppState);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteLessonArgs {
    /// The topic to add the lesson to (it goes last).
    pub topic_id: String,
    /// The lesson's title.
    pub title: String,
    /// One line on what it covers; the lesson writer follows it.
    pub summary: String,
    /// Estimated reading time in minutes.
    pub minutes: u32,
    /// How advanced the lesson is (usually the level of the topic's other lessons).
    pub level: LevelArg,
    /// Whether the lesson ends with a short "Check yourself" quiz.
    pub with_quiz: bool,
}

impl TypedTool for WriteLesson {
    type Args = WriteLessonArgs;
    const NAME: &'static str = "write_lesson";
    const DESCRIPTION: &'static str = "Add one lesson at the end of an existing topic and have it written now: the lesson writer drafts it from the title and summary, the topic's other lessons, the request the course was made for and the topic's sources. Its progress shows on a card. For a new course with several lessons use propose_course instead. No video is made; use make_video once the lesson is ready.";
    const KIND: ToolKind = ToolKind::Write;

    fn label(&self, args: &Self::Args) -> String {
        format!("Adding the lesson {}", quoted(&args.title))
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: Self::Args) -> BoxFuture<'a, ToolOutput> {
        let state = self.0.clone();
        Box::pin(to_the_end(&self.0, ctx, async move {
            let req = ask::NewLesson {
                topic_id: args.topic_id,
                title: args.title,
                summary: args.summary,
                minutes: args.minutes,
                level: args.level.into(),
                with_quiz: args.with_quiz,
            };
            let lesson = match ask::append_lesson(&state, req).await {
                Ok(l) => l,
                Err(e) => return failed(e),
            };
            let output = json!({
                "id": lesson.id,
                "title": lesson.title,
                "topic_id": lesson.topic_id,
                "position": lesson.position,
                "level": level_name(lesson.level),
                "status": "The lesson is queued and being written now; its progress shows on the card.",
            });
            ToolOutput::ok(
                output.to_string(),
                format!("Added {}", quoted(&lesson.title)),
            )
            .with_card(result_card(
                ResultTarget::Lesson {
                    lesson_id: lesson.id.clone(),
                },
                &lesson.title,
            ))
        }))
    }
}

// ---------------------------------------------------------------------------
// regenerate_lesson
// ---------------------------------------------------------------------------

pub struct RegenerateLesson(pub AppState);

impl TypedTool for RegenerateLesson {
    type Args = LessonArgs;
    const NAME: &'static str = "regenerate_lesson";
    const DESCRIPTION: &'static str = "Write a lesson again from scratch (same title, summary and place in its topic). Its current text stays until the new one is done, and comes back if writing fails. When the owner edited the lesson's text, the app asks them to confirm first; lessons as they were written change right away.";
    const KIND: ToolKind = ToolKind::Write;

    fn label(&self, _args: &Self::Args) -> String {
        "Rewriting a lesson".into()
    }

    fn confirmation<'a>(
        &'a self,
        _ctx: &'a ToolContext,
        args: &'a Self::Args,
    ) -> BoxFuture<'a, Result<Option<ConfirmationPrompt>, String>> {
        Box::pin(async move {
            let row = library::lesson(&self.0, &args.lesson_id)
                .await
                .map_err(|e| e.message)?;
            // An empty body has none of the owner's edits to lose.
            if row.author != Author::Owner || row.lesson.body_md.trim().is_empty() {
                return Ok(None);
            }
            Ok(Some(ConfirmationPrompt {
                title: format!("Rewrite the lesson {}?", quoted(&row.lesson.title)),
                detail: "You edited its text; your edits would be lost.".into(),
                approve_label: "Rewrite".into(),
            }))
        })
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: Self::Args) -> BoxFuture<'a, ToolOutput> {
        let state = self.0.clone();
        Box::pin(to_the_end(&self.0, ctx, async move {
            let lesson = match ask::regenerate_lesson(&state, &args.lesson_id).await {
                Ok(l) => l,
                Err(e) => return failed(e),
            };
            let output = json!({
                "id": lesson.id,
                "title": lesson.title,
                "topic_id": lesson.topic_id,
                "status": "The lesson is queued and being written again now; its progress shows on the card.",
            });
            ToolOutput::ok(
                output.to_string(),
                format!("Rewriting {}", quoted(&lesson.title)),
            )
            .with_card(result_card(
                ResultTarget::Lesson {
                    lesson_id: lesson.id.clone(),
                },
                &lesson.title,
            ))
        }))
    }
}

// ---------------------------------------------------------------------------
// make_video
// ---------------------------------------------------------------------------

pub struct MakeVideo(pub AppState);

impl TypedTool for MakeVideo {
    type Args = LessonArgs;
    const NAME: &'static str = "make_video";
    const DESCRIPTION: &'static str = "Make a narrated explainer video for a lesson that is ready (videos are made one at a time, so it may wait for others first). A lesson that already has a video gets it made again; one already being made is left as it is. Its progress shows on a card.";
    const KIND: ToolKind = ToolKind::Write;

    fn label(&self, _args: &Self::Args) -> String {
        "Making a video".into()
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: Self::Args) -> BoxFuture<'a, ToolOutput> {
        let state = self.0.clone();
        Box::pin(to_the_end(&self.0, ctx, async move {
            let row = match library::lesson(&state, &args.lesson_id).await {
                Ok(l) => l,
                Err(e) => return failed(e),
            };
            if row.lesson.status != LessonStatus::Ready {
                return ToolOutput::error(format!(
                    "The lesson is {}; make a video once it is ready.",
                    lesson_status(&row.lesson.status)
                ));
            }
            if let Err(e) = state.ai_config().await {
                return failed(e);
            }
            let already = match library::video_for_lesson(&state, &args.lesson_id).await {
                Ok(v) => v.is_some_and(|v| video::in_progress(&v.video.status)),
                Err(e) => return internal(e),
            };
            let stored = match video::enqueue(&state, &args.lesson_id).await {
                Ok(v) => v,
                Err(e) => return failed(e),
            };
            let title = row.lesson.title;
            let (status, summary) = if already {
                (
                    "The video was already being made; it carries on.".to_string(),
                    format!("Video of {} already on its way", quoted(&title)),
                )
            } else {
                (
                    "The video is queued and being made now (one at a time); its progress shows on the card.".to_string(),
                    format!("Making the video of {}", quoted(&title)),
                )
            };
            let output = json!({
                "video_id": stored.video.id,
                "lesson_id": args.lesson_id,
                "lesson_title": title,
                "video": video_status(Some(&stored)),
                "status": status,
            });
            ToolOutput::ok(output.to_string(), summary).with_card(result_card(
                ResultTarget::Video {
                    lesson_id: args.lesson_id.clone(),
                },
                &title,
            ))
        }))
    }
}

// ---------------------------------------------------------------------------
// attach_sources
// ---------------------------------------------------------------------------

pub struct AttachSources(pub AppState);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AttachSourcesArgs {
    /// The files to file under the topic: ids of files attached to this chat (from
    /// list_sources, attached_to_this_chat) that aren't in a topic yet.
    pub source_ids: Vec<String>,
    /// The topic they go into.
    pub topic_id: String,
}

impl TypedTool for AttachSources {
    type Args = AttachSourcesArgs;
    const NAME: &'static str = "attach_sources";
    const DESCRIPTION: &'static str = "File files attached to this chat under a topic, so they become the topic's sources: its lessons draw on them, and they stay when the chat is deleted. Only this chat's files that aren't in a topic yet; files already in that topic are left as they are.";
    const KIND: ToolKind = ToolKind::Write;

    fn label(&self, args: &Self::Args) -> String {
        format!(
            "Filing {} under a topic",
            plural(args.source_ids.len(), "file", "files")
        )
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: Self::Args) -> BoxFuture<'a, ToolOutput> {
        let state = self.0.clone();
        let chat_id = ctx.chat_id.clone();
        Box::pin(to_the_end(&self.0, ctx, async move {
            match attach_chat_files(&state, &chat_id, &args.source_ids, &args.topic_id).await {
                Ok(Ok(out)) => out,
                Ok(Err(e)) => failed(e),
                Err(e) => internal(e),
            }
        }))
    }
}

/// `attach_sources`: checks every id is a file of the chat that isn't another topic's,
/// then moves them into the topic (`sources::attach_locked`) under `write_lock` and
/// emits after releasing it. `Ok(Err(..))` is a refusal the model reads; nothing moved.
async fn attach_chat_files(
    state: &AppState,
    chat_id: &str,
    source_ids: &[String],
    topic_id: &str,
) -> anyhow::Result<AppResult<ToolOutput>> {
    let mut ids: Vec<String> = Vec::new();
    for id in source_ids {
        if !ids.contains(id) {
            ids.push(id.clone());
        }
    }
    if ids.is_empty() {
        return Ok(Err(AppError::bad_request(
            "Give the ids of the chat's files to file.",
        )));
    }
    let (topic, moved, already, events) = {
        let _w = state.write_lock.lock().await;
        let topic = match library::topic(state, topic_id).await {
            Ok(t) => t,
            Err(e) => return Ok(Err(e)),
        };
        let chat: Option<Chat> = state.store.get(db::CHAT, chat_id).await?;
        let in_chat: HashSet<String> = chat
            .map(|c| c.source_ids.into_iter().collect())
            .unwrap_or_default();
        let mut moved = Vec::new();
        let mut already = Vec::new();
        for id in &ids {
            let Some(source) = state.store.get::<Source>(db::SOURCE, id).await? else {
                return Ok(Err(AppError::bad_request(format!(
                    "There is no source {id}. Use list_sources to see the chat's files."
                ))));
            };
            if !in_chat.contains(id) {
                return Ok(Err(AppError::bad_request(format!(
                    "{} isn't attached to this chat; only the chat's files can be filed this way.",
                    quoted(&source.name)
                ))));
            }
            match source.topic_id.as_deref() {
                None => moved.push(source),
                Some(t) if t == topic.id => already.push(source),
                Some(_) => {
                    return Ok(Err(AppError::bad_request(format!(
                        "{} already belongs to another topic.",
                        quoted(&source.name)
                    ))));
                }
            }
        }
        let move_ids: Vec<String> = moved.iter().map(|s| s.id.clone()).collect();
        let events = sources::attach_locked(state, &move_ids, &topic.id).await?;
        (topic, moved, already, events)
    };
    for e in events {
        state.emit(e);
    }
    let names = |list: &[Source]| -> Vec<Value> {
        list.iter()
            .map(|s| json!({ "id": s.id, "name": s.name }))
            .collect()
    };
    let output = json!({
        "topic_id": topic.id,
        "topic_title": topic.title,
        "filed": names(&moved),
        "already_there": names(&already),
    });
    let summary = if moved.is_empty() {
        format!("Already in {}", quoted(&topic.title))
    } else {
        format!(
            "Filed {} under {}",
            plural(moved.len(), "file", "files"),
            quoted(&topic.title)
        )
    };
    Ok(Ok(ToolOutput::ok(output.to_string(), summary)))
}

// ---------------------------------------------------------------------------
// delete_topic, delete_lesson, delete_note
// ---------------------------------------------------------------------------

/// What deleting a topic takes with it.
struct TopicContents {
    title: String,
    subtopics: usize,
    lessons: usize,
    notes: usize,
    videos: usize,
    sources: usize,
}

impl TopicContents {
    async fn load(state: &AppState, topic_id: &str) -> Result<Self, String> {
        let topic = library::topic(state, topic_id)
            .await
            .map_err(|e| e.message)?;
        let counted = async {
            let lib = Snapshot::load(state).await?;
            let videos: Vec<StoredVideo> = state.store.list(db::VIDEO).await?;
            let all_sources: Vec<Source> = state.store.list(db::SOURCE).await?;
            let within = subtree(&lib.topics, topic_id);
            let lesson_ids: HashSet<&str> = lib
                .lessons
                .iter()
                .filter(|l| within.contains(&l.lesson.topic_id))
                .map(|l| l.lesson.id.as_str())
                .collect();
            anyhow::Ok(Self {
                title: topic.title.clone(),
                subtopics: within.len() - 1,
                lessons: lesson_ids.len(),
                notes: lib
                    .notes
                    .iter()
                    .filter(|n| within.contains(&n.topic_id))
                    .count(),
                videos: videos
                    .iter()
                    .filter(|v| lesson_ids.contains(v.video.lesson_id.as_str()))
                    .count(),
                sources: all_sources
                    .iter()
                    .filter(|s| s.topic_id.as_ref().is_some_and(|t| within.contains(t)))
                    .count(),
            })
        };
        counted.await.map_err(|e| {
            tracing::error!("couldn't count a topic's contents: {e:#}");
            "The service couldn't read the library just now.".to_string()
        })
    }

    fn parts(&self) -> Vec<String> {
        let mut parts = Vec::new();
        if self.subtopics > 0 {
            parts.push(plural(self.subtopics, "topic under it", "topics under it"));
        }
        for (n, one, many) in [
            (self.lessons, "lesson", "lessons"),
            (self.notes, "note", "notes"),
            (self.videos, "video", "videos"),
            (self.sources, "source", "sources"),
        ] {
            if n > 0 {
                parts.push(plural(n, one, many));
            }
        }
        parts
    }
}

pub struct DeleteTopic(pub AppState);

impl TypedTool for DeleteTopic {
    type Args = TopicArgs;
    const NAME: &'static str = "delete_topic";
    const DESCRIPTION: &'static str = "Delete a topic with everything in it: the topics under it, their lessons, notes, videos and sources. The owner confirms in the app first; just call it.";
    const KIND: ToolKind = ToolKind::Destructive;

    fn label(&self, _args: &Self::Args) -> String {
        "Deleting a topic".into()
    }

    fn confirmation<'a>(
        &'a self,
        _ctx: &'a ToolContext,
        args: &'a Self::Args,
    ) -> BoxFuture<'a, Result<Option<ConfirmationPrompt>, String>> {
        Box::pin(async move {
            let contents = TopicContents::load(&self.0, &args.topic_id).await?;
            let parts = contents.parts();
            let detail = if parts.is_empty() {
                "It's empty.".to_string()
            } else {
                go_with_it(&parts)
            };
            Ok(Some(ConfirmationPrompt {
                title: format!("Delete the topic {}?", quoted(&contents.title)),
                detail,
                approve_label: "Delete".into(),
            }))
        })
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: Self::Args) -> BoxFuture<'a, ToolOutput> {
        let state = self.0.clone();
        Box::pin(to_the_end(&self.0, ctx, async move {
            let contents = match TopicContents::load(&state, &args.topic_id).await {
                Ok(c) => c,
                Err(message) => return ToolOutput::error(message),
            };
            if let Err(e) = library::remove_topic(&state, &args.topic_id).await {
                return failed(e);
            }
            let output = json!({
                "deleted": "topic",
                "id": args.topic_id,
                "title": contents.title,
                "with_it": {
                    "topics": contents.subtopics,
                    "lessons": contents.lessons,
                    "notes": contents.notes,
                    "videos": contents.videos,
                    "sources": contents.sources,
                },
            });
            ToolOutput::ok(
                output.to_string(),
                format!("Deleted {}", quoted(&contents.title)),
            )
        }))
    }
}

pub struct DeleteLesson(pub AppState);

impl TypedTool for DeleteLesson {
    type Args = LessonArgs;
    const NAME: &'static str = "delete_lesson";
    const DESCRIPTION: &'static str = "Delete a lesson with its notes and its video. The owner confirms in the app first; just call it.";
    const KIND: ToolKind = ToolKind::Destructive;

    fn label(&self, _args: &Self::Args) -> String {
        "Deleting a lesson".into()
    }

    fn confirmation<'a>(
        &'a self,
        _ctx: &'a ToolContext,
        args: &'a Self::Args,
    ) -> BoxFuture<'a, Result<Option<ConfirmationPrompt>, String>> {
        Box::pin(async move {
            let state = &self.0;
            let row = library::lesson(state, &args.lesson_id)
                .await
                .map_err(|e| e.message)?;
            let counted = async {
                let notes: Vec<Note> = state
                    .store
                    .list_where(db::NOTE, "lesson_id", &args.lesson_id)
                    .await?;
                let video = library::video_for_lesson(state, &args.lesson_id).await?;
                anyhow::Ok((notes.len(), video.is_some()))
            };
            let (notes, video) = counted.await.map_err(|e| {
                tracing::error!("couldn't count a lesson's notes: {e:#}");
                "The service couldn't read the library just now.".to_string()
            })?;
            let mut parts = Vec::new();
            if notes > 0 {
                parts.push(plural(notes, "note", "notes"));
            }
            if video {
                parts.push("its video".to_string());
            }
            let detail = if parts.is_empty() {
                "Nothing else goes with it.".to_string()
            } else {
                go_with_it(&parts)
            };
            Ok(Some(ConfirmationPrompt {
                title: format!("Delete the lesson {}?", quoted(&row.lesson.title)),
                detail,
                approve_label: "Delete".into(),
            }))
        })
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: Self::Args) -> BoxFuture<'a, ToolOutput> {
        let state = self.0.clone();
        Box::pin(to_the_end(&self.0, ctx, async move {
            let row = match library::lesson(&state, &args.lesson_id).await {
                Ok(l) => l,
                Err(e) => return failed(e),
            };
            if let Err(e) = library::remove_lesson(&state, &args.lesson_id).await {
                return failed(e);
            }
            let output = json!({
                "deleted": "lesson",
                "id": args.lesson_id,
                "title": row.lesson.title,
                "topic_id": row.lesson.topic_id,
            });
            ToolOutput::ok(
                output.to_string(),
                format!("Deleted {}", quoted(&row.lesson.title)),
            )
        }))
    }
}

pub struct DeleteNote(pub AppState);

impl TypedTool for DeleteNote {
    type Args = NoteArgs;
    const NAME: &'static str = "delete_note";
    const DESCRIPTION: &'static str =
        "Delete a note. The owner confirms in the app first; just call it.";
    const KIND: ToolKind = ToolKind::Destructive;

    fn label(&self, _args: &Self::Args) -> String {
        "Deleting a note".into()
    }

    fn confirmation<'a>(
        &'a self,
        _ctx: &'a ToolContext,
        args: &'a Self::Args,
    ) -> BoxFuture<'a, Result<Option<ConfirmationPrompt>, String>> {
        Box::pin(async move {
            let row = library::stored_note(&self.0, &args.note_id)
                .await
                .map_err(|e| e.message)?;
            let who = match row.author {
                Author::Owner => "You wrote it",
                Author::Agent => "The assistant wrote it",
            };
            let detail = if row.note.body_md.trim().is_empty() {
                format!("{who}; it's empty.")
            } else {
                format!("{who}. It begins “{}”.", excerpt(&row.note.body_md, 80))
            };
            Ok(Some(ConfirmationPrompt {
                title: format!("Delete the note {}?", quoted(note_title(&row.note))),
                detail,
                approve_label: "Delete".into(),
            }))
        })
    }

    fn run<'a>(&'a self, ctx: &'a ToolContext, args: Self::Args) -> BoxFuture<'a, ToolOutput> {
        let state = self.0.clone();
        Box::pin(to_the_end(&self.0, ctx, async move {
            let note = match library::note(&state, &args.note_id).await {
                Ok(n) => n,
                Err(e) => return failed(e),
            };
            if let Err(e) = library::remove_note(&state, &args.note_id).await {
                return failed(e);
            }
            let title = note_title(&note).to_string();
            let output = json!({
                "deleted": "note",
                "id": args.note_id,
                "title": title,
            });
            ToolOutput::ok(output.to_string(), format!("Deleted {}", quoted(&title)))
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn t(id: &str, parent: Option<&str>) -> Topic {
        Topic {
            id: id.into(),
            parent_id: parent.map(Into::into),
            title: id.to_uppercase(),
            position: 0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn subtrees_and_paths() {
        let all = vec![
            t("a", None),
            t("b", Some("a")),
            t("c", Some("b")),
            t("d", None),
        ];
        let mut s: Vec<_> = subtree(&all, "a").into_iter().collect();
        s.sort();
        assert_eq!(s, vec!["a", "b", "c"]);
        let by_id: HashMap<&str, &Topic> = all.iter().map(|t| (t.id.as_str(), t)).collect();
        assert_eq!(topic_path(&by_id, "c"), "A / B / C");
        assert_eq!(topic_path(&by_id, "gone"), "");
    }

    #[test]
    fn lists_and_excerpts() {
        let parts = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(and_list(&parts(&[])), "");
        assert_eq!(and_list(&parts(&["2 lessons"])), "2 lessons");
        assert_eq!(
            and_list(&parts(&["1 topic under it", "2 lessons", "1 note"])),
            "1 topic under it, 2 lessons and 1 note"
        );
        assert_eq!(excerpt("a\n\n  b  c", 10), "a b c");
        assert_eq!(excerpt("abcdef", 3), "abc…");
        assert_eq!(go_with_it(&parts(&["1 note"])), "1 note goes with it.");
        assert_eq!(
            go_with_it(&parts(&["its video"])),
            "its video goes with it."
        );
        assert_eq!(go_with_it(&parts(&["2 notes"])), "2 notes go with it.");
        assert_eq!(
            go_with_it(&parts(&["1 note", "its video"])),
            "1 note and its video go with it."
        );
    }

    #[test]
    fn write_tool_arguments_are_strict() {
        for schema in [
            margin_agent::strict_schema::<CreateTopicArgs>(),
            margin_agent::strict_schema::<RenameTopicArgs>(),
            margin_agent::strict_schema::<MoveTopicArgs>(),
            margin_agent::strict_schema::<EditLessonTitleArgs>(),
            margin_agent::strict_schema::<CreateNoteArgs>(),
            margin_agent::strict_schema::<AppendToNoteArgs>(),
            margin_agent::strict_schema::<ReplaceNoteBodyArgs>(),
            margin_agent::strict_schema::<ProposeCourseArgs>(),
            margin_agent::strict_schema::<AcceptProposalArgs>(),
            margin_agent::strict_schema::<WriteLessonArgs>(),
            margin_agent::strict_schema::<LessonArgs>(),
            margin_agent::strict_schema::<AttachSourcesArgs>(),
        ] {
            margin_agent::check_strict_schema(&schema).unwrap_or_else(|e| panic!("{e}: {schema}"));
        }
        let schema = margin_agent::strict_schema::<ProposeCourseArgs>();
        assert_eq!(
            schema["properties"]["level"]["enum"],
            json!(["intro", "intermediate", "advanced"])
        );
        assert_eq!(
            schema["properties"]["lessons"]["items"]["required"],
            json!(["minutes", "summary", "title"])
        );
    }

    #[test]
    fn labels_quote_and_cut() {
        assert_eq!(quoted(" fourier "), "“fourier”");
        assert!(quoted(&"x".repeat(100)).ends_with("…”"));
        assert_eq!(plural(1, "note", "notes"), "1 note");
        assert_eq!(plural(3, "note", "notes"), "3 notes");
    }
}
