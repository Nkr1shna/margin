//! The context API for agents (`docs/agent-rag-architecture.md` §2.12): the same
//! retrieval, outline and passage reads as Margin's own agent tools, over HTTP behind
//! the device token (a per-agent token with a project scope later), plus ingest of an
//! agent's document and the change feed. The MCP adapter is a thin layer over these.
//!
//! Every handler calls the function the agent's tool calls (`retrieval::context_search`,
//! `retrieval::document_outline`, `retrieval::read_passages`): one retrieval path for
//! Margin's agent, external agents and the GUI.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use chrono::Utc;
use margin_shared::context::{
    AGENT_HEADER, ContextChanges, ContextIngest, ContextQuery, Document, DocumentOutline, Hit,
    Origin, Project, TraceKind, TraceRow, VIA_MCP, is_agent_name,
};
use margin_shared::pipeline::Route;
use margin_shared::routes::TOOL_READ_PASSAGES_MAX;
use margin_shared::text::for_model;
use serde::Deserialize;

use crate::db;
use crate::error::{ApiJson, AppError, AppResult};
use crate::retrieval::{self, BadScope, DocState};
use crate::state::{AppState, new_id};

/// The project every scope resolves inside until there is more than one.
pub const DEFAULT_PROJECT_NAME: &str = "My library";

fn bad_scope(message: String) -> AppError {
    AppError::new(StatusCode::BAD_REQUEST, "bad_scope", message)
}

fn not_ready() -> AppError {
    AppError::conflict(
        "not_ready",
        "That document is still being read. Try again in a moment.",
    )
}

/// 404 for an unknown document, 409 `not_ready` while it is first being indexed.
async fn readable(state: &AppState, id: &str) -> AppResult<()> {
    match retrieval::doc_state(&state.store, id).await? {
        DocState::Ready => Ok(()),
        DocState::NotReady => Err(not_ready()),
        DocState::Unknown => Err(AppError::not_found("document")),
    }
}

/// `GET routes::PROJECTS`.
pub async fn projects(State(state): State<AppState>) -> AppResult<Json<Vec<Project>>> {
    retrieval::project(&state.store).await?;
    let mut projects: Vec<Project> = state.store.list(db::PROJECT).await?;
    projects.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    Ok(Json(projects))
}

/// The agent `margin-mcp` names in `AGENT_HEADER`, as the trace rows' caller; `None`
/// without the header (or with a name that isn't one).
fn caller(headers: &HeaderMap) -> Option<retrieval::Caller> {
    let name = headers.get(AGENT_HEADER)?.to_str().ok()?.trim();
    is_agent_name(name).then(|| retrieval::Caller {
        actor: format!("agent:{name}"),
        via: VIA_MCP.to_string(),
    })
}

/// `POST routes::CONTEXT_SEARCH`.
pub async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiJson(q): ApiJson<ContextQuery>,
) -> AppResult<Json<Vec<Hit>>> {
    if q.text.trim().is_empty() {
        return Err(AppError::bad_request("Give something to search for."));
    }
    if q.k == 0 {
        return Err(AppError::bad_request("k must be at least 1."));
    }
    match retrieval::context_search(&state, &q, Route::Chat, None, &new_id(), caller(&headers))
        .await
    {
        Ok(packed) => Ok(Json(packed.iter().map(retrieval::packed_hit).collect())),
        Err(e) => match e.downcast_ref::<BadScope>() {
            Some(bad) => Err(bad_scope(bad.to_string())),
            None => Err(e.into()),
        },
    }
}

