//! Courses: outline proposals (written by the agent's `propose_course`), accepting them
//! (Create lessons on the outline card, or the agent's `accept_proposal`), and lesson
//! generation jobs.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::Utc;
use margin_ai::LessonContext;
use margin_shared::{
    AcceptProposal, AskRequest, Chat, ChatItemKind, Event, Id, ItemStatus, Lesson, LessonStatus,
    Level, Outline, OutlineLesson, Proposal, ProposalStatus, Source, Topic,
};

use crate::chats::{self, StoredChatItem};
use crate::error::{ApiJson, AppError, AppResult};
use crate::library::{self, create_topic_locked, is_busy, topic_lessons};
use crate::state::{AppState, Author, StoredLesson, new_id};
use crate::{db, sources, video};

pub const MAX_LESSONS: usize = 12;
/// How often a streaming lesson's partial body is saved.
const PARTIAL_SAVE_INTERVAL: Duration = Duration::from_secs(1);

/// Every source must exist and be unattached or already in the topic the course is
/// filed under. Duplicates are dropped.
async fn check_sources(state: &AppState, req: &mut AskRequest) -> AppResult<()> {
    let mut seen = HashSet::new();
    req.source_ids.retain(|id| seen.insert(id.clone()));
    for id in &req.source_ids {
        let Some(source) = state.store.get::<Source>(db::SOURCE, id).await? else {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "unknown_source",
                "One of the sources isn't on the service any more. Add it again.",
            ));
        };
        if source.topic_id.is_some() && source.topic_id != req.parent_topic_id {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "unknown_source",
                format!("{} belongs to another topic.", source.name),
            ));
        }
    }
    // Sources are read and searched with the local model; without it they'd be ignored.
    if !req.source_ids.is_empty() && !crate::embedding::will_be_available(state).await {
        return Err(AppError::conflict(
            "no_embedding_model",
            "Sources need the local embedding model. Download it in Settings → Sources.",
        ));
    }
    Ok(())
}

/// Stores a `Ready` proposal of `outline` made in chat `chat_id` (the agent's
/// `propose_course`) and emits it. Checked like an accepted outline: a prompt, 1 to 12
/// titled lessons, a topic title, a subject title when no parent topic is given (the
/// parent must exist), and sources that exist and aren't another topic's. Every earlier
/// `Ready` proposal of the chat becomes `Superseded` by it (under the same `write_lock`),
/// so only the newest outline in a chat can create a course.
pub async fn propose(
    state: &AppState,
    chat_id: &str,
    mut request: AskRequest,
    outline: Outline,
) -> AppResult<Proposal> {
    request.prompt = request.prompt.trim().to_string();
    if request.prompt.is_empty() {
        return Err(AppError::bad_request("Say what the course is for."));
    }
    let outline = validate_outline(&outline)?;
    match &request.parent_topic_id {
        Some(parent) => {
            library::topic(state, parent).await?;
        }
        None if outline.subject_title.is_empty() => {
            return Err(AppError::bad_request("Give the subject a title."));
        }
        None => {}
    }
    check_sources(state, &mut request).await?;
    request.lesson_count = outline.lessons.len() as u32;
    let proposal = Proposal {
        id: new_id(),
        request,
        status: ProposalStatus::Ready,
        outline: Some(outline),
        created_at: Utc::now(),
        chat_id: Some(chat_id.to_string()),
    };
    let superseded = {
        let _w = state.write_lock.lock().await;
        let mut earlier: Vec<Proposal> = state
            .store
            .list_where(db::PROPOSAL, "chat_id", chat_id)
            .await?;
        earlier.retain(|p| p.status == ProposalStatus::Ready);
        for p in &mut earlier {
            p.status = ProposalStatus::Superseded {
                by: proposal.id.clone(),
            };
            state.store.put(db::PROPOSAL, &p.id, &*p).await?;
        }
        state
            .store
            .put(db::PROPOSAL, &proposal.id, &proposal)
            .await?;
        earlier
    };
    for p in &superseded {
        state.emit_proposal(p);
    }
    state.emit_proposal(&proposal);
    Ok(proposal)
}

pub async fn get_proposal(
    State(state): State<AppState>,
    Path(id): Path<Id>,
) -> AppResult<Json<Proposal>> {
    let p = state
        .store
        .get(db::PROPOSAL, &id)
        .await?
        .ok_or_else(|| AppError::not_found("proposal"))?;
    Ok(Json(p))
}

