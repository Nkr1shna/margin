//! An in-process fake of the service's context API and device sign-in, over the wire
//! types of `margin_shared`. Passages are a document's paragraphs; search scores by the
//! query words a passage contains. Never the real service.

#![allow(dead_code, reason = "each test file uses part of it")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use chrono::{Duration, Utc};
use margin_shared::context::{
    ContextChanges, ContextIngest, ContextQuery, Document, DocumentOutline, DocumentStatus, Hit,
    Modality, Origin, OutlineSection, Scope,
};
use margin_shared::routes::TOOL_READ_PASSAGES_MAX;
use margin_shared::{
    ApiError, LoginState, PollLoginRequest, SourceKind, StartLogin, StartLoginRequest, routes,
};
use serde::Deserialize;

pub const TOKEN: &str = "device-token-1";
pub const POLL_SECRET: &str = "poll-secret";

#[derive(Clone)]
pub struct Passage {
    pub section_path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub text: String,
}

#[derive(Default)]
pub struct Inner {
    pub generation: u64,
    pub docs: Vec<(Document, Vec<Passage>)>,
    pub deleted: Vec<(u64, String)>,
    pub next_id: u64,
    /// login_id → (device_name, approved, collected)
    pub logins: HashMap<String, (String, bool, bool)>,
    pub searches: Vec<ContextQuery>,
    pub passage_reads: Vec<(String, u32, u32)>,
    pub base: String,
}

#[derive(Clone, Default)]
pub struct Fake(pub Arc<Mutex<Inner>>);

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(ApiError {
            code: code.into(),
            message: message.into(),
        }),
    )
        .into_response()
}

fn authorized(headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {TOKEN}"))
}

fn unauthorized() -> Response {
    error(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "Sign in to Margin.",
    )
}

/// Paragraphs with the heading path they sit under and their 1-based lines.
pub fn split(text: &str) -> Vec<Passage> {
    let mut out = Vec::new();
    let mut headings: Vec<(usize, String)> = Vec::new();
    let mut current: Vec<(u32, &str)> = Vec::new();
    let mut flush = |current: &mut Vec<(u32, &str)>, headings: &Vec<(usize, String)>| {
        if let (Some(first), Some(last)) = (current.first(), current.last()) {
            out.push(Passage {
                section_path: headings
                    .iter()
                    .map(|(_, h)| h.as_str())
                    .collect::<Vec<_>>()
                    .join(" > "),
                start_line: first.0,
                end_line: last.0,
                text: current
                    .iter()
                    .map(|(_, l)| *l)
                    .collect::<Vec<_>>()
                    .join("\n"),
            });
        }
        current.clear();
    };
    for (i, line) in text.lines().enumerate() {
        let n = i as u32 + 1;
        if let Some(rest) = line.strip_prefix('#') {
            flush(&mut current, &headings);
            let level = 1 + rest.chars().take_while(|c| *c == '#').count();
            let title = rest.trim_start_matches('#').trim().to_string();
            headings.retain(|(l, _)| *l < level);
            headings.push((level, title));
        } else if line.trim().is_empty() {
            flush(&mut current, &headings);
        } else {
            current.push((n, line));
        }
    }
    flush(&mut current, &headings);
    out
}

fn hit(doc: &Document, p: &Passage, ord: usize, score: f32) -> Hit {
    Hit {
        document_id: doc.id.clone(),
        unit_id: format!("{}-u{ord}", doc.id),
        passage_id: format!("{}-p{ord}", doc.id),
        modality: Modality::Text,
        section_path: p.section_path.clone(),
        page: None,
        start_line: Some(p.start_line),
        end_line: Some(p.end_line),
        start_ms: None,
        end_ms: None,
        text: p.text.clone(),
        score,
        highlight: None,
    }
}

