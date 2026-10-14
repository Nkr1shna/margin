//! Topics, lessons and notes: the library and its edits, with cascading deletes (a
//! topic takes its sources with it).

use std::collections::{HashMap, HashSet};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::Utc;
use margin_shared::{
    Event, Id, Lesson, LessonPatch, LessonStatus, Library, NewNote, NewTopic, Note, NotePatch,
    Source, Topic, TopicPatch,
};

use crate::error::{ApiJson, AppError, AppResult};
use crate::state::{AppState, Author, StoredLesson, StoredNote, StoredVideo, new_id};
use crate::{db, sources};

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

pub async fn topic(state: &AppState, id: &str) -> AppResult<Topic> {
    state
        .store
        .get(db::TOPIC, id)
        .await?
        .ok_or_else(|| AppError::not_found("topic"))
}

pub async fn lesson(state: &AppState, id: &str) -> AppResult<StoredLesson> {
    state
        .store
        .get(db::LESSON, id)
        .await?
        .ok_or_else(|| AppError::not_found("lesson"))
}

pub async fn note(state: &AppState, id: &str) -> AppResult<Note> {
    state
        .store
        .get(db::NOTE, id)
        .await?
        .ok_or_else(|| AppError::not_found("note"))
}

/// The note with who wrote it (owner decision Q2).
pub async fn stored_note(state: &AppState, id: &str) -> AppResult<StoredNote> {
    state
        .store
        .get(db::NOTE, id)
        .await?
        .ok_or_else(|| AppError::not_found("note"))
}

pub async fn video_for_lesson(
    state: &AppState,
    lesson_id: &str,
) -> anyhow::Result<Option<StoredVideo>> {
    let videos: Vec<StoredVideo> = state
        .store
        .list_where(db::VIDEO, "lesson_id", lesson_id)
        .await?;
    Ok(videos.into_iter().next())
}

pub fn sort_topics(topics: &mut [Topic]) {
    topics.sort_by(|a, b| {
        a.parent_id
            .cmp(&b.parent_id)
            .then(a.position.cmp(&b.position))
            .then(a.created_at.cmp(&b.created_at))
    });
}

pub fn sort_lessons(lessons: &mut [Lesson]) {
    lessons.sort_by(|a, b| {
        a.topic_id
            .cmp(&b.topic_id)
            .then(a.position.cmp(&b.position))
            .then(a.created_at.cmp(&b.created_at))
    });
}

/// Lessons of one topic in reading order.
pub async fn topic_lessons(state: &AppState, topic_id: &str) -> anyhow::Result<Vec<StoredLesson>> {
    let mut rows: Vec<StoredLesson> = state
        .store
        .list_where(db::LESSON, "topic_id", topic_id)
        .await?;
    rows.sort_by(|a, b| {
        a.lesson
            .position
            .cmp(&b.lesson.position)
            .then(a.lesson.created_at.cmp(&b.lesson.created_at))
    });
    Ok(rows)
}

/// Position after the last sibling topic (`parent_id` siblings).
async fn next_topic_position(state: &AppState, parent_id: Option<&str>) -> anyhow::Result<i64> {
    let topics: Vec<Topic> = state.store.list(db::TOPIC).await?;
    Ok(topics
        .iter()
        .filter(|t| t.parent_id.as_deref() == parent_id)
        .map(|t| t.position + 1)
        .max()
        .unwrap_or(0))
}

/// Creates a topic last among its siblings. Call with the write lock held.
pub async fn create_topic_locked(
    state: &AppState,
    parent_id: Option<Id>,
    title: &str,
) -> AppResult<Topic> {
    let now = Utc::now();
    let topic = Topic {
        id: new_id(),
        position: next_topic_position(state, parent_id.as_deref()).await?,
        parent_id,
        title: title.to_string(),
        created_at: now,
        updated_at: now,
    };
    state.store.put(db::TOPIC, &topic.id, &topic).await?;
    Ok(topic)
}

