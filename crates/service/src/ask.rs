//! Ask: outline proposals, accepting them, and lesson generation jobs.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::Utc;
use margin_ai::{LessonContext, Passage};
use margin_shared::{
    AcceptProposal, AskRequest, Event, Id, Lesson, LessonStatus, Outline, OutlineLesson, Proposal,
    ProposalStatus, Source, Topic,
};

use crate::error::{ApiJson, AppError, AppResult};
use crate::library::{self, create_topic_locked, is_busy, topic_lessons};
use crate::state::{AppState, StoredLesson, new_id};
use crate::{db, sources, video};

pub const MAX_LESSONS: usize = 12;
/// How often a streaming lesson's partial body is saved.
const PARTIAL_SAVE_INTERVAL: Duration = Duration::from_secs(1);

fn validate_ask(req: &AskRequest) -> AppResult<()> {
    if req.prompt.trim().is_empty() {
        return Err(AppError::bad_request("Say what you want to learn."));
    }
    if !(1..=MAX_LESSONS as u32).contains(&req.lesson_count) {
        return Err(AppError::bad_request("Choose between 1 and 12 lessons."));
    }
    Ok(())
}

/// Every source must exist and be unattached or already in the topic the Ask is filed
/// under. Duplicates are dropped.
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

pub async fn ask(
    State(state): State<AppState>,
    ApiJson(mut req): ApiJson<AskRequest>,
) -> AppResult<Json<Proposal>> {
    validate_ask(&req)?;
    if let Some(parent) = &req.parent_topic_id {
        library::topic(&state, parent).await?;
    }
    check_sources(&state, &mut req).await?;
    state.ai_config().await?;
    let proposal = Proposal {
        id: new_id(),
        request: req,
        status: ProposalStatus::Drafting,
        outline: None,
        created_at: Utc::now(),
    };
    state
        .store
        .put(db::PROPOSAL, &proposal.id, &proposal)
        .await?;
    state.emit_proposal(&proposal);
    tokio::spawn(run_proposal_job(state.clone(), proposal.id.clone()));
    Ok(Json(proposal))
}

async fn run_proposal_job(state: AppState, id: Id) {
    let result = draft_outline(&state, &id).await;
    let update = async {
        let _w = state.write_lock.lock().await;
        let Some(mut proposal) = state.store.get::<Proposal>(db::PROPOSAL, &id).await? else {
            return anyhow::Ok(None);
        };
        if proposal.status != ProposalStatus::Drafting {
            return Ok(None);
        }
        match result {
            Ok(outline) => {
                proposal.outline = Some(outline);
                proposal.status = ProposalStatus::Ready;
            }
            Err(error) => proposal.status = ProposalStatus::Failed { error },
        }
        state.store.put(db::PROPOSAL, &id, &proposal).await?;
        Ok(Some(proposal))
    };
    match update.await {
        Ok(Some(p)) => state.emit_proposal(&p),
        Ok(None) => {}
        Err(e) => tracing::error!("couldn't save proposal {id}: {e:#}"),
    }
}

async fn draft_outline(state: &AppState, id: &str) -> Result<Outline, String> {
    let proposal: Proposal = match state.store.get(db::PROPOSAL, id).await {
        Ok(Some(p)) => p,
        Ok(None) => return Err("The proposal disappeared.".into()),
        Err(e) => return Err(format!("couldn't read the proposal: {e}")),
    };
    let ai = state.ai_config().await.map_err(|e| e.message)?;
    let subjects = root_subjects(state).await.map_err(|e| e.to_string())?;
    // Outlines see the passages (pictures included) but don't cite them.
    let passages: Vec<Passage> = sources::passages(
        state,
        &proposal.request.source_ids,
        &proposal.request.prompt,
    )
    .await
    .into_iter()
    .map(|r| r.passage)
    .collect();
    let outline = state
        .generator
        .propose_outline(&ai, &proposal.request, &subjects, &passages)
        .await
        .map_err(|e| e.to_string())?;
    if outline.lessons.is_empty() {
        return Err("The model proposed no lessons. Try asking again.".into());
    }
    Ok(outline)
}