pub fn validate_outline(outline: &Outline) -> AppResult<Outline> {
    let topic_title = outline.topic_title.trim();
    if topic_title.is_empty() {
        return Err(AppError::bad_request("Give the topic a title."));
    }
    if outline.lessons.is_empty() || outline.lessons.len() > MAX_LESSONS {
        return Err(AppError::bad_request(
            "An outline needs between 1 and 12 lessons.",
        ));
    }
    let mut lessons = Vec::with_capacity(outline.lessons.len());
    for l in &outline.lessons {
        let title = l.title.trim();
        if title.is_empty() {
            return Err(AppError::bad_request("Every lesson needs a title."));
        }
        lessons.push(OutlineLesson {
            title: title.into(),
            summary: l.summary.trim().into(),
            minutes: l.minutes,
        });
    }
    Ok(Outline {
        topic_title: topic_title.into(),
        subject_title: outline.subject_title.trim().into(),
        lessons,
    })
}

/// `POST /api/proposals/{id}/accept`: Create lessons on the outline card, with the
/// owner's edits and chips. A proposal made in a chat records it there as an `Action`
/// so the agent knows the course exists.
pub async fn accept(
    State(state): State<AppState>,
    Path(id): Path<Id>,
    ApiJson(req): ApiJson<AcceptProposal>,
) -> AppResult<Json<Topic>> {
    let (topic, proposal) = accept_proposal(&state, &id, req).await?;
    if let Some(chat_id) = &proposal.chat_id {
        let lessons = proposal.outline.as_ref().map_or(0, |o| o.lessons.len());
        if let Err(e) = record_course_created(&state, chat_id, &topic, lessons).await {
            tracing::error!(chat = %chat_id, "couldn't record the new course in its chat: {e:#}");
        }
    }
    Ok(Json(topic))
}

/// The `Action` text a chat gets when its proposal becomes a course.
pub fn course_created_text(topic_title: &str, lessons: usize) -> String {
    let n = if lessons == 1 {
        "1 lesson".to_string()
    } else {
        format!("{lessons} lessons")
    };
    format!("Created the course “{topic_title}” with {n}.")
}

/// Adds `Action "Created the course “…” with N lessons."` (replayed to the model as a
/// developer message) to the chat, unless it was deleted. It takes the running turn's
/// id when the chat is busy (it lands between that turn's items, in `seq` order).
async fn record_course_created(
    state: &AppState,
    chat_id: &str,
    topic: &Topic,
    lessons: usize,
) -> anyhow::Result<()> {
    let _w = state.write_lock.lock().await;
    let Some(chat) = state.store.get::<Chat>(db::CHAT, chat_id).await? else {
        return Ok(());
    };
    let turn_id = chat.state.turn_id().map_or_else(new_id, str::to_string);
    let text = course_created_text(&topic.title, lessons);
    let item = chats::new_item_locked(
        state,
        chat_id,
        &turn_id,
        ItemStatus::Done,
        ChatItemKind::Action { text: text.clone() },
    )
    .await?;
    chats::save_item_locked(
        state,
        &StoredChatItem {
            item,
            model_items: vec![margin_agent::action_input(&text)],
            full_output: None,
        },
    )
    .await
}

