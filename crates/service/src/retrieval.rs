//! Hybrid retrieval over `passage` rows (`docs/agent-rag-architecture.md` §2.2, §2.3):
//! query planning, the dense (HNSW) and lexical (BM25) legs, fusion, packing to a
//! budget, and the retrieval cache. One code path for lessons, the agent's `search`
//! tool family and the context API (`context_api.rs`).
//!
//! Scope and access are filters inside the index queries (the KNN scan's and the
//! full-text scan's predicate), never applied to results. A scope resolves to *keys*:
//! the ids a passage's `scope_ids` may hold (topic, chat, workspace) plus the ids of the
//! documents (and sources) it covers; a passage matches when `scope_ids CONTAINSANY
//! keys OR document_id IN keys`. `Scope::Library` is the whole project.
//!
//! Until the one-pass migration (`ingest::PASSAGES_MIGRATION`) is recorded, sources that
//! have no passages yet are searched from their old `chunk`/`media` rows (the brute
//! cosine of before, §2.2 "Migration"), so retrieval keeps working during the pass.
//!
//! Budgets are configuration: `PipelineConfig.budget` resolved against the route's
//! model window (`pack_budget`), never a literal token cap.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::Utc;
use margin_embed::Purpose;
use margin_shared::Provider;
use margin_shared::context::{
    ACCESS_COMPANY, Document, Modality, Origin, Project, RetrievalLegs, Scope, TraceKind, TraceRow,
};
use margin_shared::pipeline::{Budget, RetrievalPurpose, Route};
use margin_shared::routes::{
    RERANK_QUERY_CHARS, RETRIEVAL_K_DENSE, RETRIEVAL_K_FUSED, RETRIEVAL_K_LEX, RRF_K,
};
use margin_shared::text::for_model;
use margin_shared::{
    Chat, Citation, Event, Id, Proposal, ProposalStatus, Source, SourceKind, SourceStatus, Topic,
    Workspace, WorkspaceFile,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

use crate::db::{self, PassageRow, Store};
use crate::engines::BoxFuture;
use crate::pipeline::{
    Diversity, FusionCfg, Hit, PackBudget, Packed, Packer, Query, Registry, RerankRequest,
    RetrievalCtx, Retriever, RewriteRequest, StageCtor, hit_citation, with_retrieval,
};
use crate::sources::{ChunkRow, MediaKind, MediaRow};
use crate::state::{AppState, SOURCE_ORIGINAL, new_id};

/// The query strings one job retrieves with (§2.3 step 1): the lesson title and
/// summary, the summary alone, the planner's sub-questions, the learner's prompt for
/// lesson 1; for the agent, its query plus an optional expansion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryPlan {
    pub queries: Vec<String>,
}

impl QueryPlan {
    /// One query string.
    pub fn single(query: &str) -> Self {
        Self::from_strings([query.to_string()])
    }

    /// Non-empty, trimmed, each once, in order.
    pub fn from_strings(strings: impl IntoIterator<Item = String>) -> Self {
        let mut queries: Vec<String> = Vec::new();
        for s in strings {
            let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
            if !s.is_empty() && !queries.iter().any(|q| q.eq_ignore_ascii_case(&s)) {
                queries.push(s);
            }
        }
        Self { queries }
    }

    /// A lesson's plan: title + summary, the summary alone, the planner's questions, and
    /// the learner's request for the first lesson only.
    pub fn lesson(
        title: &str,
        summary: &str,
        planned: &[String],
        first_lesson_prompt: Option<&str>,
    ) -> Self {
        let mut strings = vec![format!("{title}\n{summary}"), summary.to_string()];
        strings.extend(planned.iter().cloned());
        if let Some(p) = first_lesson_prompt {
            strings.push(p.to_string());
        }
        Self::from_strings(strings)
    }
}

/// What a job asks retrieval for.
#[derive(Debug, Clone)]
pub struct Request {
    pub plan: QueryPlan,
    pub scope: Scope,
    /// The caller's access paths (pre-filter in the index query; `None` = no filter).
    pub access: Option<Vec<String>>,
    /// Empty means every modality.
    pub modalities: Vec<Modality>,
    /// Most passages, whatever the budget.
    pub k: usize,
    pub budget: Budget,
    /// The context window of the route's model, in tokens, which `budget` is a share of.
    pub window_tokens: usize,
    /// Tokens kept back from the share for the model's answer (`PackWindow`).
    pub reserve_output: usize,
    /// Correlates the retrieval's trace row with its turn or job.
    pub trace_id: String,
    /// An agent calling the context API from outside (`margin-mcp`): its trace row says
    /// who and keeps the query (dogfood plan §6). `None` for the app's own retrievals.
    pub caller: Option<Caller>,
    /// Why it runs (R2): the reranker and the rewriter may act per purpose.
    pub purpose: RetrievalPurpose,
    /// The chat's provider for agent searches, whose small model a model-backed stage
    /// uses; `None` = the default provider (jobs, the context API).
    pub provider: Option<Provider>,
}

/// The reranker's query (R2, `docs/r2-brief.md` D1): the plan's first string, then the
/// others as "Also asked:" lines, at most `RERANK_QUERY_CHARS` characters.
pub fn rerank_query(plan: &QueryPlan) -> String {
    let mut q = plan.queries.first().cloned().unwrap_or_default();
    for other in plan.queries.iter().skip(1) {
        q.push_str("\nAlso asked: ");
        q.push_str(other);
    }
    match q.char_indices().nth(RERANK_QUERY_CHARS) {
        Some((end, _)) => q[..end].to_string(),
        None => q,
    }
}

/// Who called the context API, for its trace rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    /// `agent:<name>`.
    pub actor: String,
    /// `margin_shared::context::VIA_MCP`.
    pub via: String,
}

impl Caller {
    /// Stamps a trace row with the caller.
    pub fn stamp(&self, row: &mut TraceRow) {
        row.actor = Some(self.actor.clone());
        row.via = Some(self.via.clone());
    }
}

/// A scope naming something that doesn't exist (the context API's 400 "bad_scope").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadScope(pub String);

impl std::fmt::Display for BadScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BadScope {}

// ---------------------------------------------------------------------------
// Scope resolution
// ---------------------------------------------------------------------------

/// A scope resolved inside the project.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolved {
    /// Ids a passage's `scope_ids` or `document_id` may hold to be in scope.
    pub keys: Vec<Id>,
    /// The whole project (`Scope::Library`).
    pub library: bool,
    /// The uploaded sources in scope (waiting for them, and the old rows of those not
    /// yet moved to passages).
    pub source_ids: Vec<Id>,
}

impl Resolved {
    fn add_key(&mut self, key: &str) {
        if !self.keys.iter().any(|k| k == key) {
            self.keys.push(key.to_string());
        }
    }

    fn add_source(&mut self, id: &str) {
        self.add_key(id);
        if !self.source_ids.iter().any(|k| k == id) {
            self.source_ids.push(id.to_string());
        }
    }

    /// Whether nothing can be in scope.
    pub fn is_empty(&self) -> bool {
        !self.library && self.keys.is_empty()
    }
}

/// What resolution reads, loaded once.
struct Catalog {
    sources: Vec<Source>,
    documents: Vec<Document>,
    topics: Vec<Topic>,
}

impl Catalog {
    async fn load(store: &Store) -> anyhow::Result<Self> {
        Ok(Self {
            sources: store.list(db::SOURCE).await?,
            documents: store.list(db::DOCUMENT).await?,
            topics: store.list(db::TOPIC).await?,
        })
    }

    /// The documents indexed from source `id`.
    fn documents_of_source<'a>(&'a self, id: &'a str) -> impl Iterator<Item = &'a Document> {
        self.documents
            .iter()
            .filter(move |d| matches!(&d.origin, Origin::Source { source_id } if source_id == id))
    }

    fn add_source(&self, out: &mut Resolved, id: &str) {
        out.add_source(id);
        for d in self.documents_of_source(id) {
            out.add_key(&d.id);
        }
    }

    fn add_workspace(&self, out: &mut Resolved, id: &str) {
        out.add_key(id);
        for s in &self.sources {
            if s.workspace_id.as_deref() == Some(id) {
                self.add_source(out, &s.id);
            }
        }
        for d in &self.documents {
            if matches!(&d.origin, Origin::FolderFile { workspace_id, .. } if workspace_id == id) {
                out.add_key(&d.id);
            }
        }
    }
}

/// `id` and every topic under it.
fn subtree(topics: &[Topic], id: &str) -> Vec<Id> {
    let mut out = vec![id.to_string()];
    let mut i = 0;
    while i < out.len() {
        let parent = out[i].clone();
        for t in topics {
            if t.parent_id.as_deref() == Some(parent.as_str()) && !out.contains(&t.id) {
                out.push(t.id.clone());
            }
        }
        i += 1;
    }
    out
}

/// Resolves `scope` with what the store holds. Unknown ids are `BadScope`.
pub async fn resolve(store: &Store, scope: &Scope) -> anyhow::Result<Resolved> {
    let catalog = Catalog::load(store).await?;
    let mut out = Resolved::default();
    resolve_into(store, &catalog, scope, true, &mut out).await?;
    Ok(out)
}

/// `resolve` for the index query itself: an id with no record behind it is kept as a
/// key (`id IN scope_ids`, `document_id IN ids`) instead of refused, so passages filed
/// under a scope id alone (the eval index's topic and folder) are found. Callers check
/// the scope with `resolve` first.
pub async fn resolve_for_index(store: &Store, scope: &Scope) -> anyhow::Result<Resolved> {
    let catalog = Catalog::load(store).await?;
    let mut out = Resolved::default();
    resolve_into(store, &catalog, scope, false, &mut out).await?;
    Ok(out)
}

fn resolve_into<'a>(
    store: &'a Store,
    catalog: &'a Catalog,
    scope: &'a Scope,
    strict: bool,
    out: &'a mut Resolved,
) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
        match scope {
            Scope::Sources { ids } => {
                for id in ids {
                    if catalog.sources.iter().any(|s| &s.id == id) {
                        catalog.add_source(out, id);
                    } else if let Some(d) = catalog.documents.iter().find(|d| &d.id == id) {
                        out.add_key(&d.id);
                        if let Origin::Source { source_id } = &d.origin {
                            catalog.add_source(out, source_id);
                        }
                    } else if strict {
                        return Err(
                            BadScope(format!("There is no source or document {id}.")).into()
                        );
                    } else {
                        out.add_key(id);
                    }
                }
            }
            Scope::Topic { id } => {
                if strict && !catalog.topics.iter().any(|t| &t.id == id) {
                    return Err(BadScope(format!("There is no topic {id}.")).into());
                }
                let within = subtree(&catalog.topics, id);
                for t in &within {
                    out.add_key(t);
                }
                for s in &catalog.sources {
                    if s.topic_id.as_ref().is_some_and(|t| within.contains(t)) {
                        catalog.add_source(out, &s.id);
                    }
                }
            }
            Scope::Chat { id } => {
                let Some(chat) = store.get::<Chat>(db::CHAT, id).await? else {
                    if strict {
                        return Err(BadScope(format!("There is no chat {id}.")).into());
                    }
                    out.add_key(id);
                    return Ok(());
                };
                out.add_key(id);
                for s in &chat.source_ids {
                    catalog.add_source(out, s);
                }
                for w in &chat.workspace_ids {
                    catalog.add_workspace(out, w);
                }
            }
            Scope::Workspace { id } => {
                if strict && store.get::<Workspace>(db::WORKSPACE, id).await?.is_none() {
                    return Err(BadScope(format!("There is no folder {id}.")).into());
                }
                catalog.add_workspace(out, id);
            }
            Scope::Library => {
                out.library = true;
                for s in &catalog.sources {
                    out.add_source(&s.id);
                }
            }
            Scope::Union { scopes } => {
                for s in scopes {
                    resolve_into(store, catalog, s, strict, out).await?;
                }
            }
        }
        Ok(())
    })
}

