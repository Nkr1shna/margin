//! The context API for agents (`docs/agent-rag-architecture.md` §2.12): the same
//! retrieval, outline and passage reads as Margin's own agent tools, over HTTP, plus
//! ingest of an agent's document and the change feed. The MCP adapter is a thin layer
//! over these.
//!
//! Every handler calls the function the agent's tool calls (`retrieval::context_search`,
//! `retrieval::document_outline`, `retrieval::read_passages`): one retrieval path for
//! Margin's agent, external agents and the GUI.
//!
//! Who calls (L1, `docs/l1-brief.md` D6, D7): a device token (the person; an
//! `x-margin-agent` header names the agent for the trace rows, `agent:<name>`) or an
//! agent's `mga_` token (the agent: `agent/<id>`, the header ignored). A search reads
//! the caller's access paths; a caller-supplied `access` must be a subset of them (400
//! `bad_access`). An ingested document belongs to the caller's person (an agent's: its
//! owner) and only that person deletes or replaces it. Every trace row a request records
//! names its principal. Outline and passages by document id are not filtered in L1
//! (closed by L2).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use chrono::Utc;
use margin_shared::access_code;
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
use crate::principal::{CurrentPrincipal, Principal};
use crate::retrieval::{self, BadScope, DocState};
use crate::state::{AppState, new_id};
use crate::{agents, trace};

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

