//! The context API for agents (`docs/agent-rag-architecture.md` §2.12): the same
//! retrieval, outline and passage reads as Margin's own agent tools, over HTTP behind
//! the device token (a per-agent token with a project scope later), plus ingest of an
//! agent's document and the change feed. The MCP adapter is a thin layer over these.
//!
//! R1-retrieve fills the bodies (one implementation shared with `agent_tools.rs`); the
//! signatures are the seam.

use axum::Json;
use axum::extract::{Path, Query, State};
use margin_shared::context::{
    ContextChanges, ContextIngest, ContextQuery, Document, DocumentOutline, Hit, Project,
};
use serde::Deserialize;

use crate::error::{ApiJson, AppResult};
use crate::state::AppState;

/// The project every scope resolves inside until there is more than one.
pub const DEFAULT_PROJECT_NAME: &str = "My library";

/// `GET routes::PROJECTS`.
pub async fn projects(State(_state): State<AppState>) -> AppResult<Json<Vec<Project>>> {
    unimplemented!("R1, retrieve: GET /api/projects")
}

/// `POST routes::CONTEXT_SEARCH`.
pub async fn search(
    State(_state): State<AppState>,
    ApiJson(_q): ApiJson<ContextQuery>,
) -> AppResult<Json<Vec<Hit>>> {
    unimplemented!("R1, retrieve: POST /api/context/search")
}

/// `GET routes::context_document_outline(id)`.
pub async fn outline(
    State(_state): State<AppState>,
    Path(_id): Path<String>,
) -> AppResult<Json<DocumentOutline>> {
    unimplemented!("R1, retrieve: GET /api/context/documents/{{id}}/outline")
}

#[derive(Debug, Deserialize)]
pub struct PassagesQuery {
    pub from: u32,
    pub to: u32,
}

/// `GET routes::context_document_passages(id)?from&to`.
pub async fn passages(
    State(_state): State<AppState>,
    Path(_id): Path<String>,
    Query(_q): Query<PassagesQuery>,
) -> AppResult<Json<Vec<Hit>>> {
    unimplemented!("R1, retrieve: GET /api/context/documents/{{id}}/passages")
}

/// `POST routes::CONTEXT_INGEST`.
pub async fn ingest(
    State(_state): State<AppState>,
    ApiJson(_doc): ApiJson<ContextIngest>,
) -> AppResult<Json<Document>> {
    unimplemented!("R1, retrieve: POST /api/context/ingest")
}

#[derive(Debug, Deserialize)]
pub struct ChangesQuery {
    #[serde(default)]
    pub since: u64,
}

/// `GET routes::CONTEXT_CHANGES?since`.
pub async fn changes(
    State(_state): State<AppState>,
    Query(_q): Query<ChangesQuery>,
) -> AppResult<Json<ContextChanges>> {
    unimplemented!("R1, retrieve: GET /api/context/changes")
}