pub async fn get_library(State(state): State<AppState>) -> AppResult<Json<Library>> {
    let mut topics: Vec<Topic> = state.store.list(db::TOPIC).await?;
    sort_topics(&mut topics);
    let mut lessons: Vec<Lesson> = state
        .store
        .list::<StoredLesson>(db::LESSON)
        .await?
        .into_iter()
        .map(|l| state.public_lesson(&l.lesson))
        .collect();
    sort_lessons(&mut lessons);
    let mut notes: Vec<Note> = state.store.list(db::NOTE).await?;
    notes.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    let mut videos: Vec<StoredVideo> = state.store.list(db::VIDEO).await?;
    videos.sort_by_key(|v| v.video.created_at);
    let videos = videos.iter().map(|v| state.public_video(v)).collect();
    let mut sources: Vec<Source> = state.store.list(db::SOURCE).await?;
    sources.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    let sources = sources.iter().map(|s| state.public_source(s)).collect();
    Ok(Json(Library {
        topics,
        lessons,
        notes,
        videos,
        sources,
    }))
}

pub fn clean_title(title: &str, what: &str) -> AppResult<String> {
    let t = title.trim();
    if t.is_empty() {
        return Err(AppError::bad_request(format!("Give the {what} a title.")));
    }
    Ok(t.chars().take(300).collect())
}

// ---------------------------------------------------------------------------
// Topics
// ---------------------------------------------------------------------------

pub async fn create_topic(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<NewTopic>,
) -> AppResult<Json<Topic>> {
    Ok(Json(new_topic(&state, req).await?))
}

/// Creates a topic last among its siblings and emits it (`POST /api/topics` and the
/// agent's `create_topic`).
pub async fn new_topic(state: &AppState, req: NewTopic) -> AppResult<Topic> {
    let title = clean_title(&req.title, "topic")?;
    let topic = {
        let _w = state.write_lock.lock().await;
        if let Some(parent) = &req.parent_id {
            topic(state, parent).await?;
        }
        create_topic_locked(state, req.parent_id.clone(), &title).await?
    };
    state.emit_topic(&topic);
    Ok(topic)
}

/// `id` and every topic under it.
pub fn subtree(all: &[Topic], id: &str) -> Vec<Id> {
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for t in all {
        if let Some(p) = &t.parent_id {
            children.entry(p.as_str()).or_default().push(t.id.as_str());
        }
    }
    let mut out = vec![id.to_string()];
    let mut i = 0;
    while i < out.len() {
        if let Some(kids) = children.get(out[i].as_str()) {
            out.extend(kids.iter().map(|k| k.to_string()));
        }
        i += 1;
    }
    out
}

pub async fn patch_topic(
    State(state): State<AppState>,
    Path(id): Path<Id>,
    ApiJson(patch): ApiJson<TopicPatch>,
) -> AppResult<Json<Topic>> {
    Ok(Json(update_topic(&state, &id, patch).await?))
}

/// Renames and moves a topic and emits it (`PATCH /api/topics/{id}` and the agent's
/// `rename_topic`).
pub async fn update_topic(state: &AppState, id: &str, patch: TopicPatch) -> AppResult<Topic> {
    let topic = {
        let _w = state.write_lock.lock().await;
        let mut topic = topic(state, id).await?;
        if let Some(title) = &patch.title {
            topic.title = clean_title(title, "topic")?;
        }
        if let Some(new_parent) = &patch.parent_id
            && *new_parent != topic.parent_id
        {
            if let Some(p) = new_parent {
                let all: Vec<Topic> = state.store.list(db::TOPIC).await?;
                if subtree(&all, id).iter().any(|t| t == p) {
                    return Err(AppError::bad_request("A topic can't move inside itself."));
                }
                if !all.iter().any(|t| &t.id == p) {
                    return Err(AppError::not_found("parent topic"));
                }
            }
            topic.parent_id = new_parent.clone();
            if patch.position.is_none() {
                topic.position = next_topic_position(state, topic.parent_id.as_deref()).await?;
            }
        }
        if let Some(pos) = patch.position {
            topic.position = pos;
        }
        topic.updated_at = Utc::now();
        state.store.put(db::TOPIC, &topic.id, &topic).await?;
        topic
    };
    state.emit_topic(&topic);
    Ok(topic)
}