/// Turns a `Ready` proposal into a topic with queued lessons (filed under its parent
/// topic, or under the subject named by the outline, created if needed), attaches its
/// sources to the topic and starts the lesson jobs. `req.chips` override the
/// proposal's level, videos and quiz (and are kept on it, which the lesson jobs read).
/// Returns the topic and the accepted proposal.
pub async fn accept_proposal(
    state: &AppState,
    id: &str,
    req: AcceptProposal,
) -> AppResult<(Topic, Proposal)> {
    let outline = validate_outline(&req.outline)?;
    state.ai_config().await?;

    let (topic, events, lesson_ids, proposal) = {
        let _w = state.write_lock.lock().await;
        let mut proposal: Proposal = state
            .store
            .get(db::PROPOSAL, id)
            .await?
            .ok_or_else(|| AppError::not_found("proposal"))?;
        match &proposal.status {
            ProposalStatus::Ready => {}
            ProposalStatus::Drafting => {
                return Err(AppError::conflict(
                    "proposal_not_ready",
                    "The outline is still being written.",
                ));
            }
            ProposalStatus::Accepted { .. } => {
                return Err(AppError::conflict(
                    "already_accepted",
                    "This outline was already turned into lessons.",
                ));
            }
            ProposalStatus::Failed { .. } => {
                return Err(AppError::conflict(
                    "proposal_failed",
                    "This outline failed. Ask again.",
                ));
            }
            ProposalStatus::Superseded { .. } => {
                return Err(AppError::conflict(
                    "superseded",
                    "A newer outline replaced this one.",
                ));
            }
        }
        let chips = req.chips;
        if let Some(level) = chips.level {
            proposal.request.level = level;
        }
        if let Some(with_videos) = chips.with_videos {
            proposal.request.with_videos = with_videos;
        }
        if let Some(with_quiz) = chips.with_quiz {
            proposal.request.with_quiz = with_quiz;
        }
        proposal.request.lesson_count = outline.lessons.len() as u32;
        let mut events = Vec::new();

        // A parent topic deleted since the proposal was made: the course goes under the
        // outline's subject title instead, as the outline card shows it.
        let parent = match &proposal.request.parent_topic_id {
            Some(id) => state.store.get::<Topic>(db::TOPIC, id).await?,
            None => None,
        };
        if parent.is_none() {
            proposal.request.parent_topic_id = None;
        }
        let subject = match parent {
            Some(parent) => parent,
            None => {
                if outline.subject_title.is_empty() {
                    return Err(AppError::bad_request("Give the subject a title."));
                }
                let topics: Vec<Topic> = state.store.list(db::TOPIC).await?;
                let wanted = outline.subject_title.to_lowercase();
                let mut roots: Vec<Topic> = topics
                    .into_iter()
                    .filter(|t| t.parent_id.is_none())
                    .collect();
                library::sort_topics(&mut roots);
                match roots
                    .into_iter()
                    .find(|t| t.title.trim().to_lowercase() == wanted)
                {
                    Some(t) => t,
                    None => {
                        let t = create_topic_locked(state, None, &outline.subject_title).await?;
                        events.push(Event::TopicChanged { topic: t.clone() });
                        t
                    }
                }
            }
        };

        let topic =
            create_topic_locked(state, Some(subject.id.clone()), &outline.topic_title).await?;
        events.push(Event::TopicChanged {
            topic: topic.clone(),
        });

        let now = Utc::now();
        let mut lesson_ids = Vec::new();
        for (i, l) in outline.lessons.iter().enumerate() {
            let row = StoredLesson {
                lesson: Lesson {
                    citations: Vec::new(),
                    id: new_id(),
                    topic_id: topic.id.clone(),
                    title: l.title.clone(),
                    summary: l.summary.clone(),
                    position: i as i64,
                    level: proposal.request.level,
                    minutes: l.minutes,
                    body_md: String::new(),
                    status: LessonStatus::Queued,
                    created_at: now,
                    updated_at: now,
                },
                proposal_id: Some(proposal.id.clone()),
                previous_body: None,
                previous_citations: None,
                author: Author::Agent,
                prompt: None,
                with_quiz: false,
            };
            state.store.put(db::LESSON, &row.lesson.id, &row).await?;
            lesson_ids.push(row.lesson.id.clone());
            events.push(Event::LessonChanged {
                lesson: state.public_lesson(&row.lesson),
            });
        }

        // The proposal's sources now belong to the new topic.
        events
            .extend(sources::attach_locked(state, &proposal.request.source_ids, &topic.id).await?);

        proposal.outline = Some(outline);
        proposal.status = ProposalStatus::Accepted {
            topic_id: topic.id.clone(),
        };
        state
            .store
            .put(db::PROPOSAL, &proposal.id, &proposal)
            .await?;
        events.push(Event::ProposalChanged {
            proposal: proposal.clone(),
        });
        (topic, events, lesson_ids, proposal)
    };
    for e in events {
        state.emit(e);
    }
    for lesson_id in lesson_ids {
        spawn_lesson_job(state, lesson_id, true);
    }
    Ok((topic, proposal))
}

