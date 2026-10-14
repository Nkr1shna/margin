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
use axum::http::StatusCode;
use margin_shared::context::{
    ContextChanges, ContextIngest, ContextQuery, Document, DocumentOutline, Hit, Origin, Project,
};
use margin_shared::pipeline::Route;
use margin_shared::routes::TOOL_READ_PASSAGES_MAX;
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

/// `POST routes::CONTEXT_SEARCH`.
pub async fn search(
    State(state): State<AppState>,
    ApiJson(q): ApiJson<ContextQuery>,
) -> AppResult<Json<Vec<Hit>>> {
    if q.text.trim().is_empty() {
        return Err(AppError::bad_request("Give something to search for."));
    }
    if q.k == 0 {
        return Err(AppError::bad_request("k must be at least 1."));
    }
    match retrieval::context_search(&state, &q, Route::Chat, &new_id()).await {
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
    Path(id): Path<String>,
    Query(q): Query<PassagesQuery>,
) -> AppResult<Json<Vec<Hit>>> {
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
    if doc.name.trim().is_empty() {
        return Err(AppError::bad_request("Give the document a name."));
    }
    if doc.text.trim().is_empty() {
        return Err(AppError::bad_request("The document has no text."));
    }
    let req = ContextIngest {
        origin: Origin::Agent { name: agent },
        ..doc
    };
    Ok(Json(crate::ingest::ingest_text(&state, &req).await?))
}

#[derive(Debug, Deserialize)]
pub struct ChangesQuery {
    #[serde(default)]
    pub since: u64,
}

/// `GET routes::CONTEXT_CHANGES?since`: `ingest::changes_since`.
pub async fn changes(
    State(state): State<AppState>,
    Query(q): Query<ChangesQuery>,
) -> AppResult<Json<ContextChanges>> {
    Ok(Json(crate::ingest::changes_since(&state, q.since).await?))
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