/// The ids a scope resolves to inside the project (topic → its sources and the chats
/// filed under it, chat → its files and folders, …). Unknown ids are an error the
/// context API answers with 400 "bad_scope". `Scope::Library` resolves to the project's
/// id.
pub async fn resolve_scope(state: &AppState, scope: &Scope) -> anyhow::Result<Vec<Id>> {
    let resolved = resolve(&state.store, scope).await?;
    if resolved.library {
        let project = project(&state.store).await?;
        let mut keys = vec![project.id];
        keys.extend(resolved.keys);
        return Ok(keys);
    }
    Ok(resolved.keys)
}

/// The topics a chat works in: those of its attached files, and those its accepted
/// proposals became.
pub async fn chat_topics(store: &Store, chat: &Chat) -> anyhow::Result<Vec<Id>> {
    let mut topics: Vec<Id> = Vec::new();
    for id in &chat.source_ids {
        if let Some(s) = store.get::<Source>(db::SOURCE, id).await?
            && let Some(t) = s.topic_id
            && !topics.contains(&t)
        {
            topics.push(t);
        }
    }
    let proposals: Vec<Proposal> = store.list_where(db::PROPOSAL, "chat_id", &chat.id).await?;
    let known: Vec<Topic> = store.list(db::TOPIC).await?;
    for p in proposals {
        if let ProposalStatus::Accepted { topic_id } = p.status
            && !topics.contains(&topic_id)
            && known.iter().any(|t| t.id == topic_id)
        {
            topics.push(topic_id);
        }
    }
    Ok(topics)
}

/// The agent's default scope (owner Q1): the chat's files and folders, then its topics;
/// never every source.
pub async fn chat_scope(store: &Store, chat: &Chat) -> anyhow::Result<Scope> {
    let mut scopes = vec![Scope::Chat {
        id: chat.id.clone(),
    }];
    for id in chat_topics(store, chat).await? {
        scopes.push(Scope::Topic { id });
    }
    Ok(match scopes.len() {
        1 => scopes.remove(0),
        _ => Scope::Union { scopes },
    })
}

// ---------------------------------------------------------------------------
// The project
// ---------------------------------------------------------------------------

/// The id of the project created when none exists yet.
pub const DEFAULT_PROJECT_ID: &str = "my-library";

/// The project every scope resolves inside (the oldest one); created as "My library"
/// when there is none.
pub async fn project(store: &Store) -> anyhow::Result<Project> {
    let mut projects: Vec<Project> = store.list(db::PROJECT).await?;
    projects.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    if let Some(p) = projects.into_iter().next() {
        return Ok(p);
    }
    let p = Project {
        id: DEFAULT_PROJECT_ID.into(),
        name: crate::context_api::DEFAULT_PROJECT_NAME.into(),
        created_at: Utc::now(),
    };
    store.put(db::PROJECT, &p.id, &p).await?;
    Ok(p)
}

// ---------------------------------------------------------------------------
// Access (the seam; not enforced for the owner)
// ---------------------------------------------------------------------------