pub async fn regenerate(
    State(state): State<AppState>,
    Path(id): Path<Id>,
) -> AppResult<Json<Lesson>> {
    Ok(Json(regenerate_lesson(&state, &id).await?))
}

/// Queues a lesson to be written again and starts its job (`POST
/// /api/lessons/{id}/regenerate` and the agent's `regenerate_lesson`). Its body stays
/// until the new one is done, and comes back if writing fails.
pub async fn regenerate_lesson(state: &AppState, id: &str) -> AppResult<Lesson> {
    state.ai_config().await?;
    let lesson = {
        let _w = state.write_lock.lock().await;
        let mut row = library::lesson(state, id).await?;
        let running = state.lesson_jobs.lock().unwrap().contains_key(id);
        if is_busy(&row.lesson.status) && running {
            return Err(AppError::conflict(
                "lesson_busy",
                "This lesson is already being written.",
            ));
        }
        row.lesson.status = LessonStatus::Queued;
        row.lesson.updated_at = Utc::now();
        state.store.put(db::LESSON, id, &row).await?;
        row.lesson
    };
    state.emit_lesson(&lesson);
    spawn_lesson_job(state, id.to_string(), false);
    Ok(state.public_lesson(&lesson))
}

/// A lesson to add at the end of a topic (the agent's `write_lesson`).
pub struct NewLesson {
    pub topic_id: Id,
    pub title: String,
    pub summary: String,
    pub minutes: u32,
    pub level: Level,
    pub with_quiz: bool,
}

/// Appends a Queued lesson, written by the agent, to a topic and starts its job. With no
/// proposal behind it, the lesson keeps its own prompt (the request of the course its
/// topic came from, when there is one) and quiz option for the lesson writer. No video
/// follows it (`make_video` asks for one).
pub async fn append_lesson(state: &AppState, req: NewLesson) -> AppResult<Lesson> {
    let title = library::clean_title(&req.title, "lesson")?;
    state.ai_config().await?;
    let lesson = {
        let _w = state.write_lock.lock().await;
        library::topic(state, &req.topic_id).await?;
        let siblings = topic_lessons(state, &req.topic_id).await?;
        let position = siblings
            .iter()
            .map(|s| s.lesson.position + 1)
            .max()
            .unwrap_or(0);
        // The course's request, so the new lesson is written for the same learner.
        let mut prompt = None;
        for s in &siblings {
            if let Some(pid) = &s.proposal_id
                && let Some(p) = state.store.get::<Proposal>(db::PROPOSAL, pid).await?
            {
                prompt = Some(p.request.prompt);
                break;
            }
            if s.prompt.is_some() {
                prompt = s.prompt.clone();
                break;
            }
        }
        let now = Utc::now();
        let row = StoredLesson {
            lesson: Lesson {
                citations: Vec::new(),
                id: new_id(),
                topic_id: req.topic_id.clone(),
                title,
                summary: req.summary.trim().to_string(),
                position,
                level: req.level,
                minutes: req.minutes,
                body_md: String::new(),
                status: LessonStatus::Queued,
                created_at: now,
                updated_at: now,
            },
            proposal_id: None,
            previous_body: None,
            previous_citations: None,
            author: Author::Agent,
            prompt,
            with_quiz: req.with_quiz,
        };
        state.store.put(db::LESSON, &row.lesson.id, &row).await?;
        row.lesson
    };
    state.emit_lesson(&lesson);
    spawn_lesson_job(state, lesson.id.clone(), false);
    Ok(state.public_lesson(&lesson))
}

/// Starts writing a Queued lesson in the background (at most three at once).
/// `first_write` lessons queue a video afterwards when their proposal asked for one.
pub fn spawn_lesson_job(state: &AppState, lesson_id: Id, first_write: bool) {
    // Hold the map while spawning so the job can't remove its entry before it exists.
    let mut jobs = state.lesson_jobs.lock().unwrap();
    let handle = tokio::spawn(run_lesson_job(
        state.clone(),
        lesson_id.clone(),
        first_write,
    ));
    if let Some(old) = jobs.insert(lesson_id, handle.abort_handle()) {
        old.abort();
    }
}

