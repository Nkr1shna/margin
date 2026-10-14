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
//!   edited by the owner), `make_video` (result cards).
//!
//! Outputs are compact JSON the model reads; library text (lesson bodies, notes, source
//! passages) only ever reaches the model this way, never in the developer prompt.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use margin_agent::{BoxFuture, ToolContext, ToolKind, ToolOutput, ToolRegistry, Typed, TypedTool};
use margin_shared::{
    Card, Chat, Citation, LessonStatus, Note, PassagesCard, Source, SourceStatus, Topic,
    VideoStatus,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::library::{self, sort_topics};
use crate::state::{AppState, StoredLesson, StoredVideo};
use crate::{db, embedding, search, sources};

/// How long `search_sources` waits for sources still being read before searching what
/// is ready.
pub const SOURCE_SEARCH_WAIT: Duration = Duration::from_secs(30);

/// Every tool, service tools first then `margin_agent::device_tools()` (from phase 3),
/// in a fixed order so the `tools` array is identical on every call.
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
        let all = vec![t("a", None), t("b", Some("a")), t("c", Some("b")), t("d", None)];
        let mut s: Vec<_> = subtree(&all, "a").into_iter().collect();
        s.sort();
        assert_eq!(s, vec!["a", "b", "c"]);
        let by_id: HashMap<&str, &Topic> = all.iter().map(|t| (t.id.as_str(), t)).collect();
        assert_eq!(topic_path(&by_id, "c"), "A / B / C");
        assert_eq!(topic_path(&by_id, "gone"), "");
    }

    #[test]
    fn labels_quote_and_cut() {
        assert_eq!(quoted(" fourier "), "“fourier”");
        assert!(quoted(&"x".repeat(100)).ends_with("…”"));
        assert_eq!(plural(1, "note", "notes"), "1 note");
        assert_eq!(plural(3, "note", "notes"), "3 notes");
    }
}