/// `GET routes::context_document_outline(id)`.
pub async fn outline(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<DocumentOutline>> {
    readable(&state, &id).await?;
    let sections = retrieval::document_outline(&state.store, &id).await?;
    Ok(Json(DocumentOutline {
        document_id: id,
        sections,
    }))
}

#[derive(Debug, Deserialize)]
pub struct PassagesQuery {
    pub from: u32,
    pub to: u32,
}

/// `GET routes::context_document_passages(id)?from&to`.
pub async fn passages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<PassagesQuery>,
) -> AppResult<Json<Vec<Hit>>> {
    let started = std::time::Instant::now();
    if q.to < q.from {
        return Err(AppError::bad_request("`to` comes before `from`."));
    }
    readable(&state, &id).await?;
    let rows = retrieval::read_passages(
        &state.store,
        &id,
        retrieval::Selector::Ords {
            from: q.from,
            to: q.to,
        },
        TOOL_READ_PASSAGES_MAX,
    )
    .await?;
    // An agent reading after a search: an implicit relevance mark for the eval feed.
    if let Some(c) = caller(&headers) {
        let mut row = TraceRow::new(TraceKind::ToolCall, new_id(), Utc::now());
        row.trace_id = new_id();
        c.stamp(&mut row);
        row.tool = Some("read_passages".into());
        row.document_id = Some(id.clone());
        row.read_range = Some([q.from, q.to]);
        row.hit_ids = rows.iter().map(|r| r.id.clone()).collect();
        row.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
        retrieval::record_trace(&state, row);
    }
    Ok(Json(
        rows.iter()
            .map(|r| retrieval::context_hit(r, 1.0, None))
            .collect(),
    ))
}

/// `POST routes::CONTEXT_INGEST`: `ingest::ingest_text` (the same agent and name
/// replaces that document's text; the diff keeps unchanged passages), `actor:
/// agent:<name>`.
pub async fn ingest(
    State(state): State<AppState>,
    ApiJson(doc): ApiJson<ContextIngest>,
) -> AppResult<Json<Document>> {
    let agent = match &doc.origin {
        Origin::Agent { name } if !name.trim().is_empty() => name.trim().to_string(),
        _ => {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "bad_origin",
                "Documents added through the context API come from an agent: origin {\"origin\": \"agent\", \"name\": …}.",
            ));
        }
    };
    // Another agent's writing reaches Margin's models when it is retrieved: stored as a
    // model may read it (`text::for_model`, `docs/phase5-brief.md` D2.1).
    let name = for_model(&doc.name);
    let text = for_model(&doc.text);
    if name.trim().is_empty() {
        return Err(AppError::bad_request("Give the document a name."));
    }
    if text.trim().is_empty() {
        return Err(AppError::bad_request("The document has no text."));
    }
    let req = ContextIngest {
        origin: Origin::Agent { name: agent },
        name,
        text,
        ..doc
    };
    Ok(Json(crate::ingest::ingest_text(&state, &req).await?))
}

#[derive(Debug, Deserialize)]
pub struct ChangesQuery {
    #[serde(default)]
    pub since: u64,
    /// Changes per page: `CHANGES_PAGE_DEFAULT` when absent, at most `CHANGES_PAGE_MAX`.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `GET routes::CONTEXT_CHANGES?since`: `ingest::changes_since`.
pub async fn changes(
    State(state): State<AppState>,
    Query(q): Query<ChangesQuery>,
) -> AppResult<Json<ContextChanges>> {
    use margin_shared::context::{CHANGES_PAGE_DEFAULT, CHANGES_PAGE_MAX};
    let limit = q.limit.unwrap_or(CHANGES_PAGE_DEFAULT);
    if !(1..=CHANGES_PAGE_MAX).contains(&limit) {
        return Err(AppError::bad_request(format!(
            "limit is 1 to {CHANGES_PAGE_MAX}."
        )));
    }
    Ok(Json(
        crate::ingest::changes_page(&state, q.since, limit).await?,
    ))
}

/// `DELETE routes::context_document(id)`: an agent's document and its passages go, and
/// the change feed lists it as deleted. 404 unknown; 403 `forbidden` for a document
/// that isn't an agent's (sources, folder files and library items are deleted where
/// they live).
pub async fn delete_document(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    let _w = state.write_lock.lock().await;
    let Some(doc) = state.store.get::<Document>(db::DOCUMENT, &id).await? else {
        return Err(AppError::not_found("document"));
    };
    if !matches!(doc.origin, Origin::Agent { .. }) {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Only documents an agent added can be deleted here.",
        ));
    }
    crate::ingest::delete_document_locked(&state, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}