impl Fake {
    pub fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap()
    }

    /// Adds a document directly, as if `origin`/`actor` had written it.
    pub fn add(&self, name: &str, origin: Origin, actor: &str, text: &str) -> String {
        let mut s = self.lock();
        s.generation += 1;
        s.next_id += 1;
        let id = format!("doc{}", s.next_id);
        let passages = split(text);
        let now = Utc::now();
        let doc = Document {
            id: id.clone(),
            project_id: "project1".into(),
            origin,
            name: name.into(),
            kind: SourceKind::Text,
            status: DocumentStatus::Ready {
                passages: passages.len() as u32,
            },
            generation: s.generation,
            bytes: text.len() as u64,
            actor: actor.into(),
            access: vec!["company".into()],
            created_at: now,
            updated_at: now,
        };
        s.docs.push((doc, passages));
        id
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .lock()
            .docs
            .iter()
            .map(|(d, _)| d.name.clone())
            .collect();
        names.sort();
        names
    }

    /// Serves on 127.0.0.1 and returns the base URL.
    pub async fn start() -> (Fake, String) {
        let fake = Fake::default();
        let app = Router::new()
            .route(routes::AUTH_START, post(auth_start))
            .route(routes::AUTH_POLL, post(auth_poll))
            .route("/fake-github", get(fake_github))
            .route(routes::CONTEXT_SEARCH, post(search))
            .route("/api/context/documents/{id}/outline", get(outline))
            .route("/api/context/documents/{id}/passages", get(passages))
            .route("/api/context/documents/{id}", delete(remove))
            .route(routes::CONTEXT_INGEST, post(ingest))
            .route(routes::CONTEXT_CHANGES, get(changes))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        fake.lock().base = base.clone();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (fake, base)
    }
}

async fn auth_start(State(fake): State<Fake>, Json(req): Json<StartLoginRequest>) -> Response {
    let mut s = fake.lock();
    let login_id = format!("login{}", s.logins.len() + 1);
    s.logins
        .insert(login_id.clone(), (req.device_name, false, false));
    Json(StartLogin {
        authorize_url: format!("{}/fake-github?state={login_id}", s.base),
        login_id,
        poll_secret: POLL_SECRET.into(),
        expires_at: Utc::now() + Duration::minutes(10),
    })
    .into_response()
}

#[derive(Deserialize)]
struct GithubQuery {
    state: String,
}

/// Stands in for GitHub approving the sign-in.
async fn fake_github(State(fake): State<Fake>, Query(q): Query<GithubQuery>) -> Response {
    match fake.lock().logins.get_mut(&q.state) {
        Some(login) => {
            login.1 = true;
            "approved".into_response()
        }
        None => error(StatusCode::NOT_FOUND, "not_found", "No such sign-in."),
    }
}

async fn auth_poll(State(fake): State<Fake>, Json(req): Json<PollLoginRequest>) -> Response {
    if req.poll_secret != POLL_SECRET {
        return error(StatusCode::FORBIDDEN, "forbidden", "Wrong secret.");
    }
    let mut s = fake.lock();
    let Some(login) = s.logins.get_mut(&req.login_id) else {
        return Json(LoginState::Expired).into_response();
    };
    let state = match (login.1, login.2) {
        (false, _) => LoginState::Pending,
        (true, false) => {
            login.2 = true;
            LoginState::Approved {
                token: TOKEN.into(),
                github_login: "Nkr1shna".into(),
            }
        }
        (true, true) => LoginState::Expired,
    };
    Json(state).into_response()
}

async fn search(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Json(q): Json<ContextQuery>,
) -> Response {
    if !authorized(&headers) {
        return unauthorized();
    }
    if q.scope != Scope::Library {
        return error(
            StatusCode::BAD_REQUEST,
            "bad_scope",
            "That scope doesn't exist.",
        );
    }
    let mut s = fake.lock();
    s.searches.push(q.clone());
    let words: Vec<String> = q
        .text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2)
        .map(str::to_string)
        .collect();
    let mut hits: Vec<Hit> = Vec::new();
    for (doc, passages) in &s.docs {
        for (ord, p) in passages.iter().enumerate() {
            let text = p.text.to_lowercase();
            let score = words.iter().filter(|w| text.contains(w.as_str())).count() as f32;
            if score > 0.0 {
                hits.push(hit(doc, p, ord, score));
            }
        }
    }
    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    hits.truncate(q.k as usize);
    Json(hits).into_response()
}