async fn run_lesson_job(state: AppState, lesson_id: Id, first_write: bool) {
    let Ok(_permit) = state.lesson_slots.clone().acquire_owned().await else {
        return;
    };
    if let Err(e) = write_lesson(&state, &lesson_id, first_write).await {
        tracing::error!("lesson job {lesson_id} failed: {e:#}");
    }
    let me = tokio::task::id();
    let mut jobs = state.lesson_jobs.lock().unwrap();
    if jobs.get(&lesson_id).is_some_and(|h| h.id() == me) {
        jobs.remove(&lesson_id);
    }
}

/// Builds the generation context from the lesson's place in the library.
async fn lesson_context(
    state: &AppState,
    row: &StoredLesson,
) -> anyhow::Result<(LessonContext, Option<Proposal>, Vec<sources::Retrieved>)> {
    let topics: Vec<Topic> = state.store.list(db::TOPIC).await?;
    let topic = topics.iter().find(|t| t.id == row.lesson.topic_id);
    let topic_title = topic.map(|t| t.title.clone()).unwrap_or_default();
    // The subject is the topic's root ancestor.
    let mut subject = topic;
    while let Some(parent) = subject.and_then(|t| t.parent_id.as_ref()) {
        match topics.iter().find(|t| &t.id == parent) {
            Some(p) => subject = Some(p),
            None => break,
        }
    }
    let subject_title = subject
        .map(|t| t.title.clone())
        .unwrap_or_else(|| topic_title.clone());

    let proposal: Option<Proposal> = match &row.proposal_id {
        Some(pid) => state.store.get(db::PROPOSAL, pid).await?,
        None => None,
    };
    let siblings = topic_lessons(state, &row.lesson.topic_id).await?;
    let outline: Vec<OutlineLesson> = siblings
        .iter()
        .map(|s| OutlineLesson {
            title: s.lesson.title.clone(),
            summary: s.lesson.summary.clone(),
            minutes: s.lesson.minutes,
        })
        .collect();
    let index = siblings
        .iter()
        .position(|s| s.lesson.id == row.lesson.id)
        .unwrap_or(0);
    let prompt = proposal
        .as_ref()
        .map(|p| p.request.prompt.clone())
        .or_else(|| row.prompt.clone())
        .unwrap_or_else(|| row.lesson.title.clone());

    // The passages most relevant to this lesson, from its topic's sources.
    let asked = proposal
        .as_ref()
        .map(|p| p.request.source_ids.clone())
        .unwrap_or_default();
    let source_ids = sources::for_lesson(state, &row.lesson.topic_id, &asked).await?;
    let query = format!(
        "{}\n{}\n{}",
        row.lesson.title,
        row.lesson.summary,
        prompt.trim()
    );
    let retrieved = sources::passages(state, &source_ids, &query).await;
    let passages = retrieved.iter().map(|r| r.passage.clone()).collect();

    let ctx = LessonContext {
        subject_title,
        topic_title,
        prompt,
        level: row.lesson.level,
        with_quiz: proposal
            .as_ref()
            .map_or(row.with_quiz, |p| p.request.with_quiz),
        outline,
        index,
        passages,
    };
    Ok((ctx, proposal, retrieved))
}