async fn root_subjects(state: &AppState) -> anyhow::Result<Vec<String>> {
    let mut topics: Vec<Topic> = state.store.list(db::TOPIC).await?;
    library::sort_topics(&mut topics);
    Ok(topics
        .into_iter()
        .filter(|t| t.parent_id.is_none())
        .map(|t| t.title)
        .collect())
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

fn validate_outline(outline: &Outline) -> AppResult<Outline> {
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

pub async fn accept(
    State(state): State<AppState>,
    Path(id): Path<Id>,
    ApiJson(req): ApiJson<AcceptProposal>,
) -> AppResult<Json<Topic>> {
    let outline = validate_outline(&req.outline)?;
    state.ai_config().await?;

    let (topic, events, lesson_ids) = {
        let _w = state.write_lock.lock().await;
        let mut proposal: Proposal = state
            .store
            .get(db::PROPOSAL, &id)
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
        }
        let mut events = Vec::new();

        let subject = match &proposal.request.parent_topic_id {
            Some(parent) => library::topic(&state, parent).await?,
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
                        let t = create_topic_locked(&state, None, &outline.subject_title).await?;
                        events.push(Event::TopicChanged { topic: t.clone() });
                        t
                    }
                }
            }
        };

        let topic =
            create_topic_locked(&state, Some(subject.id.clone()), &outline.topic_title).await?;
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
            };
            state.store.put(db::LESSON, &row.lesson.id, &row).await?;
            lesson_ids.push(row.lesson.id.clone());
            events.push(Event::LessonChanged {
                lesson: state.public_lesson(&row.lesson),
            });
        }

        // The Ask's sources now belong to the new topic.
        events
            .extend(sources::attach_locked(&state, &proposal.request.source_ids, &topic.id).await?);

        proposal.outline = Some(outline);
        proposal.status = ProposalStatus::Accepted {
            topic_id: topic.id.clone(),
        };
        state
            .store
            .put(db::PROPOSAL, &proposal.id, &proposal)
            .await?;
        events.push(Event::ProposalChanged { proposal });
        (topic, events, lesson_ids)
    };
    for e in events {
        state.emit(e);
    }
    for lesson_id in lesson_ids {
        spawn_lesson_job(&state, lesson_id, true);
    }
    Ok(Json(topic))
}

pub async fn regenerate(
    State(state): State<AppState>,
    Path(id): Path<Id>,
) -> AppResult<Json<Lesson>> {
    state.ai_config().await?;
    let lesson = {
        let _w = state.write_lock.lock().await;
        let mut row = library::lesson(&state, &id).await?;
        let running = state.lesson_jobs.lock().unwrap().contains_key(&id);
        if is_busy(&row.lesson.status) && running {
            return Err(AppError::conflict(
                "lesson_busy",
                "This lesson is already being written.",
            ));
        }
        row.lesson.status = LessonStatus::Queued;
        row.lesson.updated_at = Utc::now();
        state.store.put(db::LESSON, &id, &row).await?;
        row.lesson
    };
    state.emit_lesson(&lesson);
    spawn_lesson_job(&state, id, false);
    Ok(Json(state.public_lesson(&lesson)))
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
        with_quiz: proposal.as_ref().is_some_and(|p| p.request.with_quiz),
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
    use margin_shared::Level;

    fn req(prompt: &str, n: u32) -> AskRequest {
        AskRequest {
            prompt: prompt.into(),
            lesson_count: n,
            level: Level::Intro,
            with_videos: false,
            with_quiz: false,
            parent_topic_id: None,
            source_ids: vec![],
        }
    }

    #[test]
    fn ask_validation() {
        assert!(validate_ask(&req("x", 1)).is_ok());
        assert!(validate_ask(&req("x", 12)).is_ok());
        assert!(validate_ask(&req("x", 0)).is_err());
        assert!(validate_ask(&req("x", 13)).is_err());
        assert!(validate_ask(&req("  ", 3)).is_err());
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