async fn outline(State(fake): State<Fake>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if !authorized(&headers) {
        return unauthorized();
    }
    let s = fake.lock();
    let Some((doc, passages)) = s.docs.iter().find(|(d, _)| d.id == id) else {
        return error(StatusCode::NOT_FOUND, "not_found", "No such document.");
    };
    let mut sections: Vec<OutlineSection> = Vec::new();
    for (ord, p) in passages.iter().enumerate() {
        match sections.last_mut() {
            Some(last) if last.section_path == p.section_path => {
                last.to_ord = ord as u32;
                last.end_line = Some(p.end_line);
            }
            _ => sections.push(OutlineSection {
                section_path: p.section_path.clone(),
                from_ord: ord as u32,
                to_ord: ord as u32,
                first_page: None,
                last_page: None,
                start_line: Some(p.start_line),
                end_line: Some(p.end_line),
            }),
        }
    }
    Json(DocumentOutline {
        document_id: doc.id.clone(),
        sections,
    })
    .into_response()
}

#[derive(Deserialize)]
struct FromTo {
    from: u32,
    to: u32,
}

async fn passages(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<FromTo>,
) -> Response {
    if !authorized(&headers) {
        return unauthorized();
    }
    if q.to < q.from || (q.to - q.from + 1) as usize > TOOL_READ_PASSAGES_MAX {
        return error(StatusCode::BAD_REQUEST, "bad_range", "Too many passages.");
    }
    let mut s = fake.lock();
    s.passage_reads.push((id.clone(), q.from, q.to));
    let Some((doc, passages)) = s.docs.iter().find(|(d, _)| d.id == id) else {
        return error(StatusCode::NOT_FOUND, "not_found", "No such document.");
    };
    let hits: Vec<Hit> = passages
        .iter()
        .enumerate()
        .filter(|(ord, _)| (q.from..=q.to).contains(&(*ord as u32)))
        .map(|(ord, p)| hit(doc, p, ord, 0.0))
        .collect();
    Json(hits).into_response()
}

async fn ingest(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Json(req): Json<ContextIngest>,
) -> Response {
    if !authorized(&headers) {
        return unauthorized();
    }
    let Origin::Agent { name: agent } = &req.origin else {
        return error(
            StatusCode::BAD_REQUEST,
            "bad_origin",
            "Agents ingest as agents.",
        );
    };
    let actor = format!("agent:{agent}");
    // The same name by the same agent is replaced in place.
    let existing = {
        let s = fake.lock();
        s.docs
            .iter()
            .find(|(d, _)| d.name == req.name && d.actor == actor)
            .map(|(d, _)| d.id.clone())
    };
    let id = match existing {
        Some(id) => {
            let mut s = fake.lock();
            s.generation += 1;
            let generation = s.generation;
            let (doc, passages) = s.docs.iter_mut().find(|(d, _)| d.id == id).unwrap();
            *passages = split(&req.text);
            doc.generation = generation;
            doc.bytes = req.text.len() as u64;
            doc.status = DocumentStatus::Ready {
                passages: passages.len() as u32,
            };
            doc.updated_at = Utc::now();
            id
        }
        None => fake.add(&req.name, req.origin.clone(), &actor, &req.text),
    };
    let s = fake.lock();
    let doc = s.docs.iter().find(|(d, _)| d.id == id).unwrap().0.clone();
    Json(doc).into_response()
}

#[derive(Deserialize)]
struct Since {
    #[serde(default)]
    since: u64,
}

async fn changes(State(fake): State<Fake>, headers: HeaderMap, Query(q): Query<Since>) -> Response {
    if !authorized(&headers) {
        return unauthorized();
    }
    let s = fake.lock();
    Json(ContextChanges {
        generation: s.generation,
        changed: s
            .docs
            .iter()
            .filter(|(d, _)| d.generation > q.since)
            .map(|(d, _)| d.clone())
            .collect(),
        deleted: s
            .deleted
            .iter()
            .filter(|(g, _)| *g > q.since)
            .map(|(_, id)| id.clone())
            .collect(),
    })
    .into_response()
}

async fn remove(State(fake): State<Fake>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if !authorized(&headers) {
        return unauthorized();
    }
    let mut s = fake.lock();
    let before = s.docs.len();
    s.docs.retain(|(d, _)| d.id != id);
    if s.docs.len() == before {
        return error(StatusCode::NOT_FOUND, "not_found", "No such document.");
    }
    s.generation += 1;
    let generation = s.generation;
    s.deleted.push((generation, id));
    StatusCode::NO_CONTENT.into_response()
}