/// Moves a topic under `parent_id` (`None`: the top level) at `index` among its new
/// siblings (0 first; `None` or past the end: last), renumbering the siblings'
/// positions 0, 1, 2… The agent's `move_topic`. Emits every topic that changed and
/// returns the moved one.
pub async fn place_topic(
    state: &AppState,
    id: &str,
    parent_id: Option<Id>,
    index: Option<usize>,
) -> AppResult<Topic> {
    let changed = {
        let _w = state.write_lock.lock().await;
        let mut moved = topic(state, id).await?;
        let all: Vec<Topic> = state.store.list(db::TOPIC).await?;
        if let Some(p) = &parent_id {
            if subtree(&all, id).iter().any(|t| t == p) {
                return Err(AppError::bad_request("A topic can't move inside itself."));
            }
            if !all.iter().any(|t| &t.id == p) {
                return Err(AppError::not_found("parent topic"));
            }
        }
        let mut siblings: Vec<Topic> = all
            .into_iter()
            .filter(|t| t.parent_id == parent_id && t.id != moved.id)
            .collect();
        sort_topics(&mut siblings);
        moved.parent_id = parent_id;
        let at = index.unwrap_or(siblings.len()).min(siblings.len());
        siblings.insert(at, moved.clone());
        let now = Utc::now();
        let mut changed = Vec::new();
        for (i, mut t) in siblings.into_iter().enumerate() {
            let position = i as i64;
            let is_moved = t.id == moved.id;
            if t.position == position && !is_moved {
                continue;
            }
            t.position = position;
            t.updated_at = now;
            state.store.put(db::TOPIC, &t.id, &t).await?;
            changed.push(t);
        }
        changed
    };
    let mut moved = None;
    for t in changed {
        state.emit_topic(&t);
        if t.id == id {
            moved = Some(t);
        }
    }
    moved.ok_or_else(|| AppError::not_found("topic"))
}