/// Whether a reader with `paths` may see something whose access path is `path`: one of
/// the reader's paths is `path` or a prefix of it ("company" sees "company/eng").
pub fn may_see(paths: &[String], path: &str) -> bool {
    paths.iter().any(|r| {
        path == r
            || path
                .strip_prefix(r.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// The access paths a reader with `paths` may see, among those the project's documents
/// use (plus "company" and the reader's own): what `access CONTAINSANY` is given, so the
/// filter stays an index condition.
async fn visible_paths(store: &Store, paths: &[String]) -> anyhow::Result<Vec<String>> {
    let documents: Vec<Document> = store.list(db::DOCUMENT).await?;
    let mut known: Vec<String> = vec![ACCESS_COMPANY.to_string()];
    known.extend(documents.into_iter().flat_map(|d| d.access));
    known.extend(paths.iter().cloned());
    let mut out: Vec<String> = Vec::new();
    for p in known {
        if may_see(paths, &p) && !out.contains(&p) {
            out.push(p);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// The index legs
// ---------------------------------------------------------------------------

/// The `WHERE` conditions every leg shares, with their bindings.
#[derive(Debug, Clone, Default)]
struct Filter {
    resolved: Resolved,
    modalities: Vec<String>,
    /// Visible access paths; `None` = no access filter.
    access: Option<Vec<String>>,
    /// The query vector's embedder version: the dense leg's `embedder = $embedder`
    /// (empty = none).
    embedder: String,
}

impl Filter {
    async fn new(store: &Store, q: &Query) -> anyhow::Result<Self> {
        let resolved = resolve_for_index(store, &q.scope).await?;
        let access = match &q.access {
            Some(paths) => Some(visible_paths(store, paths).await?),
            None => None,
        };
        Ok(Self {
            resolved,
            modalities: q
                .modalities
                .iter()
                .map(|m| m.as_str().to_string())
                .collect(),
            access,
            embedder: q.embedder.clone(),
        })
    }

    /// Whether the filter lets nothing through.
    fn is_empty(&self) -> bool {
        self.resolved.is_empty() || self.access.as_ref().is_some_and(|a| a.is_empty())
    }

    /// ` AND …` conditions for a `passage` query.
    fn sql(&self) -> String {
        let mut sql = String::new();
        if !self.resolved.library {
            sql.push_str(" AND (scope_ids CONTAINSANY $keys OR document_id IN $keys)");
        }
        if !self.modalities.is_empty() {
            sql.push_str(" AND modality IN $modalities");
        }
        if self.access.is_some() {
            sql.push_str(" AND access CONTAINSANY $paths");
        }
        sql
    }

    fn binds(&self) -> Vec<(&'static str, Value)> {
        let mut binds = vec![
            ("keys", json!(self.resolved.keys)),
            ("modalities", json!(self.modalities)),
        ];
        if let Some(paths) = &self.access {
            binds.push(("paths", json!(paths)));
        }
        if !self.embedder.is_empty() {
            binds.push(("embedder", json!(self.embedder)));
        }
        binds
    }

    /// ` AND …` conditions for the dense leg: the shared ones, and only vectors of the
    /// query's embedder (one more pre-filter conjunct inside the KNN scan).
    fn dense_sql(&self) -> String {
        let mut sql = self.sql();
        if !self.embedder.is_empty() {
            sql.push_str(" AND embedder = $embedder");
        }
        sql
    }
}

/// Which legs an index retriever runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Legs {
    Hybrid,
    /// Both legs, each min-max normalised and summed with `dense_weight` and
    /// `lex_weight` (`"hybrid-linear"`, `evals/variants/hybrid.toml`).
    HybridLinear,
    Dense,
    Lexical,
}

/// The index retrievers: `"hybrid-rrf"` (both legs fused with `search::rrf` in one
/// statement, §2.2), `"dense"` (HNSW only) and `"bm25"` (FULLTEXT only).
pub struct IndexRetriever {
    store: Store,
    legs: Legs,
    k_dense: usize,
    k_lex: usize,
    rrf_k: u32,
    /// HNSW's search breadth (`<|k, ef|>`).
    ef: usize,
    /// The legs' weights in `"hybrid-linear"`.
    dense_weight: f32,
    lex_weight: f32,
}

/// A passage row and what the legs said about it.
#[derive(Debug, Deserialize)]
struct RankedRow {
    #[serde(flatten)]
    row: PassageRow,
    #[serde(default)]
    d: Option<f64>,
    #[serde(default)]
    s: Option<f64>,
    #[serde(default)]
    highlight: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct FusedRef {
    pid: String,
    rrf_score: f64,
    #[serde(default)]
    highlight: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct HybridOut {
    #[serde(default)]
    fused: Vec<FusedRef>,
    #[serde(default)]
    dense: Vec<String>,
    #[serde(default)]
    lex: Vec<String>,
    #[serde(default)]
    rows: Vec<PassageRow>,
}

/// Marks around a full-text match in `Hit.highlight`.
pub const HIGHLIGHT_OPEN: &str = "**";
pub const HIGHLIGHT_CLOSE: &str = "**";

fn highlight_text(v: Option<Value>) -> Option<String> {
    match v? {
        Value::String(s) if !s.is_empty() => Some(s),
        _ => None,
    }
}

fn param_usize(params: &serde_json::Map<String, Value>, key: &str, default: usize) -> usize {
    params
        .get(key)
        .and_then(Value::as_u64)
        .map_or(default, |v| v as usize)
}

fn param_f32(params: &serde_json::Map<String, Value>, key: &str, default: f32) -> f32 {
    params
        .get(key)
        .and_then(Value::as_f64)
        .map_or(default, |v| v as f32)
}

impl IndexRetriever {
    fn new(store: Store, legs: Legs, params: &serde_json::Map<String, Value>) -> Self {
        Self {
            store,
            legs,
            k_dense: param_usize(params, "k_dense", RETRIEVAL_K_DENSE).max(1),
            k_lex: param_usize(params, "k_lex", RETRIEVAL_K_LEX).max(1),
            rrf_k: param_usize(params, "rrf_k", RRF_K as usize) as u32,
            ef: param_usize(params, "ef", 128).max(1),
            dense_weight: param_f32(params, "dense_weight", 1.0),
            lex_weight: param_f32(params, "lex_weight", 1.0),
        }
    }

    /// `"hybrid-linear"`: both legs, each min-max normalised to 0–1, summed with their
    /// weights; a passage only one leg found gets 0 from the other.
    async fn hybrid_linear(&self, filter: &Filter, q: &Query) -> anyhow::Result<Vec<Hit>> {
        let dense = self.dense(filter, q).await?;
        let lex = self.lexical(filter, q).await?;
        let normalised = |leg: &[Hit]| -> Vec<f32> {
            let (lo, hi) = leg.iter().fold((f32::MAX, f32::MIN), |(lo, hi), h| {
                (lo.min(h.score), hi.max(h.score))
            });
            leg.iter()
                .map(|h| {
                    if hi > lo {
                        (h.score - lo) / (hi - lo)
                    } else {
                        1.0
                    }
                })
                .collect()
        };
        let (dn, ln) = (normalised(&dense), normalised(&lex));
        let mut fused: Vec<Hit> = Vec::new();
        let mut at: HashMap<String, usize> = HashMap::new();
        for (hit, n) in dense.into_iter().zip(dn) {
            at.insert(hit.passage.id.clone(), fused.len());
            fused.push(Hit {
                score: self.dense_weight * n,
                ..hit
            });
        }
        for (hit, n) in lex.into_iter().zip(ln) {
            match at.get(&hit.passage.id) {
                Some(&i) => {
                    let f = &mut fused[i];
                    f.score += self.lex_weight * n;
                    f.lex_rank = hit.lex_rank;
                    f.highlight = hit.highlight;
                }
                None => fused.push(Hit {
                    score: self.lex_weight * n,
                    ..hit
                }),
            }
        }
        fused.sort_by(|a, b| b.score.total_cmp(&a.score));
        Ok(fused)
    }

    fn dense_sql(&self, filter: &Filter, k: usize, fields: &str) -> String {
        format!(
            "SELECT {fields}, vector::distance::knn() AS d FROM passage \
             WHERE vector <|{k},{ef}|> $q{cond} ORDER BY d",
            ef = self.ef.max(k),
            cond = filter.dense_sql(),
        )
    }

    fn lex_sql(&self, filter: &Filter, k: usize, fields: &str) -> String {
        format!(
            "SELECT {fields}, search::score(1) AS s, \
             search::highlight('{HIGHLIGHT_OPEN}', '{HIGHLIGHT_CLOSE}', 1) AS highlight \
             FROM passage WHERE text @1@ $text{cond} ORDER BY s DESC LIMIT {k}",
            cond = filter.sql(),
        )
    }

    async fn dense(&self, filter: &Filter, q: &Query) -> anyhow::Result<Vec<Hit>> {
        let sql = self.dense_sql(filter, self.k_dense, "*, record::id(id) AS id");
        let mut binds = filter.binds();
        binds.push(("q", json!(q.vector)));
        let rows: Vec<RankedRow> = self.store.query_rows(&sql, binds).await?;
        Ok(rows
            .into_iter()
            .enumerate()
            .map(|(i, r)| Hit {
                score: 1.0 - r.d.unwrap_or(1.0) as f32,
                passage: r.row,
                dense_rank: Some(i as u32),
                lex_rank: None,
                rerank: None,
                highlight: None,
            })
            .collect())
    }

    async fn lexical(&self, filter: &Filter, q: &Query) -> anyhow::Result<Vec<Hit>> {
        let sql = self.lex_sql(filter, self.k_lex, "*, record::id(id) AS id");
        let mut binds = filter.binds();
        binds.push(("text", json!(q.text)));
        let rows: Vec<RankedRow> = self.store.query_rows(&sql, binds).await?;
        Ok(rows
            .into_iter()
            .enumerate()
            .map(|(i, r)| Hit {
                score: r.s.unwrap_or(0.0) as f32,
                passage: r.row,
                dense_rank: None,
                lex_rank: Some(i as u32),
                rerank: None,
                highlight: highlight_text(r.highlight),
            })
            .collect())
    }

    async fn hybrid(&self, filter: &Filter, q: &Query, k_fused: usize) -> anyhow::Result<Vec<Hit>> {
        let fields = "id, record::id(id) AS pid";
        let sql = format!(
            "LET $dense = {dense}; LET $lex = {lex}; \
             LET $fused = search::rrf([$dense, $lex], {k_fused}, {rrf_k}); \
             RETURN {{ fused: (SELECT pid, rrf_score, highlight FROM $fused), \
               dense: $dense.pid, lex: $lex.pid, \
               rows: (SELECT *, record::id(id) AS id FROM passage WHERE id IN $fused.id) }};",
            dense = self.dense_sql(filter, self.k_dense, fields),
            lex = self.lex_sql(filter, self.k_lex, fields),
            rrf_k = self.rrf_k,
        );
        let mut binds = filter.binds();
        binds.push(("q", json!(q.vector)));
        binds.push(("text", json!(q.text)));
        let out: Vec<HybridOut> = self.store.query_rows(&sql, binds).await?;
        let Some(out) = out.into_iter().next() else {
            return Ok(Vec::new());
        };
        let mut rows: HashMap<String, PassageRow> =
            out.rows.into_iter().map(|r| (r.id.clone(), r)).collect();
        let rank = |list: &[String], id: &str| list.iter().position(|x| x == id).map(|i| i as u32);
        Ok(out
            .fused
            .into_iter()
            .filter_map(|f| {
                let passage = rows.remove(&f.pid)?;
                Some(Hit {
                    score: f.rrf_score as f32,
                    dense_rank: rank(&out.dense, &f.pid),
                    lex_rank: rank(&out.lex, &f.pid),
                    rerank: None,
                    highlight: highlight_text(f.highlight),
                    passage,
                })
            })
            .collect())
    }
}

impl Retriever for IndexRetriever {
    fn name(&self) -> &'static str {
        match self.legs {
            Legs::Hybrid => "hybrid-rrf",
            Legs::HybridLinear => "hybrid-linear",
            Legs::Dense => "dense",
            Legs::Lexical => "bm25",
        }
    }

    fn search<'a>(&'a self, q: &'a Query) -> BoxFuture<'a, anyhow::Result<Vec<Hit>>> {
        Box::pin(async move {
            let filter = Filter::new(&self.store, q).await?;
            if filter.is_empty() {
                return Ok(Vec::new());
            }
            let has_vector = !q.vector.is_empty();
            let has_text = !q.text.trim().is_empty();
            let k = q.k.max(1);
            let mut hits = match (self.legs, has_vector, has_text) {
                (Legs::Hybrid, true, true) => self.hybrid(&filter, q, k).await?,
                (Legs::HybridLinear, true, true) => self.hybrid_linear(&filter, q).await?,
                (Legs::Hybrid | Legs::HybridLinear | Legs::Dense, true, _) => {
                    self.dense(&filter, q).await?
                }
                (Legs::Hybrid | Legs::HybridLinear | Legs::Lexical, _, true) => {
                    self.lexical(&filter, q).await?
                }
                _ => Vec::new(),
            };
            hits.truncate(k);
            Ok(hits)
        })
    }
}

// ---------------------------------------------------------------------------
// Fusion across query strings
// ---------------------------------------------------------------------------

/// Reciprocal rank fusion across query strings (max score kept per passage).
pub struct Rrf;

/// Keeps, for one passage seen in several lists, the best ranks and the highlight.
fn merge_into(best: &mut Hit, other: &Hit) {
    best.dense_rank = match (best.dense_rank, other.dense_rank) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    best.lex_rank = match (best.lex_rank, other.lex_rank) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    if best.highlight.is_none() {
        best.highlight.clone_from(&other.highlight);
    }
}

impl crate::pipeline::Fusion for Rrf {
    fn name(&self) -> &'static str {
        "rrf"
    }

    /// One list passes as it is. Several (one per query string): each passage scores
    /// `Σ 1 / (rrf_k + rank)` over the lists it is in (rank from 1), and keeps the hit
    /// with its best ranks.
    fn fuse(&self, legs: Vec<Vec<Hit>>, cfg: &FusionCfg) -> Vec<Hit> {
        let mut legs: Vec<Vec<Hit>> = legs.into_iter().filter(|l| !l.is_empty()).collect();
        if legs.len() <= 1 {
            let mut hits = legs.pop().unwrap_or_default();
            hits.truncate(cfg.k_fused);
            return hits;
        }
        let mut fused: Vec<(f32, Hit)> = Vec::new();
        let mut at: HashMap<String, usize> = HashMap::new();
        for leg in legs {
            for (rank, hit) in leg.into_iter().enumerate() {
                let add = 1.0 / (cfg.rrf_k as f32 + rank as f32 + 1.0);
                match at.get(&hit.passage.id) {
                    Some(&i) => {
                        fused[i].0 += add;
                        merge_into(&mut fused[i].1, &hit);
                    }
                    None => {
                        at.insert(hit.passage.id.clone(), fused.len());
                        fused.push((add, hit));
                    }
                }
            }
        }
        fused.sort_by(|a, b| b.0.total_cmp(&a.0));
        fused
            .into_iter()
            .take(cfg.k_fused)
            .map(|(score, mut hit)| {
                hit.score = score;
                hit
            })
            .collect()
    }
}

/// Weighted sum of each list's min-max normalised scores (`params.weights`, one per
/// list, default 1.0 each).
pub struct Linear;

impl crate::pipeline::Fusion for Linear {
    fn name(&self) -> &'static str {
        "linear"
    }

    fn fuse(&self, legs: Vec<Vec<Hit>>, cfg: &FusionCfg) -> Vec<Hit> {
        let weights: Vec<f32> = cfg
            .params
            .get("weights")
            .and_then(Value::as_array)
            .map(|w| w.iter().map(|x| x.as_f64().unwrap_or(1.0) as f32).collect())
            .unwrap_or_default();
        let mut fused: Vec<(f32, Hit)> = Vec::new();
        let mut at: HashMap<String, usize> = HashMap::new();
        for (i, leg) in legs.into_iter().enumerate() {
            let weight = weights.get(i).copied().unwrap_or(1.0);
            let (lo, hi) = leg.iter().fold((f32::MAX, f32::MIN), |(lo, hi), h| {
                (lo.min(h.score), hi.max(h.score))
            });
            for hit in leg {
                let norm = if hi > lo {
                    (hit.score - lo) / (hi - lo)
                } else {
                    1.0
                };
                let add = weight * norm;
                match at.get(&hit.passage.id) {
                    Some(&j) => {
                        fused[j].0 += add;
                        merge_into(&mut fused[j].1, &hit);
                    }
                    None => {
                        at.insert(hit.passage.id.clone(), fused.len());
                        fused.push((add, hit));
                    }
                }
            }
        }
        fused.sort_by(|a, b| b.0.total_cmp(&a.0));
        fused
            .into_iter()
            .take(cfg.k_fused)
            .map(|(score, mut hit)| {
                hit.score = score;
                hit
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Packing
// ---------------------------------------------------------------------------

/// What one picture costs in a prompt: a 1536-px picture is about 1 100 input tokens on
/// OpenAI's high detail and about 1 600 on Claude (`w × h / 750` after its resize), so
/// the larger is counted. `packer.params.picture_tokens` overrides it.
pub const PICTURE_TOKENS: usize = 1_600;

/// Tokens `text` takes, counted high: about four bytes make a token of English with
/// the Gemma tokenizer and the providers' ones, three is the safe side for code and
/// other languages.
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(3)
}

/// Unit dedup, adjacent merge, MMR with a per-document cap, the score percentile and
/// the token budget (§2.3 step 4).
pub struct MmrV1 {
    pub picture_tokens: usize,
    /// `params.lambda`: MMR's relevance weight instead of the request's.
    pub lambda: Option<f32>,
    /// `params.per_document_max` instead of the request's (`ceil(k / 2)` when neither).
    pub per_document_max: Option<usize>,
}

impl Default for MmrV1 {
    fn default() -> Self {
        Self {
            picture_tokens: PICTURE_TOKENS,
            lambda: None,
            per_document_max: None,
        }
    }
}

impl MmrV1 {
    /// From the packer stage's parameters.
    pub fn from_params(params: &serde_json::Map<String, Value>) -> Self {
        Self {
            picture_tokens: param_usize(params, "picture_tokens", PICTURE_TOKENS),
            lambda: params
                .contains_key("lambda")
                .then(|| param_f32(params, "lambda", margin_shared::routes::MMR_LAMBDA)),
            per_document_max: params
                .contains_key("per_document_max")
                .then(|| param_usize(params, "per_document_max", 1)),
        }
    }
}

/// One thing the packer may pick: a passage, possibly with the picture of its unit.
#[derive(Debug, Clone)]
struct Candidate {
    hit: Hit,
    /// The unit's picture, attached when it outranked the unit's text.
    picture: Option<Hit>,
    tokens: usize,
}

impl Candidate {
    fn has_picture(&self) -> bool {
        self.picture.is_some() || is_picture_like(&self.hit)
    }
    fn score(&self) -> f32 {
        self.picture
            .as_ref()
            .map_or(self.hit.score, |p| p.score.max(self.hit.score))
    }
}

fn is_text(h: &Hit) -> bool {
    h.passage.modality == Modality::Text.as_str()
}

fn is_picture_like(h: &Hit) -> bool {
    h.passage.file.is_some() && !is_text(h)
}

/// The score at `pct` percent of `scores` (nearest rank, ascending).
fn percentile(scores: &[f32], pct: u8) -> f32 {
    if scores.is_empty() {
        return f32::MIN;
    }
    let mut sorted = scores.to_vec();
    sorted.sort_by(f32::total_cmp);
    let pct = pct.min(100) as usize;
    let i = (pct * (sorted.len() - 1)) / 100;
    sorted[i]
}

/// The longest end of `a` that `b` starts with (the chunker's sentence overlap), in
/// bytes, at a char boundary of both.
fn overlap(a: &str, b: &str) -> usize {
    let max = a.len().min(b.len());
    (1..=max)
        .rev()
        .find(|&n| a.is_char_boundary(a.len() - n) && b.is_char_boundary(n) && a.ends_with(&b[..n]))
        .filter(|&n| n >= 8 || b[..n].trim().is_empty())
        .unwrap_or(0)
}

impl MmrV1 {
    fn candidate(&self, hit: Hit, picture: Option<Hit>) -> Candidate {
        let mut tokens = estimate_tokens(&hit.passage.text);
        if picture.is_some() || is_picture_like(&hit) {
            tokens += self.picture_tokens;
        }
        Candidate {
            hit,
            picture,
            tokens,
        }
    }

    /// Drops near-duplicates by unit: a picture whose unit has text goes with the best
    /// text of the unit only when it outranked it; a unit of pictures alone keeps its
    /// best picture.
    fn by_unit(&self, hits: Vec<Hit>) -> Vec<Candidate> {
        let mut best_text: HashMap<String, f32> = HashMap::new();
        let mut best_picture: HashMap<String, Hit> = HashMap::new();
        for h in &hits {
            let unit = h.passage.unit_id.clone();
            if is_text(h) {
                let e = best_text.entry(unit).or_insert(f32::MIN);
                *e = e.max(h.score);
            } else {
                match best_picture.get(&unit) {
                    Some(b) if b.score >= h.score => {}
                    _ => {
                        best_picture.insert(unit, h.clone());
                    }
                }
            }
        }
        let mut out = Vec::new();
        let mut picture_given: HashSet<String> = HashSet::new();
        for h in hits {
            let unit = h.passage.unit_id.clone();
            if is_text(&h) {
                let top = best_text.get(&unit).copied().unwrap_or(f32::MIN);
                let picture = best_picture
                    .get(&unit)
                    .filter(|p| p.score > top && h.score >= top && is_picture_like(p))
                    .filter(|_| picture_given.insert(unit.clone()))
                    .cloned();
                out.push(self.candidate(h, picture));
            } else if !best_text.contains_key(&unit)
                && best_picture
                    .get(&unit)
                    .is_some_and(|b| b.passage.id == h.passage.id)
            {
                out.push(self.candidate(h, None));
            }
        }
        out
    }

    /// Merges selected text passages of one document (and one page) whose `ord`s follow
    /// each other.
    fn merge_adjacent(&self, picked: Vec<Candidate>) -> Vec<Packed> {
        // Groups of picks, in pick order; a pick joins a group of its document whose
        // ord range it extends by one.
        let mut groups: Vec<Vec<Candidate>> = Vec::new();
        'next: for c in picked {
            let mergeable = is_text(&c.hit) && c.picture.is_none();
            if mergeable {
                for g in &mut groups {
                    let first = &g[0];
                    // One locator per passage: text of another page isn't merged in
                    // (its citation would point at the wrong page).
                    if first.hit.passage.document_id != c.hit.passage.document_id
                        || first.hit.passage.page != c.hit.passage.page
                        || !is_text(&first.hit)
                        || g.iter().any(|x| x.picture.is_some())
                    {
                        continue;
                    }
                    let lo = g.iter().map(|x| x.hit.passage.ord).min().unwrap_or(0);
                    let hi = g.iter().map(|x| x.hit.passage.ord).max().unwrap_or(0);
                    let ord = c.hit.passage.ord;
                    if ord == lo - 1 || ord == hi + 1 {
                        g.push(c);
                        continue 'next;
                    }
                }
            }
            groups.push(vec![c]);
        }
        let mut packed: Vec<(f32, Packed)> = groups
            .into_iter()
            .map(|mut g| {
                let best = g.iter().map(Candidate::score).fold(f32::MIN, f32::max);
                let sum: usize = g.iter().map(|c| c.tokens).sum();
                // Best first for `hits`; reading order for the text and the locator.
                let mut by_ord = g.clone();
                by_ord.sort_by_key(|c| c.hit.passage.ord);
                g.sort_by(|a, b| b.score().total_cmp(&a.score()));
                let mut text = String::new();
                for c in &by_ord {
                    let t = c.hit.passage.text.trim();
                    if text.is_empty() {
                        text.push_str(t);
                    } else {
                        let cut = overlap(&text, t);
                        let rest = t[cut..].trim_start();
                        if !rest.is_empty() {
                            text.push('\n');
                            text.push_str(rest);
                        }
                    }
                }
                let first = &by_ord[0];
                let mut citation = hit_citation(&first.hit);
                if let Some(p) = &first.picture {
                    citation.image_url.clone_from(&p.passage.file);
                }
                let mut hits: Vec<Hit> = Vec::new();
                for c in g {
                    hits.push(c.hit);
                    if let Some(p) = c.picture {
                        hits.push(p);
                    }
                }
                let tokens = if by_ord.len() == 1 {
                    sum
                } else {
                    estimate_tokens(&text).min(sum)
                };
                (
                    best,
                    Packed {
                        hits,
                        text,
                        tokens,
                        citation,
                    },
                )
            })
            .collect();
        packed.sort_by(|a, b| b.0.total_cmp(&a.0));
        packed.into_iter().map(|(_, p)| p).collect()
    }
}

impl Packer for MmrV1 {
    fn name(&self) -> &'static str {
        "mmr-v1"
    }

    fn pack(&self, hits: Vec<Hit>, budget: &PackBudget) -> Vec<Packed> {
        if hits.is_empty() || budget.k == 0 {
            return Vec::new();
        }
        // The score threshold first: weak retrieval gives fewer passages, not padding.
        let mut hits = hits;
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        let scores: Vec<f32> = hits.iter().map(|h| h.score).collect();
        let cutoff = percentile(&scores, budget.min_score_pct);
        let keep = hits
            .iter()
            .filter(|h| h.score >= cutoff)
            .count()
            .max(budget.passages_min.min(hits.len()));
        hits.truncate(keep);

        let mut remaining = drop_spoken_overlaps(self.by_unit(hits));
        let top = remaining
            .iter()
            .map(Candidate::score)
            .fold(f32::MIN, f32::max);
        let low = remaining
            .iter()
            .map(Candidate::score)
            .fold(f32::MAX, f32::min);
        let relevance = |c: &Candidate| {
            if top > low {
                (c.score() - low) / (top - low)
            } else {
                1.0
            }
        };
        let lambda = self
            .lambda
            .unwrap_or(budget.diversity.lambda)
            .clamp(0.0, 1.0);
        let per_doc = self
            .per_document_max
            .or(budget.diversity.per_document_max)
            .unwrap_or_else(|| budget.k.div_ceil(2))
            .max(1);

        let mut picked: Vec<Candidate> = Vec::new();
        let mut per_document: HashMap<String, usize> = HashMap::new();
        let mut tokens = 0usize;
        let mut images = 0usize;
        while picked.len() < budget.k && !remaining.is_empty() {
            let (i, _) = remaining
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let similar = picked
                        .iter()
                        .map(|p| similarity(&c.hit.passage.vector, &p.hit.passage.vector))
                        .fold(0.0f32, f32::max);
                    (i, lambda * relevance(c) - (1.0 - lambda) * similar)
                })
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .expect("not empty");
            let mut c = remaining.remove(i);
            let doc = c.hit.passage.document_id.clone();
            if per_document.get(&doc).copied().unwrap_or(0) >= per_doc {
                continue;
            }
            if c.has_picture() && images >= budget.images_max {
                match c.picture.take() {
                    // Text with its unit's picture: the text alone still counts.
                    Some(_) => c.tokens -= self.picture_tokens,
                    None => continue,
                }
            }
            if tokens + c.tokens > budget.tokens {
                continue;
            }
            tokens += c.tokens;
            if c.has_picture() {
                images += 1;
            }
            *per_document.entry(doc).or_default() += 1;
            picked.push(c);
        }
        self.merge_adjacent(picked)
    }
}

/// The stretch of a recording a passage is read as (a transcript passage or a sound
/// window; not a frame, which is a picture).
fn spoken_range(h: &Hit) -> Option<(i64, i64)> {
    if h.passage.modality == Modality::Frame.as_str() {
        return None;
    }
    Some((h.passage.start_ms?, h.passage.end_ms?))
}

/// Whether two stretches overlap by more than half of the shorter one.
fn mostly_overlap(a: (i64, i64), b: (i64, i64)) -> bool {
    let overlap = (a.1.min(b.1) - a.0.max(b.0)).max(0);
    let shorter = (a.1 - a.0).min(b.1 - b.0).max(1);
    overlap * 2 > shorter
}

/// Drops a stretch of a recording that mostly repeats a better-ranked one of the same
/// document (a transcript passage and the sound windows it spans say the same words).
fn drop_spoken_overlaps(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut kept: Vec<(String, (i64, i64))> = Vec::new();
    let mut out = Vec::with_capacity(candidates.len());
    for c in candidates {
        if let Some(range) = spoken_range(&c.hit) {
            let doc = &c.hit.passage.document_id;
            if kept
                .iter()
                .any(|(d, r)| d == doc && mostly_overlap(*r, range))
            {
                continue;
            }
            kept.push((doc.clone(), range));
        }
        out.push(c);
    }
    out
}

fn similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    margin_embed::cosine(a, b)
}

/// Candidates kept after fusion under `cfg` (`fusion.params.k_fused`, else
/// `RETRIEVAL_K_FUSED`): the most passages a generation job asks for, the budget
/// deciding how many of them go in.
pub fn fused_k(cfg: &margin_shared::pipeline::PipelineConfig) -> usize {
    param_usize(&cfg.fusion.params, "k_fused", RETRIEVAL_K_FUSED).max(1)
}

/// `PackBudget` for `budget` against a model window of `window_tokens`, nothing
/// reserved for the answer (an eval variant pinning `window_tokens`).
pub fn pack_budget(budget: &Budget, window_tokens: usize, k: usize) -> PackBudget {
    pack_budget_reserving(budget, window_tokens, 0, k)
}

/// `PackBudget` for `budget` against a model window of `window_tokens`, less the
/// `reserve_output` tokens the answer may take (D13):
/// `floor(window × share_of_window) − reserve_output`.
pub fn pack_budget_reserving(
    budget: &Budget,
    window_tokens: usize,
    reserve_output: usize,
    k: usize,
) -> PackBudget {
    let share = budget.share_of_window.clamp(0.0, 1.0) as f64;
    PackBudget {
        tokens: ((window_tokens as f64 * share) as usize).saturating_sub(reserve_output),
        images_max: budget.images_max as usize,
        min_score_pct: budget.min_score_pct.min(100),
        passages_min: budget.passages_min as usize,
        k,
        diversity: Diversity::default(),
    }
}

// ---------------------------------------------------------------------------
// Model windows
// ---------------------------------------------------------------------------

/// The window a route's passages are a share of, and the tokens kept back for the
/// model's answer (D13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackWindow {
    pub tokens: usize,
    pub reserve_output: usize,
}

/// The output a call on `route` may take, kept out of the packed passages: on Claude
/// `min(window.output, the client's max_tokens)` (`CLAUDE_MAX_OUTPUT_TOKENS` for chat
/// steps, `CLAUDE_TEXT_MAX_OUTPUT_TOKENS` for text calls); on OpenAI and ChatGPT the
/// model's own output cap, which the Responses client asks for.
pub fn output_reserve(provider: Provider, route: Route, window: margin_ai::ModelWindow) -> usize {
    let cap = match (provider, route) {
        (Provider::Claude, Route::Chat) => window.output.min(margin_ai::CLAUDE_MAX_OUTPUT_TOKENS),
        (Provider::Claude, _) => window.output.min(margin_ai::CLAUDE_TEXT_MAX_OUTPUT_TOKENS),
        (Provider::OpenAi | Provider::ChatGpt, _) => window.output,
    };
    cap as usize
}

/// The window `budget` is a share of for `route` on `provider` (the default provider
/// when `None`): the packer's `window_tokens` param when the pipeline sets one (an eval
/// variant pins it; nothing is reserved then), else the input window of the route's
/// model (`model_for`) less [`output_reserve`]. When no model is configured for the
/// route, `MODEL_WINDOW_FALLBACK` (logged), so the context API still answers.
pub async fn route_window(
    state: &AppState,
    route: Route,
    provider: Option<Provider>,
) -> anyhow::Result<PackWindow> {
    let pipeline = state.pipeline.load_full();
    if let Some(w) = pipeline
        .config
        .packer
        .params
        .get("window_tokens")
        .and_then(Value::as_u64)
    {
        return Ok(PackWindow {
            tokens: w as usize,
            reserve_output: 0,
        });
    }
    let (provider, window) = match state.model_for(route, provider).await {
        Ok(m) => (m.config.provider, m.window),
        Err(e) => {
            tracing::warn!(
                ?route,
                "no model for the route ({}); packing against the fallback window",
                e.message
            );
            let provider = match provider {
                Some(p) => p,
                None => state.default_provider().await?,
            };
            (provider, margin_ai::MODEL_WINDOW_FALLBACK)
        }
    };
    Ok(PackWindow {
        tokens: window.input as usize,
        reserve_output: output_reserve(provider, route, window),
    })
}

// ---------------------------------------------------------------------------
// The cache
// ---------------------------------------------------------------------------

/// Retrieval results by `hash(query strings, scope, embedder, index generation)`, an
/// in-memory LRU of `RETRIEVAL_CACHE_ENTRIES`; a document's ingest bumps its generation,
/// which invalidates every entry that read it.
pub struct RetrievalCache {
    capacity: usize,
    entries: Mutex<HashMap<String, CacheEntry>>,
}

struct CacheEntry {
    packed: Arc<Vec<Packed>>,
    documents: HashSet<String>,
    used: u64,
}

/// Entries the retrieval cache keeps.
pub const RETRIEVAL_CACHE_ENTRIES: usize = 256;

static CACHE_CLOCK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn tick() -> u64 {
    CACHE_CLOCK.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl RetrievalCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn get(&self, key: &str) -> Option<Arc<Vec<Packed>>> {
        let mut entries = self.entries.lock().unwrap();
        let e = entries.get_mut(key)?;
        e.used = tick();
        Some(e.packed.clone())
    }

    pub fn insert(&self, key: String, packed: Arc<Vec<Packed>>) {
        if self.capacity == 0 {
            return;
        }
        let documents = packed
            .iter()
            .flat_map(|p| p.hits.iter().map(|h| h.passage.document_id.clone()))
            .collect();
        let mut entries = self.entries.lock().unwrap();
        if !entries.contains_key(&key) && entries.len() >= self.capacity {
            let oldest = entries
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone());
            if let Some(oldest) = oldest {
                entries.remove(&oldest);
            }
        }
        entries.insert(
            key,
            CacheEntry {
                packed,
                documents,
                used: tick(),
            },
        );
    }

    /// Drops every entry that read `document_id`.
    pub fn invalidate(&self, document_id: &str) {
        self.entries
            .lock()
            .unwrap()
            .retain(|_, e| !e.documents.contains(document_id));
    }

    /// Entries kept now.
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Changes whenever a document is indexed, re-indexed or deleted: the count and the sum
/// of the documents' generations.
async fn index_generation(store: &Store) -> anyhow::Result<String> {
    #[derive(Deserialize)]
    struct Stamp {
        #[serde(default)]
        n: u64,
        #[serde(default)]
        g: f64,
    }
    let rows: Vec<Stamp> = store
        .query_rows(
            "RETURN { n: count(SELECT id FROM document), \
             g: math::sum(SELECT VALUE doc.generation FROM document) };",
            vec![],
        )
        .await?;
    Ok(rows
        .first()
        .map_or_else(String::new, |s| format!("{}:{}", s.n, s.g)))
}

// ---------------------------------------------------------------------------
// retrieve
// ---------------------------------------------------------------------------

/// Registers this file's stages.
pub fn register(reg: &mut Registry) {
    for (name, legs) in [
        ("hybrid-rrf", Legs::Hybrid),
        ("hybrid-linear", Legs::HybridLinear),
        ("dense", Legs::Dense),
        ("bm25", Legs::Lexical),
    ] {
        reg.register(
            name,
            StageCtor::Retriever(Arc::new(move |cfg, deps| {
                Ok(Arc::new(IndexRetriever::new(
                    deps.store.clone(),
                    legs,
                    &cfg.params,
                )))
            })),
        );
    }
    reg.register("rrf", StageCtor::Fusion(Arc::new(|_, _| Ok(Arc::new(Rrf)))));
    reg.register(
        "linear",
        StageCtor::Fusion(Arc::new(|_, _| Ok(Arc::new(Linear)))),
    );
    reg.register(
        "mmr-v1",
        StageCtor::Packer(Arc::new(|cfg, _| {
            Ok(Arc::new(MmrV1::from_params(&cfg.params)))
        })),
    );
}

/// A source's rows in the old `chunk`/`media` tables, while they exist (the migration
/// to `passage` removes them; reading a removed table is an error).
async fn legacy_rows<T: serde::de::DeserializeOwned>(
    store: &Store,
    table: &'static str,
    source_id: &str,
) -> anyhow::Result<Vec<T>> {
    if !store.table_exists(table).await? {
        return Ok(Vec::new());
    }
    store.list_where(table, "source_id", source_id).await
}

/// Records a trace row through `AppState.tracer`.
pub(crate) fn record_trace(state: &AppState, row: TraceRow) {
    state.tracer.record(row);
}

fn hash_hex(text: &str) -> String {
    crate::state::sha256_hex(text)[..16].to_string()
}

/// Retrieves, fuses, reranks and packs for `req` with the current pipeline, through the
/// cache, and records a `retrieval` trace row.
pub async fn retrieve(state: &AppState, req: &Request) -> anyhow::Result<Vec<Packed>> {
    let started = Instant::now();
    let pipeline = state.pipeline.load_full();
    let resolved = resolve(&state.store, &req.scope).await?;
    let mut row = TraceRow::new(TraceKind::Retrieval, new_id(), Utc::now());
    row.trace_id.clone_from(&req.trace_id);
    row.pipeline_hash = Some(pipeline.hash.clone());
    row.scope = Some(req.scope.clone());
    row.query_hash = Some(hash_hex(&req.plan.queries.join("\n")));
    if let Some(caller) = &req.caller {
        caller.stamp(&mut row);
        row.query = Some(req.plan.queries.join("\n"));
    }
    if req.plan.queries.is_empty() || resolved.is_empty() || req.k == 0 {
        row.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
        record_trace(state, row);
        return Ok(Vec::new());
    }

    let legacy = legacy_sources(state, &resolved).await?;
    let embedder = crate::embedding::embedder(state).await.ok().flatten();
    let key = json!({
        "q": req.plan.queries,
        "scope": req.scope,
        "access": req.access,
        "modalities": req.modalities,
        "k": req.k,
        "budget": req.budget,
        "window": req.window_tokens,
        "reserve": req.reserve_output,
        "pipeline": pipeline.hash,
        "embedder": embedder.as_ref().map(|e| e.version().to_string()),
        "generation": index_generation(&state.store).await?,
    })
    .to_string();
    let key = crate::state::sha256_hex(&key);
    if legacy.is_empty()
        && let Some(hit) = state.retrieval_cache.get(&key)
    {
        row.cache_hit = Some(true);
        fill_packed_trace(&mut row, &hit);
        row.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
        record_trace(state, row);
        return Ok(hit.as_ref().clone());
    }

    // The rewriter (R2): after the cache check, whose key stays on the original plan
    // (`pipeline.hash` names the rewriter), and before embedding. A failure keeps the
    // plan as it was.
    // Model-backed stages trace their calls under this row and keep `passages_min`.
    let stage_ctx = RetrievalCtx {
        row_id: row.id.clone(),
        passages_min: req.budget.passages_min as usize,
    };
    // A lesson's retrieval runs under the lesson's id: the stages' calls name it as
    // their job.
    let job_id = (req.purpose == RetrievalPurpose::Lesson).then_some(req.trace_id.as_str());
    let t = Instant::now();
    let names = if pipeline.rewriter.name() == "none" {
        Vec::new()
    } else {
        scope_names(&state.store, &resolved, req.access.as_deref()).await
    };
    let rewrite = RewriteRequest {
        purpose: req.purpose,
        provider: req.provider,
        trace_id: &req.trace_id,
        job_id,
        scope_names: &names,
    };
    let rewritten = with_retrieval(
        stage_ctx.clone(),
        pipeline.rewriter.rewrite(&rewrite, req.plan.clone()),
    )
    .await;
    let plan = match rewritten {
        Ok(plan) if !plan.queries.is_empty() => plan,
        Ok(_) => req.plan.clone(),
        Err(e) => {
            tracing::warn!(
                "the rewriter \"{}\" failed; retrieving with the plan as it was: {e:#}",
                pipeline.rewriter.name()
            );
            req.plan.clone()
        }
    };
    let rewrite_ms = t.elapsed().as_secs_f64() * 1000.0;
    row.queries = Some(plan.queries.len() as u32);

    let vectors: Vec<Vec<f32>> = match &embedder {
        Some(e) => {
            let e = e.clone();
            let texts = plan.queries.clone();
            tokio::task::spawn_blocking(move || e.embed(&texts, Purpose::Query))
                .await?
                .context("couldn't embed the query")?
        }
        None => vec![Vec::new(); plan.queries.len()],
    };
    let fusion_params = &pipeline.config.fusion.params;
    let fusion_cfg = FusionCfg {
        k_fused: param_usize(fusion_params, "k_fused", RETRIEVAL_K_FUSED).max(req.k),
        rrf_k: param_usize(fusion_params, "rrf_k", RRF_K as usize) as u32,
        params: fusion_params.clone(),
    };
    let budget = pack_budget_reserving(&req.budget, req.window_tokens, req.reserve_output, req.k);
    let mut legs = Vec::new();
    let t = Instant::now();
    for (text, vector) in plan.queries.iter().zip(vectors) {
        let q = Query {
            text: text.clone(),
            embedder: embedder
                .as_ref()
                .filter(|_| !vector.is_empty())
                .map_or_else(String::new, |e| e.version().to_string()),
            vector,
            scope: req.scope.clone(),
            access: req.access.clone(),
            modalities: req.modalities.clone(),
            k: fusion_cfg.k_fused,
            budget_tokens: budget.tokens,
            images_max: budget.images_max,
            diversity: budget.diversity,
        };
        let mut leg = pipeline.retriever.search(&q).await?;
        if !legacy.is_empty() {
            leg.extend(legacy_hits(state, &legacy, &q, req.access.as_deref()).await?);
            leg.sort_by(|a, b| b.score.total_cmp(&a.score));
            leg.truncate(fusion_cfg.k_fused);
        }
        legs.push(leg);
    }
    let search_ms = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    let fused = pipeline.fusion.fuse(legs, &fusion_cfg);
    let fuse_ms = t.elapsed().as_secs_f64() * 1000.0;
    row.hit_ids = fused.iter().map(|h| h.passage.id.clone()).collect();
    row.scores = fused.iter().map(|h| h.score).collect();
    // The reranker (R2): a failure keeps the fused order, traced as "rerank_failed".
    let t = Instant::now();
    let query = rerank_query(&plan);
    let rerank = RerankRequest {
        query: &query,
        purpose: req.purpose,
        provider: req.provider,
        trace_id: &req.trace_id,
        job_id,
    };
    row.reranker = Some(pipeline.reranker.name().to_string());
    let result = with_retrieval(stage_ctx, pipeline.reranker.rerank(&rerank, fused.clone())).await;
    let reranked = match result {
        Ok(hits) => {
            row.reranked = reranked_count(&fused, &hits);
            hits
        }
        Err(e) => {
            tracing::warn!(
                "the reranker \"{}\" failed; keeping the fused order: {e:#}",
                pipeline.reranker.name()
            );
            row.error_code = Some("rerank_failed".into());
            fused
        }
    };
    let rerank_ms = t.elapsed().as_secs_f64() * 1000.0;
    let mut packed = pipeline.packer.pack(reranked, &budget);
    describe_packed(state, &mut packed).await?;

    row.legs = Some(RetrievalLegs {
        dense_ms: search_ms,
        lex_ms: 0.0,
        fuse_ms,
        rerank_ms,
        rewrite_ms,
    });
    row.cache_hit = Some(false);
    fill_packed_trace(&mut row, &packed);
    if legacy.is_empty() {
        state.retrieval_cache.insert(key, Arc::new(packed.clone()));
    }
    row.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
    record_trace(state, row);
    Ok(packed)
}

/// The candidates a reranker saw, for the trace: those it scored plus those it dropped
/// (a listwise grade 0); `None` when it changed nothing (`"none"`, or a purpose it
/// passes through).
pub fn reranked_count(fused: &[Hit], reranked: &[Hit]) -> Option<u32> {
    let scored = reranked.iter().filter(|h| h.rerank.is_some()).count();
    let kept: std::collections::HashSet<&str> =
        reranked.iter().map(|h| h.passage.id.as_str()).collect();
    let dropped = fused
        .iter()
        .filter(|h| !kept.contains(h.passage.id.as_str()))
        .count();
    (scored + dropped > 0).then_some((scored + dropped) as u32)
}

/// Up to `REWRITE_SCOPE_NAMES` names of the documents in `resolved` (every document for
/// the whole library) that a reader with `access` may see (L1; `None` = no filter), for
/// the query rewriter's prompt: names (through `text::for_model`), never passage text,
/// and never a document the caller can't read.
pub async fn scope_names(
    store: &Store,
    resolved: &Resolved,
    access: Option<&[String]>,
) -> Vec<String> {
    let documents: Vec<Document> = match store.list(db::DOCUMENT).await {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!("couldn't list documents for the rewriter: {e:#}");
            return Vec::new();
        }
    };
    let mut names: Vec<String> = Vec::new();
    for d in documents {
        let visible = access.is_none_or(|paths| d.access.iter().any(|a| may_see(paths, a)));
        let name = for_model(&d.name);
        if (resolved.library || resolved.keys.contains(&d.id))
            && visible
            && !name.trim().is_empty()
            && !names.contains(&name)
        {
            names.push(name);
        }
    }
    names.sort();
    names.truncate(crate::rewrite::REWRITE_SCOPE_NAMES);
    names
}

fn fill_packed_trace(row: &mut TraceRow, packed: &[Packed]) {
    row.packed_ids = packed
        .iter()
        .flat_map(|p| p.hits.iter().map(|h| h.passage.id.clone()))
        .collect();
    row.packed_tokens = Some(packed.iter().map(|p| p.tokens as u64).sum());
    row.images = Some(
        packed
            .iter()
            .filter(|p| p.citation.image_url.is_some())
            .count() as u32,
    );
}

// ---------------------------------------------------------------------------
// Old rows, until the migration is recorded
// ---------------------------------------------------------------------------

/// The Ready sources in scope that have no passages yet, while the migration hasn't
/// been recorded.
async fn legacy_sources(state: &AppState, resolved: &Resolved) -> anyhow::Result<Vec<Source>> {
    if resolved.source_ids.is_empty() {
        return Ok(Vec::new());
    }
    let settings = state.settings_row().await?;
    if settings
        .migrations
        .iter()
        .any(|m| m == crate::ingest::PASSAGES_MIGRATION)
    {
        return Ok(Vec::new());
    }
    #[derive(Deserialize)]
    struct Count {
        n: u64,
    }
    let mut out = Vec::new();
    let docs: Vec<Document> = state.store.list(db::DOCUMENT).await?;
    for id in &resolved.source_ids {
        let Some(source) = state.store.get::<Source>(db::SOURCE, id).await? else {
            continue;
        };
        if !matches!(source.status, SourceStatus::Ready { .. }) {
            continue;
        }
        let mut keys = vec![id.clone()];
        keys.extend(
            docs.iter()
                .filter(|d| matches!(&d.origin, Origin::Source { source_id } if source_id == id))
                .map(|d| d.id.clone()),
        );
        let n: Vec<Count> = state
            .store
            .query_rows(
                "RETURN { n: count(SELECT id FROM passage WHERE document_id IN $keys) };",
                vec![("keys", json!(keys))],
            )
            .await?;
        if n.first().map_or(0, |c| c.n) == 0 {
            out.push(source);
        }
    }
    Ok(out)
}

fn legacy_row(
    source_id: &str,
    modality: Modality,
    ord: u32,
    (page, start_ms, end_ms): (Option<u32>, Option<u64>, Option<u64>),
    text: &str,
    file: Option<String>,
    vector: &[f32],
) -> PassageRow {
    PassageRow {
        id: format!("{source_id}:{}:{ord}", modality.as_str()),
        project_id: String::new(),
        document_id: source_id.to_string(),
        origin: "source".into(),
        scope_ids: Vec::new(),
        access: vec![ACCESS_COMPANY.to_string()],
        unit_id: match page {
            Some(p) => format!("{source_id}:page:{p}"),
            None => format!("{source_id}:{}:{ord}", modality.as_str()),
        },
        modality: modality.as_str().into(),
        ord: ord as i64,
        page: page.map(i64::from),
        start_line: None,
        end_line: None,
        start_ms: start_ms.map(|v| v as i64),
        end_ms: end_ms.map(|v| v as i64),
        section_path: String::new(),
        text: text.to_string(),
        file,
        chunk_hash: String::new(),
        embedder: String::new(),
        vector: vector.to_vec(),
    }
}

/// The old rows of `sources` ranked by cosine against the query (the ranking of
/// before; it needs the query's vector).
async fn legacy_hits(
    state: &AppState,
    sources: &[Source],
    q: &Query,
    access: Option<&[String]>,
) -> anyhow::Result<Vec<Hit>> {
    if q.vector.is_empty() || access.is_some_and(|paths| !may_see(paths, ACCESS_COMPANY)) {
        return Ok(Vec::new());
    }
    let mut rows = Vec::new();
    for s in sources {
        let chunks: Vec<ChunkRow> = legacy_rows(&state.store, db::CHUNK, &s.id).await?;
        rows.extend(chunks.iter().map(|c| {
            legacy_row(
                &s.id,
                Modality::Text,
                c.ord,
                (c.page, c.start_ms, c.end_ms),
                &c.text,
                None,
                &c.vector,
            )
        }));
        let media: Vec<MediaRow> = legacy_rows(&state.store, db::MEDIA, &s.id).await?;
        rows.extend(media.iter().filter_map(|m| {
            let modality = match m.kind {
                MediaKind::Image | MediaKind::Page => Modality::Picture,
                MediaKind::Frame => Modality::Frame,
                // A sound window without a transcript has nothing a writer can read.
                MediaKind::Audio if m.text.trim().is_empty() => return None,
                MediaKind::Audio => Modality::Sound,
            };
            Some(legacy_row(
                &s.id,
                modality,
                m.ord,
                (m.page, m.start_ms, m.end_ms),
                &m.text,
                m.file.clone(),
                &m.vector,
            ))
        }));
    }
    let wanted = |m: &str| q.modalities.is_empty() || q.modalities.iter().any(|x| x.as_str() == m);
    let tools = state.source_tools.clone();
    let query_vec = q.vector.clone();
    let query_text = q.text.clone();
    let k = q.k;
    let rows: Vec<PassageRow> = rows.into_iter().filter(|r| wanted(&r.modality)).collect();
    let hits = tokio::task::spawn_blocking(move || {
        let mut hits: Vec<Hit> = rows
            .into_iter()
            .map(|passage| Hit {
                score: tools.score(&query_vec, &query_text, &passage.vector, &passage.text),
                passage,
                dense_rank: None,
                lex_rank: None,
                rerank: None,
                highlight: None,
            })
            .collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(k);
        for (i, h) in hits.iter_mut().enumerate() {
            h.dense_rank = Some(i as u32);
        }
        hits
    })
    .await?;
    Ok(hits)
}

// ---------------------------------------------------------------------------
// Documents, citations, and what a writer is shown
// ---------------------------------------------------------------------------

/// What a citation names about a passage's document.
#[derive(Debug, Clone)]
pub struct DocInfo {
    pub document_id: Id,
    /// The uploaded source behind it (its media are served under this id).
    pub source_id: Option<Id>,
    pub name: String,
    pub kind: SourceKind,
    pub file: Option<WorkspaceFile>,
}

/// The document `id` (or, for rows written before documents existed, the source).
pub async fn doc_info(store: &Store, id: &str) -> anyhow::Result<Option<DocInfo>> {
    if let Some(d) = store.get::<Document>(db::DOCUMENT, id).await? {
        let (source_id, file) = match &d.origin {
            Origin::Source { source_id } => (Some(source_id.clone()), None),
            Origin::FolderFile { workspace_id, path } => (
                None,
                Some(WorkspaceFile {
                    workspace_id: workspace_id.clone(),
                    path: path.clone(),
                }),
            ),
            _ => (None, None),
        };
        let (name, kind) = match &source_id {
            Some(sid) => match store.get::<Source>(db::SOURCE, sid).await? {
                Some(s) => (s.name, s.kind),
                None => (d.name.clone(), d.kind),
            },
            None => (d.name.clone(), d.kind),
        };
        // A folder's file is a source with the document's id: its media are served under
        // it, and it reads as "folder/path".
        if file.is_some()
            && let Some(s) = store.get::<Source>(db::SOURCE, &d.id).await?
        {
            return Ok(Some(DocInfo {
                document_id: d.id,
                source_id: Some(s.id),
                name: folder_file_name(s.folder.as_deref(), &s.name),
                kind: s.kind,
                file,
            }));
        }
        return Ok(Some(DocInfo {
            document_id: d.id,
            source_id,
            name,
            kind,
            file,
        }));
    }
    Ok(store.get::<Source>(db::SOURCE, id).await?.map(|s| DocInfo {
        document_id: s.id.clone(),
        source_id: Some(s.id.clone()),
        file: s.workspace_id.clone().map(|workspace_id| WorkspaceFile {
            workspace_id,
            path: s.name.clone(),
        }),
        name: match &s.workspace_id {
            Some(_) => folder_file_name(s.folder.as_deref(), &s.name),
            None => s.name,
        },
        kind: s.kind,
    }))
}

/// A folder file's name as citations and tools show it: "`folder`/`path`".
pub fn folder_file_name(folder: Option<&str>, path: &str) -> String {
    match folder.filter(|f| !f.is_empty()) {
        Some(f) => format!("{f}/{path}"),
        None => path.to_string(),
    }
}

/// Fills each packed passage's citation with its document (name, kind, the source its
/// media are served under) and, for text on a rendered page or in a recording, the
/// picture to show with it.
pub async fn describe_packed(state: &AppState, packed: &mut [Packed]) -> anyhow::Result<()> {
    let mut infos: HashMap<String, Option<DocInfo>> = HashMap::new();
    for p in packed.iter_mut() {
        normalise_packed(p);
        let doc = p.citation.document_id.clone().unwrap_or_default();
        if !infos.contains_key(&doc) {
            infos.insert(doc.clone(), doc_info(&state.store, &doc).await?);
        }
        let Some(info) = infos.get(&doc).cloned().flatten() else {
            continue;
        };
        let c = &mut p.citation;
        c.source_id = info
            .source_id
            .clone()
            .unwrap_or_else(|| info.document_id.clone());
        c.source_name.clone_from(&info.name);
        c.kind = info.kind;
        c.file.clone_from(&info.file);
        c.source_name = for_model(&c.source_name);
        if let Some(file) = &mut c.file {
            file.path = for_model(&file.path);
        }
        if c.image_url.is_none()
            && let Some(source_id) = &info.source_id
        {
            c.image_url = text_picture(state, source_id, &p.hits[0].passage).await?;
        }
    }
    Ok(())
}

/// A packed passage as a model may read it (`docs/phase5-brief.md` D2.1): its text,
/// its hits' text and section paths, and its citation's name, excerpt and section
/// through `text::for_model`. Every packed passage passes here before it reaches a
/// prompt (lessons, the planner), a tool output or the context API, and before it is
/// cached.
pub fn normalise_packed(p: &mut Packed) {
    p.text = for_model(&p.text);
    for h in &mut p.hits {
        h.passage.text = for_model(&h.passage.text);
        h.passage.section_path = for_model(&h.passage.section_path);
    }
    let c = &mut p.citation;
    c.source_name = for_model(&c.source_name);
    c.excerpt = for_model(&c.excerpt);
    c.section_path = c.section_path.as_deref().map(for_model);
    if let Some(file) = &mut c.file {
        file.path = for_model(&file.path);
    }
}

/// The picture shown with a text passage: its rendered PDF page, or the video frame
/// nearest the words.
async fn text_picture(
    state: &AppState,
    source_id: &str,
    passage: &PassageRow,
) -> anyhow::Result<Option<String>> {
    if passage.modality != Modality::Text.as_str() && passage.modality != Modality::Sound.as_str() {
        return Ok(None);
    }
    let media_dir = state.source_media_dir(source_id);
    if let Some(page) = passage.page {
        let file = margin_embed::page_file(page as u32);
        return Ok(media_dir.join(&file).is_file().then_some(file));
    }
    let Some(start) = passage.start_ms else {
        return Ok(None);
    };
    let middle = (start + passage.end_ms.unwrap_or(start)) / 2;
    #[derive(Deserialize)]
    struct Frame {
        #[serde(default)]
        file: Option<String>,
        #[serde(default)]
        start_ms: Option<i64>,
    }
    let mut frames: Vec<(i64, String)> = state
        .store
        .query_rows::<Frame>(
            "SELECT file, start_ms FROM passage WHERE document_id = $d AND modality = 'frame'",
            vec![("d", json!(passage.document_id))],
        )
        .await?
        .into_iter()
        .filter_map(|f| Some((f.start_ms?, f.file?)))
        .collect();
    if frames.is_empty() {
        let media: Vec<MediaRow> = legacy_rows(&state.store, db::MEDIA, source_id).await?;
        frames = media
            .into_iter()
            .filter(|m| m.kind == MediaKind::Frame)
            .filter_map(|m| Some((m.start_ms? as i64, m.file?)))
            .collect();
    }
    Ok(frames
        .into_iter()
        .min_by_key(|(at, _)| at.abs_diff(middle))
        .map(|(_, file)| file))
}

/// Renumbers a finished body's `[^n]` markers (n = 1-based index into `packed`) in order
/// of first appearance and builds its citations, `image_url` still a bare media file
/// name (`App::public_lesson` signs it).
pub fn cite(body: &str, packed: &[Packed]) -> (String, Vec<Citation>) {
    let renumbered = margin_ai::renumber_citations(body, packed.len());
    let citations = renumbered
        .cited
        .iter()
        .enumerate()
        .map(|(i, &index)| Citation {
            n: i as u32 + 1,
            ..packed[index].citation.clone()
        })
        .collect();
    (renumbered.body, citations)
}

/// Where the picture `file` of a source is on disk.
pub fn picture_path(state: &AppState, source_id: &str, name: &str, file: &str) -> PathBuf {
    let dir = state.source_dir(source_id);
    if file == SOURCE_ORIGINAL {
        let named = name
            .rsplit(['/', '\\'])
            .next()
            .and_then(|n| n.rsplit_once('.'))
            .map(|(_, e)| format!("input.{}", e.to_ascii_lowercase()))
            .map(|n| dir.join(n))
            .filter(|p| p.exists());
        return named.unwrap_or_else(|| dir.join(crate::sources::FILE_NAME));
    }
    state.source_media_dir(source_id).join(file)
}

/// The packed passages as the lesson writer and the planner are shown them, pictures
/// read at `WRITER_IMAGE_SIDE`.
pub async fn for_writer(state: &AppState, packed: &[Packed]) -> Vec<margin_ai::Passage> {
    let mut out = Vec::with_capacity(packed.len());
    for p in packed {
        let c = &p.citation;
        let mut image = None;
        if let Some(file) = &c.image_url
            && p.hits
                .iter()
                .any(|h| h.passage.file.as_deref() == Some(file.as_str()))
        {
            let path = picture_path(state, &c.source_id, &c.source_name, file);
            let tools = state.source_tools.clone();
            match tokio::task::spawn_blocking(move || tools.image_for_model(&path)).await {
                Ok(Ok((media_type, data))) => image = Some(margin_ai::Image { media_type, data }),
                Ok(Err(e)) => tracing::warn!("couldn't read a picture for the writer: {e}"),
                Err(e) => tracing::warn!("couldn't read a picture for the writer: {e}"),
            }
        }
        out.push(margin_ai::Passage {
            source: c.source_name.clone(),
            page: c.page,
            text: p.text.clone(),
            start_ms: c.start_ms,
            end_ms: c.end_ms,
            image,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Waiting for documents still being read
// ---------------------------------------------------------------------------

/// Waits (at most `wait`) until no source in `scope` is still being read, and returns
/// the names of those that still are. Sources waiting for an embedding model that won't
/// come aren't waited for.
pub async fn wait_until_read(
    state: &AppState,
    scope: &Scope,
    wait: Duration,
) -> anyhow::Result<Vec<String>> {
    let deadline = Instant::now() + wait;
    let mut events = state.events.subscribe();
    loop {
        let resolved = resolve(&state.store, scope).await?;
        let mut pending = Vec::new();
        let mut processing = false;
        for id in &resolved.source_ids {
            if let Some(s) = state.store.get::<Source>(db::SOURCE, id).await?
                && crate::sources::is_pending(&s.status)
            {
                processing |= matches!(s.status, SourceStatus::Processing { .. });
                pending.push(s.name);
            }
        }
        let documents: Vec<Document> = state.store.list(db::DOCUMENT).await?;
        for d in documents {
            if resolved.keys.contains(&d.id)
                && matches!(
                    d.status,
                    margin_shared::context::DocumentStatus::Indexing { .. }
                )
                && !pending.contains(&d.name)
            {
                processing = true;
                pending.push(d.name);
            }
        }
        if pending.is_empty() {
            return Ok(pending);
        }
        if !processing && !crate::embedding::will_be_available(state).await {
            return Ok(pending);
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(pending);
        }
        let changed = async {
            loop {
                match events.recv().await.map(|e| e.event) {
                    Ok(
                        Event::SourceChanged { .. }
                        | Event::SourceDeleted { .. }
                        | Event::DocumentChanged { .. },
                    )
                    | Err(RecvError::Lagged(_)) => return,
                    Ok(_) => {}
                    Err(RecvError::Closed) => std::future::pending::<()>().await,
                }
            }
        };
        let _ = tokio::time::timeout((deadline - now).min(Duration::from_secs(5)), changed).await;
    }
}

// ---------------------------------------------------------------------------
// Reading a document's passages (read_passage, document_outline, view_picture)
// ---------------------------------------------------------------------------

/// A passage row as the context API's `Hit`.
pub fn context_hit(
    row: &PassageRow,
    score: f32,
    highlight: Option<String>,
) -> margin_shared::context::Hit {
    let modality = Modality::ALL
        .into_iter()
        .find(|m| m.as_str() == row.modality)
        .unwrap_or(Modality::Text);
    margin_shared::context::Hit {
        document_id: row.document_id.clone(),
        unit_id: row.unit_id.clone(),
        passage_id: row.id.clone(),
        modality,
        section_path: for_model(&row.section_path),
        page: row.page.map(|v| v as u32),
        start_line: row.start_line.map(|v| v as u32),
        end_line: row.end_line.map(|v| v as u32),
        start_ms: row.start_ms.map(|v| v as u64),
        end_ms: row.end_ms.map(|v| v as u64),
        // What the context API and the tools return is what a model reads.
        text: for_model(&row.text),
        score,
        highlight: highlight.as_deref().map(for_model),
    }
}

/// A packed passage as the context API's `Hit` (its first hit's locator, the packed
/// text).
pub fn packed_hit(p: &Packed) -> margin_shared::context::Hit {
    let first = p
        .hits
        .iter()
        .min_by_key(|h| (h.passage.modality != Modality::Text.as_str(), h.passage.ord))
        .unwrap_or(&p.hits[0]);
    let mut hit = context_hit(
        &first.passage,
        p.hits.iter().map(|h| h.score).fold(f32::MIN, f32::max),
        p.hits.iter().find_map(|h| h.highlight.clone()),
    );
    hit.text = for_model(&p.text);
    hit
}

/// Whether a document is known (has a document row, a source, or passages), and
/// whether it is still being indexed with nothing to read yet.
pub enum DocState {
    Unknown,
    NotReady,
    Ready,
}

pub async fn doc_state(store: &Store, id: &str) -> anyhow::Result<DocState> {
    #[derive(Deserialize)]
    struct Count {
        n: u64,
    }
    let n: Vec<Count> = store
        .query_rows(
            "RETURN { n: count(SELECT id FROM passage WHERE document_id = $d) };",
            vec![("d", json!(id))],
        )
        .await?;
    let has_passages = n.first().is_some_and(|c| c.n > 0);
    if let Some(d) = store.get::<Document>(db::DOCUMENT, id).await? {
        return Ok(match d.status {
            margin_shared::context::DocumentStatus::Indexing { .. } if !has_passages => {
                DocState::NotReady
            }
            _ => DocState::Ready,
        });
    }
    if has_passages {
        return Ok(DocState::Ready);
    }
    Ok(match store.get::<Source>(db::SOURCE, id).await? {
        Some(s) if crate::sources::is_pending(&s.status) => DocState::NotReady,
        Some(_) => DocState::Ready,
        None => DocState::Unknown,
    })
}

/// Which text passages of a document to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selector {
    /// Those on a PDF page.
    Page(u32),
    /// `from ≤ ord ≤ to`.
    Ords { from: u32, to: u32 },
}

/// The text passages `sel` names, in order, at most `max`; a source not yet moved to
/// passages is read from its old chunks.
pub async fn read_passages(
    store: &Store,
    document_id: &str,
    sel: Selector,
    max: usize,
) -> anyhow::Result<Vec<PassageRow>> {
    let rows = match sel {
        Selector::Page(page) => passages_on_page(store, document_id, page, max).await?,
        Selector::Ords { from, to } => passages_by_ord(store, document_id, from, to, max).await?,
    };
    if !rows.is_empty() {
        return Ok(rows);
    }
    let mut chunks: Vec<ChunkRow> = legacy_rows(store, db::CHUNK, document_id).await?;
    chunks.sort_by_key(|c| c.ord);
    let mut rows: Vec<PassageRow> = chunks
        .iter()
        .filter(|c| match sel {
            Selector::Page(p) => c.page == Some(p),
            Selector::Ords { from, to } => c.ord >= from && c.ord <= to.max(from),
        })
        .map(|c| {
            legacy_row(
                document_id,
                Modality::Text,
                c.ord,
                (c.page, c.start_ms, c.end_ms),
                &c.text,
                None,
                &c.vector,
            )
        })
        .collect();
    rows.truncate(max);
    Ok(rows)
}

/// `retrieve` for a context query (the agent's `search` tool and `POST
/// routes::CONTEXT_SEARCH`, one path): packed to `q.budget`, or to the pipeline's budget
/// when it has none, against `route`'s window.
pub async fn context_search(
    state: &AppState,
    q: &margin_shared::context::ContextQuery,
    route: Route,
    provider: Option<Provider>,
    purpose: RetrievalPurpose,
    trace_id: &str,
    caller: Option<Caller>,
) -> anyhow::Result<Vec<Packed>> {
    let budget = q.budget.unwrap_or(state.pipeline.load_full().config.budget);
    let window = route_window(state, route, provider).await?;
    retrieve(
        state,
        &Request {
            plan: QueryPlan::single(&q.text),
            scope: q.scope.clone(),
            access: q.access.clone(),
            modalities: q.modalities.clone(),
            k: q.k as usize,
            budget,
            window_tokens: window.tokens,
            reserve_output: window.reserve_output,
            trace_id: trace_id.to_string(),
            caller,
            purpose,
            provider,
        },
    )
    .await
}

/// Text passages of `document_id` with `from ≤ ord ≤ to`, in order, at most `max`
/// (the index on `document_id` finds them).
pub async fn passages_by_ord(
    store: &Store,
    document_id: &str,
    from: u32,
    to: u32,
    max: usize,
) -> anyhow::Result<Vec<PassageRow>> {
    let to = to
        .max(from)
        .min(from.saturating_add(max.saturating_sub(1) as u32));
    store
        .query_rows(
            "SELECT *, record::id(id) AS id FROM passage WHERE document_id = $d \
             AND modality = 'text' AND ord >= $from AND ord <= $to ORDER BY ord",
            vec![
                ("d", json!(document_id)),
                ("from", json!(from)),
                ("to", json!(to)),
            ],
        )
        .await
}

/// Text passages of `document_id` on `page`, in order, at most `max`.
pub async fn passages_on_page(
    store: &Store,
    document_id: &str,
    page: u32,
    max: usize,
) -> anyhow::Result<Vec<PassageRow>> {
    let mut rows: Vec<PassageRow> = store
        .query_rows(
            "SELECT *, record::id(id) AS id FROM passage WHERE document_id = $d \
             AND modality = 'text' AND page = $page ORDER BY ord",
            vec![("d", json!(document_id)), ("page", json!(page))],
        )
        .await?;
    rows.truncate(max);
    Ok(rows)
}

/// A document's sections, from its text passages' `section_path` (consecutive passages
/// of one path are one section).
pub async fn document_outline(
    store: &Store,
    document_id: &str,
) -> anyhow::Result<Vec<margin_shared::context::OutlineSection>> {
    #[derive(Deserialize)]
    struct Loc {
        ord: i64,
        #[serde(default)]
        section_path: String,
        #[serde(default)]
        page: Option<i64>,
        #[serde(default)]
        start_line: Option<i64>,
        #[serde(default)]
        end_line: Option<i64>,
    }
    let mut rows: Vec<Loc> = store
        .query_rows(
            "SELECT ord, section_path, page, start_line, end_line FROM passage \
             WHERE document_id = $d AND modality = 'text' ORDER BY ord",
            vec![("d", json!(document_id))],
        )
        .await?;
    if rows.is_empty() {
        // A source not yet moved to passages: its old chunks (pages, no sections).
        let mut chunks: Vec<ChunkRow> = legacy_rows(store, db::CHUNK, document_id).await?;
        chunks.sort_by_key(|c| c.ord);
        rows = chunks
            .into_iter()
            .map(|c| Loc {
                ord: c.ord as i64,
                section_path: String::new(),
                page: c.page.map(i64::from),
                start_line: None,
                end_line: None,
            })
            .collect();
    }
    let mut out: Vec<margin_shared::context::OutlineSection> = Vec::new();
    let min_opt = |a: Option<u32>, b: Option<u32>| match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let max_opt = |a: Option<u32>, b: Option<u32>| match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    for r in rows {
        let ord = r.ord.max(0) as u32;
        let page = r.page.map(|v| v as u32);
        let start = r.start_line.map(|v| v as u32);
        let end = r.end_line.map(|v| v as u32);
        match out.last_mut() {
            Some(s) if s.section_path == r.section_path => {
                s.to_ord = ord;
                s.first_page = min_opt(s.first_page, page);
                s.last_page = max_opt(s.last_page, page);
                s.start_line = min_opt(s.start_line, start);
                s.end_line = max_opt(s.end_line, end);
            }
            _ => out.push(margin_shared::context::OutlineSection {
                section_path: r.section_path,
                from_ord: ord,
                to_ord: ord,
                first_page: page,
                last_page: page,
                start_line: start,
                end_line: end,
            }),
        }
    }
    Ok(out)
}

/// A document's picture: the one of `page`, else its first.
pub async fn document_picture(
    store: &Store,
    document_id: &str,
    page: Option<u32>,
) -> anyhow::Result<Option<PassageRow>> {
    let (sql, mut binds) = match page {
        Some(p) => (
            "SELECT *, record::id(id) AS id FROM passage WHERE document_id = $d \
             AND modality = 'picture' AND page = $page ORDER BY ord LIMIT 1",
            vec![("page", json!(p))],
        ),
        None => (
            "SELECT *, record::id(id) AS id FROM passage WHERE document_id = $d \
             AND modality = 'picture' ORDER BY ord LIMIT 1",
            Vec::new(),
        ),
    };
    binds.push(("d", json!(document_id)));
    let rows: Vec<PassageRow> = store.query_rows(sql, binds).await?;
    if let Some(row) = rows.into_iter().next() {
        return Ok(Some(row));
    }
    // A source not yet moved to passages: its old picture rows.
    let media: Vec<MediaRow> = legacy_rows(store, db::MEDIA, document_id).await?;
    Ok(media
        .into_iter()
        .filter(|m| matches!(m.kind, MediaKind::Image | MediaKind::Page))
        .filter(|m| page.is_none() || m.page == page)
        .min_by_key(|m| m.ord)
        .map(|m| {
            legacy_row(
                document_id,
                Modality::Picture,
                m.ord,
                (m.page, m.start_ms, m.end_ms),
                &m.text,
                m.file.clone(),
                &m.vector,
            )
        }))
}

#[cfg(test)]
mod tests;
