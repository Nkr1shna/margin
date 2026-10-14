//! Topics, lessons and notes: the library and its edits, with cascading deletes.

use std::collections::{HashMap, HashSet};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::Utc;
use margin_shared::{
    Event, Id, Lesson, LessonPatch, LessonStatus, Library, NewNote, NewTopic, Note, NotePatch,
    Topic, TopicPatch,
};

use crate::db;
use crate::error::{ApiJson, AppError, AppResult};
use crate::state::{AppState, StoredLesson, StoredVideo, new_id};

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
        .map(|l| l.lesson)
        .collect();
    sort_lessons(&mut lessons);
    let mut notes: Vec<Note> = state.store.list(db::NOTE).await?;
    notes.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    let mut videos: Vec<StoredVideo> = state.store.list(db::VIDEO).await?;
    videos.sort_by_key(|v| v.video.created_at);
    let videos = videos.iter().map(|v| state.public_video(v)).collect();
    Ok(Json(Library {
        topics,
        lessons,
        notes,
        videos,
        sources: Vec::new(),
    }))
}

fn clean_title(title: &str, what: &str) -> AppResult<String> {
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
    let title = clean_title(&req.title, "topic")?;
    let topic = {
        let _w = state.write_lock.lock().await;
        if let Some(parent) = &req.parent_id {
            topic(&state, parent).await?;
        }
        create_topic_locked(&state, req.parent_id.clone(), &title).await?
    };
    state.emit_topic(&topic);
    Ok(Json(topic))
}

/// `id` and every topic under it.
fn subtree(all: &[Topic], id: &str) -> Vec<Id> {
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
    let topic = {
        let _w = state.write_lock.lock().await;
        let mut topic = topic(&state, &id).await?;
        if let Some(title) = &patch.title {
            topic.title = clean_title(title, "topic")?;
        }
        if let Some(new_parent) = &patch.parent_id
            && *new_parent != topic.parent_id
        {
            if let Some(p) = new_parent {
                let all: Vec<Topic> = state.store.list(db::TOPIC).await?;
                if subtree(&all, &id).iter().any(|t| t == p) {
                    return Err(AppError::bad_request("A topic can't move inside itself."));
                }
                if !all.iter().any(|t| &t.id == p) {
                    return Err(AppError::not_found("parent topic"));
                }
            }
            topic.parent_id = new_parent.clone();
            if patch.position.is_none() {
                topic.position = next_topic_position(&state, topic.parent_id.as_deref()).await?;
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
    Ok(Json(topic))
}

pub async fn delete_topic(
    State(state): State<AppState>,
    Path(id): Path<Id>,
) -> AppResult<StatusCode> {
    let events = {
        let _w = state.write_lock.lock().await;
        topic(&state, &id).await?;
        let all: Vec<Topic> = state.store.list(db::TOPIC).await?;
        let doomed = subtree(&all, &id);
        let doomed_set: HashSet<&str> = doomed.iter().map(String::as_str).collect();
        let mut events = Vec::new();

        let lessons: Vec<StoredLesson> = state.store.list(db::LESSON).await?;
        for l in lessons
            .iter()
            .filter(|l| doomed_set.contains(l.lesson.topic_id.as_str()))
        {
            events.extend(delete_lesson_locked(&state, &l.lesson.id).await?);
        }
        let notes: Vec<Note> = state.store.list(db::NOTE).await?;
        for n in notes
            .iter()
            .filter(|n| doomed_set.contains(n.topic_id.as_str()))
        {
            state.store.delete(db::NOTE, &n.id).await?;
            events.push(Event::NoteDeleted { id: n.id.clone() });
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
    Ok(StatusCode::NO_CONTENT)
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
    let lesson = {
        let _w = state.write_lock.lock().await;
        let mut row = lesson(&state, &id).await?;
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
            row.lesson.body_md = body.clone();
        }
        if let Some(pos) = patch.position {
            row.lesson.position = pos;
        }
        row.lesson.updated_at = Utc::now();
        state.store.put(db::LESSON, &id, &row).await?;
        row.lesson
    };
    state.emit_lesson(&lesson);
    Ok(Json(lesson))
}

pub async fn delete_lesson(
    State(state): State<AppState>,
    Path(id): Path<Id>,
) -> AppResult<StatusCode> {
    let events = {
        let _w = state.write_lock.lock().await;
        lesson(&state, &id).await?;
        delete_lesson_locked(&state, &id).await?
    };
    for e in events {
        state.emit(e);
    }
    Ok(StatusCode::NO_CONTENT)
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
    let note = {
        let _w = state.write_lock.lock().await;
        topic(&state, &req.topic_id).await?;
        if let Some(lesson_id) = &req.lesson_id {
            let l = lesson(&state, lesson_id).await?;
            if l.lesson.topic_id != req.topic_id {
                return Err(AppError::bad_request(
                    "That lesson belongs to a different topic.",
                ));
            }
        }
        let now = Utc::now();
        let note = Note {
            id: new_id(),
            topic_id: req.topic_id,
            lesson_id: req.lesson_id,
            title: req.title.trim().to_string(),
            body_md: req.body_md,
            quote: req.quote.filter(|q| !q.trim().is_empty()),
            created_at: now,
            updated_at: now,
        };
        state.store.put(db::NOTE, &note.id, &note).await?;
        note
    };
    state.emit(Event::NoteChanged { note: note.clone() });
    Ok(Json(note))
}

pub async fn patch_note(
    State(state): State<AppState>,
    Path(id): Path<Id>,
    ApiJson(patch): ApiJson<NotePatch>,
) -> AppResult<Json<Note>> {
    let note = {
        let _w = state.write_lock.lock().await;
        let mut note = note(&state, &id).await?;
        if let Some(title) = patch.title {
            note.title = title.trim().to_string();
        }
        if let Some(body) = patch.body_md {
            note.body_md = body;
        }
        note.updated_at = Utc::now();
        state.store.put(db::NOTE, &id, &note).await?;
        note
    };
    state.emit(Event::NoteChanged { note: note.clone() });
    Ok(Json(note))
}

pub async fn delete_note(
    State(state): State<AppState>,
    Path(id): Path<Id>,
) -> AppResult<StatusCode> {
    {
        let _w = state.write_lock.lock().await;
        note(&state, &id).await?;
        state.store.delete(db::NOTE, &id).await?;
    }
    state.emit(Event::NoteDeleted { id });
    Ok(StatusCode::NO_CONTENT)
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