fn not_owner() -> AppError {
    AppError::new(
        StatusCode::FORBIDDEN,
        access_code::NOT_OWNER,
        "That document belongs to someone else; only they can change or delete it.",
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

/// Who the trace rows say called: an agent token is its agent (`agent/<id>`, whatever
/// the header says); a device token names its agent in `AGENT_HEADER` (`agent:<name>`),
/// or is the app's own call (`None`).
fn caller(p: &Principal, headers: &HeaderMap) -> Option<retrieval::Caller> {
    if p.is_agent() {
        return Some(retrieval::Caller {
            actor: p.id.clone(),
            via: VIA_MCP.to_string(),
        });
    }
    let name = headers.get(AGENT_HEADER)?.to_str().ok()?.trim();
    is_agent_name(name).then(|| retrieval::Caller {
        actor: format!("agent:{name}"),
        via: VIA_MCP.to_string(),
    })
}

/// The access paths a query reads: the caller's whole set, or the subset it asked for
/// (400 `bad_access` naming the first path it may not read).
pub fn query_access(p: &Principal, asked: Option<Vec<String>>) -> AppResult<Vec<String>> {
    match asked {
        None => Ok(p.paths.clone()),
        Some(asked) => {
            if let Some(bad) = asked.iter().find(|a| !p.paths.contains(a)) {
                return Err(AppError::new(
                    StatusCode::BAD_REQUEST,
                    access_code::BAD_ACCESS,
                    format!(
                        "You can't read {bad}. Leave out `access` to search everything you can read."
                    ),
                ));
            }
            Ok(asked)
        }
    }
}

/// `POST routes::CONTEXT_SEARCH`.
pub async fn search(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    headers: HeaderMap,
    ApiJson(mut q): ApiJson<ContextQuery>,
) -> AppResult<Json<Vec<Hit>>> {
    if q.text.trim().is_empty() {
        return Err(AppError::bad_request("Give something to search for."));
    }
    if q.k == 0 {
        return Err(AppError::bad_request("k must be at least 1."));
    }
    q.access = Some(query_access(&p, q.access.take())?);
    let found = trace::with_principal(
        p.id.clone(),
        retrieval::context_search(
            &state,
            &q,
            Route::Chat,
            None,
            &new_id(),
            caller(&p, &headers),
        ),
    )
    .await;
    match found {
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
    CurrentPrincipal(p): CurrentPrincipal,
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
    if let Some(c) = caller(&p, &headers) {
        let mut row = TraceRow::new(TraceKind::ToolCall, new_id(), Utc::now());
        row.trace_id = new_id();
        c.stamp(&mut row);
        row.principal = Some(p.id.clone());
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

/// The document an agent's ingest of `name` would replace.
async fn existing_agent_document(
    state: &AppState,
    origin: &Origin,
    name: &str,
) -> AppResult<Option<Document>> {
    let found: Vec<Document> = state
        .store
        .query_rows(
            "SELECT VALUE doc FROM document WHERE doc.origin = $origin AND doc.name = $name",
            vec![
                (
                    "origin",
                    serde_json::to_value(origin).map_err(AppError::internal)?,
                ),
                ("name", serde_json::json!(name)),
            ],
        )
        .await?;
    Ok(found.into_iter().next())
}

/// `POST routes::CONTEXT_INGEST`: `ingest::ingest_text` (the same agent and name
/// replaces that document's text; the diff keeps unchanged passages). On a device token
/// the origin's name is the agent's and `actor` is `agent:<name>`; on an agent token the
/// origin is that agent's name and `actor` is `agent/<id>`. Either way the document's
/// `owner` is the caller's person, and someone else's document isn't replaced (403
/// `not_owner`).
pub async fn ingest(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
    ApiJson(doc): ApiJson<ContextIngest>,
) -> AppResult<Json<Document>> {
    let agent = match agents::agent_of(&state, &p).await? {
        Some(agent) => agent.name,
        None if p.is_agent() => return Err(AppError::unauthorized()),
        None => match &doc.origin {
            Origin::Agent { name } if !name.trim().is_empty() => name.trim().to_string(),
            _ => {
                return Err(AppError::new(
                    StatusCode::BAD_REQUEST,
                    "bad_origin",
                    "Documents added through the context API come from an agent: origin {\"origin\": \"agent\", \"name\": …}.",
                ));
            }
        },
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
    if let Some(existing) = existing_agent_document(&state, &req.origin, req.name.trim()).await?
        && existing.owner.as_ref().is_some_and(|o| o != &p.person_id)
    {
        return Err(not_owner());
    }
    // Whose it is, and (on an agent token) which agent wrote it: written in the same
    // write as the document, so no reader sees it without its owner.
    let actor = p.is_agent().then(|| p.id.clone());
    let written = trace::with_principal(
        p.id.clone(),
        crate::ingest::ingest_text_as(&state, &req, Some(&p.person_id), actor.as_deref()),
    )
    .await?;
    Ok(Json(written))
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
/// they live); 403 `not_owner` for another person's.
pub async fn delete_document(
    State(state): State<AppState>,
    CurrentPrincipal(p): CurrentPrincipal,
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
    if doc.owner.as_ref().is_some_and(|o| o != &p.person_id) {
        return Err(not_owner());
    }
    trace::with_principal(
        p.id.clone(),
        crate::ingest::delete_document_locked(&state, &id),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::principal::PrincipalKind;
    use margin_shared::{OrgRole, Person, PersonSettings, PersonState};

    fn principal(kind: PrincipalKind, paths: &[&str]) -> Principal {
        Principal {
            id: match &kind {
                PrincipalKind::Agent { agent_id, .. } => format!("agent/{agent_id}"),
                PrincipalKind::Person => "person/p".into(),
            },
            kind,
            person_id: "p".into(),
            paths: paths.iter().map(|p| p.to_string()).collect(),
            deny_ids: vec![],
            org_role: OrgRole::Member,
            device: None,
            person: Person {
                id: "p".into(),
                github_login: "p".into(),
                github_id: None,
                handle: "pp".into(),
                name: "P".into(),
                avatar_url: None,
                org_role: OrgRole::Member,
                state: PersonState::Active,
                groups: vec![],
                settings: PersonSettings::default(),
                created_at: Utc::now(),
            },
        }
    }

    #[test]
    fn access_is_the_callers_or_a_subset() {
        let p = principal(PrincipalKind::Person, &["company", "person/p"]);
        assert_eq!(query_access(&p, None).unwrap(), vec!["company", "person/p"]);
        assert_eq!(
            query_access(&p, Some(vec!["person/p".into()])).unwrap(),
            vec!["person/p"]
        );
        for bad in ["company/finance", "person/q"] {
            let e = query_access(&p, Some(vec!["company".into(), bad.into()])).unwrap_err();
            assert_eq!((e.status, e.code), (StatusCode::BAD_REQUEST, "bad_access"));
            assert!(e.message.contains(bad));
        }
    }

    #[test]
    fn an_agent_token_is_its_agent_whatever_the_header_says() {
        let mut headers = HeaderMap::new();
        headers.insert(AGENT_HEADER, "claude-code".parse().unwrap());
        let agent = principal(
            PrincipalKind::Agent {
                owner_id: "p".into(),
                agent_id: "a1".into(),
            },
            &["company"],
        );
        let c = caller(&agent, &headers).unwrap();
        assert_eq!((c.actor.as_str(), c.via.as_str()), ("agent/a1", VIA_MCP));
        assert!(caller(&agent, &HeaderMap::new()).is_some());
        let person = principal(PrincipalKind::Person, &["company"]);
        assert_eq!(
            caller(&person, &headers).unwrap().actor,
            "agent:claude-code"
        );
        assert!(caller(&person, &HeaderMap::new()).is_none());
    }
}