pub async fn delete_topic(
    State(state): State<AppState>,
    Path(id): Path<Id>,
) -> AppResult<StatusCode> {
    remove_topic(&state, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes a topic with everything under it (topics, lessons, notes, videos, sources)
/// and emits the deletions (`DELETE /api/topics/{id}` and the agent's `delete_topic`).
pub async fn remove_topic(state: &AppState, id: &str) -> AppResult<()> {
    let events = {
        let _w = state.write_lock.lock().await;
        topic(state, id).await?;
        let all: Vec<Topic> = state.store.list(db::TOPIC).await?;
        let doomed = subtree(&all, id);
        let doomed_set: HashSet<&str> = doomed.iter().map(String::as_str).collect();
        let mut events = Vec::new();

        let lessons: Vec<StoredLesson> = state.store.list(db::LESSON).await?;
        for l in lessons
            .iter()
            .filter(|l| doomed_set.contains(l.lesson.topic_id.as_str()))
        {
            events.extend(delete_lesson_locked(state, &l.lesson.id).await?);
        }
        let notes: Vec<Note> = state.store.list(db::NOTE).await?;
        for n in notes
            .iter()
            .filter(|n| doomed_set.contains(n.topic_id.as_str()))
        {
            state.store.delete(db::NOTE, &n.id).await?;
            events.push(Event::NoteDeleted { id: n.id.clone() });
        }
        let sources: Vec<Source> = state.store.list(db::SOURCE).await?;
        for s in sources.iter().filter(|s| {
            s.topic_id
                .as_deref()
                .is_some_and(|t| doomed_set.contains(t))
        }) {
            events.push(sources::delete_locked(state, &s.id).await?);
        }
        // Children before parents.
        for t in doomed.iter().rev() {
            state.store.delete(db::TOPIC, t).await?;
            events.push(Event::TopicDeleted { id: t.clone() });
        }
        events
    };
    for e in events {
        state.emit(e);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Lessons
// ---------------------------------------------------------------------------

pub fn is_busy(status: &LessonStatus) -> bool {
    matches!(status, LessonStatus::Queued | LessonStatus::Generating)
}

pub async fn patch_lesson(
    State(state): State<AppState>,
    Path(id): Path<Id>,
    ApiJson(patch): ApiJson<LessonPatch>,
) -> AppResult<Json<Lesson>> {
    // An edit through the API is the owner's (owner decision Q2).
    Ok(Json(
        update_lesson(&state, &id, patch, Author::Owner).await?,
    ))
}

/// Edits a lesson and emits it (`PATCH /api/lessons/{id}` and the agent's
/// `edit_lesson_title`). A changed body becomes `by`'s writing.
pub async fn update_lesson(
    state: &AppState,
    id: &str,
    patch: LessonPatch,
    by: Author,
) -> AppResult<Lesson> {
    let lesson = {
        let _w = state.write_lock.lock().await;
        let mut row = lesson(state, id).await?;
        if let Some(title) = &patch.title {
            row.lesson.title = clean_title(title, "lesson")?;
        }
        if let Some(body) = &patch.body_md {
            if is_busy(&row.lesson.status) {
                return Err(AppError::conflict(
                    "lesson_busy",
                    "This lesson is being written. Edit it when it's ready.",
                ));
            }
            if *body != row.lesson.body_md {
                row.lesson.body_md = body.clone();
                row.author = by;
            }
        }
        if let Some(pos) = patch.position {
            row.lesson.position = pos;
        }
        row.lesson.updated_at = Utc::now();
        state.store.put(db::LESSON, id, &row).await?;
        row.lesson
    };
    state.emit_lesson(&lesson);
    Ok(state.public_lesson(&lesson))
}

pub async fn delete_lesson(
    State(state): State<AppState>,
    Path(id): Path<Id>,
) -> AppResult<StatusCode> {
    remove_lesson(&state, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes a lesson with its notes and video and emits the deletions
/// (`DELETE /api/lessons/{id}` and the agent's `delete_lesson`).
pub async fn remove_lesson(state: &AppState, id: &str) -> AppResult<()> {
    let events = {
        let _w = state.write_lock.lock().await;
        lesson(state, id).await?;
        delete_lesson_locked(state, id).await?
    };
    for e in events {
        state.emit(e);
    }
    Ok(())
}

/// Deletes a lesson, its notes and its video (files included), stopping its jobs.
/// Call with the write lock held; returns the events to emit.
async fn delete_lesson_locked(state: &AppState, lesson_id: &str) -> AppResult<Vec<Event>> {
    let mut events = Vec::new();
    if let Some(job) = state.lesson_jobs.lock().unwrap().remove(lesson_id) {
        job.abort();
    }
    if let Some(video) = video_for_lesson(state, lesson_id).await? {
        events.push(delete_video_locked(state, &video.video.id).await?);
    }
    let notes: Vec<Note> = state
        .store
        .list_where(db::NOTE, "lesson_id", lesson_id)
        .await?;
    for n in notes {
        state.store.delete(db::NOTE, &n.id).await?;
        events.push(Event::NoteDeleted { id: n.id });
    }
    state.store.delete(db::LESSON, lesson_id).await?;
    events.push(Event::LessonDeleted {
        id: lesson_id.to_string(),
    });
    Ok(events)
}

/// Stops a running render of the video, removes its folder and its row.
async fn delete_video_locked(state: &AppState, video_id: &str) -> AppResult<Event> {
    {
        let mut current = state.render_job.lock().unwrap();
        if current.as_ref().is_some_and(|(id, _)| id == video_id)
            && let Some((_, handle)) = current.take()
        {
            handle.abort();
        }
    }
    let dir = state.video_dir(video_id);
    match tokio::fs::remove_dir_all(&dir).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("couldn't remove {}: {e}", dir.display()),
    }
    state.store.delete(db::VIDEO, video_id).await?;
    Ok(Event::VideoDeleted {
        id: video_id.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Notes
// ---------------------------------------------------------------------------

pub async fn create_note(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<NewNote>,
) -> AppResult<Json<Note>> {
    // A note made through the API is the owner's (owner decision Q2).
    Ok(Json(new_note(&state, req, Author::Owner).await?))
}

/// Creates a note written by `by` and emits it (`POST /api/notes` and the agent's
/// `create_note`).
pub async fn new_note(state: &AppState, req: NewNote, by: Author) -> AppResult<Note> {
    let note = {
        let _w = state.write_lock.lock().await;
        topic(state, &req.topic_id).await?;
        if let Some(lesson_id) = &req.lesson_id {
            let l = lesson(state, lesson_id).await?;
            if l.lesson.topic_id != req.topic_id {
                return Err(AppError::bad_request(
                    "That lesson belongs to a different topic.",
                ));
            }
        }
        let now = Utc::now();
        let row = StoredNote {
            note: Note {
                id: new_id(),
                topic_id: req.topic_id,
                lesson_id: req.lesson_id,
                title: req.title.trim().to_string(),
                body_md: req.body_md,
                quote: req.quote.filter(|q| !q.trim().is_empty()),
                created_at: now,
                updated_at: now,
            },
            author: by,
        };
        state.store.put(db::NOTE, &row.note.id, &row).await?;
        row.note
    };
    state.emit(Event::NoteChanged { note: note.clone() });
    Ok(note)
}

pub async fn patch_note(
    State(state): State<AppState>,
    Path(id): Path<Id>,
    ApiJson(patch): ApiJson<NotePatch>,
) -> AppResult<Json<Note>> {
    // An edit through the API is the owner's (owner decision Q2).
    Ok(Json(update_note(&state, &id, patch, Author::Owner).await?))
}

/// Edits a note and emits it (`PATCH /api/notes/{id}` and the agent's note tools). A
/// changed body becomes `by`'s writing; a retitle alone keeps who wrote the body.
pub async fn update_note(
    state: &AppState,
    id: &str,
    patch: NotePatch,
    by: Author,
) -> AppResult<Note> {
    let note = {
        let _w = state.write_lock.lock().await;
        let mut row = stored_note(state, id).await?;
        if let Some(title) = patch.title {
            row.note.title = title.trim().to_string();
        }
        if let Some(body) = patch.body_md
            && body != row.note.body_md
        {
            row.note.body_md = body;
            row.author = by;
        }
        row.note.updated_at = Utc::now();
        state.store.put(db::NOTE, id, &row).await?;
        row.note
    };
    state.emit(Event::NoteChanged { note: note.clone() });
    Ok(note)
}

/// Adds `text` at the end of a note's body, after a blank line, and emits it (the
/// agent's `append_to_note`). Who wrote the note doesn't change: appending keeps what
/// is there (owner decision Q2).
pub async fn append_to_note(state: &AppState, id: &str, text: &str) -> AppResult<Note> {
    let note = {
        let _w = state.write_lock.lock().await;
        let mut row = stored_note(state, id).await?;
        let body = &mut row.note.body_md;
        if !body.trim().is_empty() {
            while !body.ends_with("\n\n") {
                body.push('\n');
            }
        }
        body.push_str(text);
        row.note.updated_at = Utc::now();
        state.store.put(db::NOTE, id, &row).await?;
        row.note
    };
    state.emit(Event::NoteChanged { note: note.clone() });
    Ok(note)
}

pub async fn delete_note(
    State(state): State<AppState>,
    Path(id): Path<Id>,
) -> AppResult<StatusCode> {
    remove_note(&state, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes a note and emits it (`DELETE /api/notes/{id}` and the agent's
/// `delete_note`).
pub async fn remove_note(state: &AppState, id: &str) -> AppResult<()> {
    {
        let _w = state.write_lock.lock().await;
        note(state, id).await?;
        state.store.delete(db::NOTE, id).await?;
    }
    state.emit(Event::NoteDeleted { id: id.to_string() });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: &str, parent: Option<&str>) -> Topic {
        Topic {
            id: id.into(),
            parent_id: parent.map(Into::into),
            title: id.into(),
            position: 0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn subtree_collects_descendants() {
        let all = vec![
            t("a", None),
            t("b", Some("a")),
            t("c", Some("b")),
            t("d", None),
        ];
        let mut s = subtree(&all, "a");
        s.sort();
        assert_eq!(s, vec!["a", "b", "c"]);
        assert_eq!(subtree(&all, "d"), vec!["d"]);
    }
}