async fn write_lesson(state: &AppState, lesson_id: &str, first_write: bool) -> anyhow::Result<()> {
    // Generating: stash the old body (restored on failure) and start from empty.
    let row = {
        let _w = state.write_lock.lock().await;
        let Some(mut row) = state
            .store
            .get::<StoredLesson>(db::LESSON, lesson_id)
            .await?
        else {
            return Ok(());
        };
        let ai = match state.ai_config().await {
            Ok(ai) => ai,
            Err(e) => {
                row.lesson.status = LessonStatus::Failed { error: e.message };
                row.lesson.updated_at = Utc::now();
                state.store.put(db::LESSON, lesson_id, &row).await?;
                state.emit_lesson(&row.lesson);
                return Ok(());
            }
        };
        if !row.lesson.body_md.is_empty() {
            row.previous_body = Some(std::mem::take(&mut row.lesson.body_md));
            row.previous_citations = Some(std::mem::take(&mut row.lesson.citations));
        }
        row.lesson.citations.clear();
        row.lesson.status = LessonStatus::Generating;
        row.lesson.updated_at = Utc::now();
        state.store.put(db::LESSON, lesson_id, &row).await?;
        state.emit_lesson(&row.lesson);
        (row, ai)
    };
    let (row, ai) = row;
    let (ctx, proposal, retrieved) = lesson_context(state, &row).await?;

    let partial = Arc::new(Mutex::new(String::new()));
    let saver = tokio::spawn(save_partial_bodies(
        state.clone(),
        lesson_id.to_string(),
        partial.clone(),
    ));
    let on_delta = {
        let state = state.clone();
        let partial = partial.clone();
        let lesson_id = lesson_id.to_string();
        Box::new(move |text: &str| {
            if text.is_empty() {
                return;
            }
            partial.lock().unwrap().push_str(text);
            state.emit(Event::LessonDelta {
                lesson_id: lesson_id.clone(),
                text: text.to_string(),
            });
        })
    };
    let result = state.generator.write_lesson(&ai, &ctx, on_delta).await;
    saver.abort();
    let _ = saver.await;

    let lesson = {
        let _w = state.write_lock.lock().await;
        let Some(mut row) = state
            .store
            .get::<StoredLesson>(db::LESSON, lesson_id)
            .await?
        else {
            return Ok(());
        };
        match &result {
            Ok(body) => {
                // Footnotes renumbered in reading order, with what they point at.
                (row.lesson.body_md, row.lesson.citations) = sources::cite(body, &retrieved);
                row.lesson.status = LessonStatus::Ready;
                // The body is the model's writing now (owner decision Q2).
                row.author = Author::Agent;
            }
            Err(e) => {
                match (row.previous_body.clone(), row.previous_citations.clone()) {
                    (Some(body), citations) => {
                        row.lesson.body_md = body;
                        row.lesson.citations = citations.unwrap_or_default();
                    }
                    (None, _) => {
                        let streamed = partial.lock().unwrap().clone();
                        (row.lesson.body_md, row.lesson.citations) =
                            sources::cite(&streamed, &retrieved);
                    }
                }
                row.lesson.status = LessonStatus::Failed {
                    error: e.to_string(),
                };
            }
        }
        row.previous_body = None;
        row.previous_citations = None;
        row.lesson.updated_at = Utc::now();
        state.store.put(db::LESSON, lesson_id, &row).await?;
        row.lesson
    };
    state.emit_lesson(&lesson);

    if result.is_ok()
        && first_write
        && proposal.is_some_and(|p| p.request.with_videos)
        && let Err(e) = video::enqueue(state, lesson_id).await
    {
        tracing::warn!(
            "couldn't queue the video for lesson {lesson_id}: {}",
            e.message
        );
    }
    Ok(())
}

/// Saves the streamed text every second so a library reload shows progress.
async fn save_partial_bodies(state: AppState, lesson_id: Id, partial: Arc<Mutex<String>>) {
    let mut saved_len = 0;
    let mut tick = tokio::time::interval(PARTIAL_SAVE_INTERVAL);
    tick.tick().await;
    loop {
        tick.tick().await;
        let text = partial.lock().unwrap().clone();
        if text.len() == saved_len {
            continue;
        }
        let _w = state.write_lock.lock().await;
        match state
            .store
            .get::<StoredLesson>(db::LESSON, &lesson_id)
            .await
        {
            Ok(Some(mut row)) if row.lesson.status == LessonStatus::Generating => {
                row.lesson.body_md = text.clone();
                if let Err(e) = state.store.put(db::LESSON, &lesson_id, &row).await {
                    tracing::warn!("couldn't save partial lesson: {e:#}");
                }
                saved_len = text.len();
            }
            _ => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn course_created_texts() {
        assert_eq!(
            course_created_text("Fourier series", 5),
            "Created the course “Fourier series” with 5 lessons."
        );
        assert_eq!(
            course_created_text("Laplace", 1),
            "Created the course “Laplace” with 1 lesson."
        );
    }

    #[test]
    fn outline_validation_trims() {
        let o = Outline {
            topic_title: " T ".into(),
            subject_title: " S ".into(),
            lessons: vec![OutlineLesson {
                title: " a ".into(),
                summary: "s".into(),
                minutes: 5,
            }],
        };
        let v = validate_outline(&o).unwrap();
        assert_eq!(v.topic_title, "T");
        assert_eq!(v.lessons[0].title, "a");
        assert!(
            validate_outline(&Outline {
                lessons: vec![],
                ..o
            })
            .is_err()
        );
    }
}
