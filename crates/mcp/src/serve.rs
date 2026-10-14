//! `margin-mcp serve`: a stdio MCP server (the official Rust SDK, `rmcp`) that gives
//! Claude Code five tools over Margin's context API. A thin adapter: every tool is one
//! route (`docs/plan-dogfood.md` §2), and the only state is a cache of document names
//! kept fresh from the change feed so hits can say which document they come from.

use std::collections::HashMap;
use std::sync::Arc;

use margin_shared::Id;
use margin_shared::context::{ContextQuery, Document, Hit, Origin, Scope};
use margin_shared::routes::{TOOL_READ_PASSAGES_MAX, TOOL_SEARCH_K_MAX};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;

use crate::client::Client;
use crate::ingest::{CLAUDE_CODE, ingest_text};

/// Hits `search_context` returns when `k` isn't given.
pub const DEFAULT_K: u32 = 8;
/// Most passages one `read_passages` call returns (fetched `TOOL_READ_PASSAGES_MAX` at a
/// time).
pub const READ_PASSAGES_LIMIT: u32 = 32;

const INSTRUCTIONS: &str = "Margin is the knowledge base for building Margin: the repository's docs and briefs, \
design decisions, the owner's chats with Margin's agent and earlier Claude Code session summaries. \
Search it (search_context) before reading docs for context; follow a hit with document_outline and \
read_passages to read around it; cite what you use with the hit's citation. Record decisions with \
ingest_document (kind \"decision\"). recent_changes lists what changed since a generation you saw.";

#[derive(Default)]
struct Names {
    generation: u64,
    by_id: HashMap<Id, String>,
}

/// The MCP server: one `Client` and the name cache.
#[derive(Clone)]
pub struct Margin {
    client: Client,
    names: Arc<Mutex<Names>>,
    #[allow(dead_code, reason = "read by the tool_handler macro")]
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// What to look for, in words (a question or the terms you'd expect in the passage).
    pub query: String,
    /// Most hits to return (default 8, at most 12).
    #[serde(default)]
    pub k: Option<u32>,
    /// Where to search: "library" (default, every document), or "topic:<id>",
    /// "chat:<id>", "workspace:<id>", "sources:<id>,<id>".
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OutlineArgs {
    /// The document's id, from a search hit or recent_changes.
    pub document_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PassagesArgs {
    /// The document's id.
    pub document_id: String,
    /// First passage position (`from_ord` of an outline section), inclusive.
    pub from: u32,
    /// Last passage position (`to_ord`), inclusive; at most 32 passages per call.
    pub to: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct IngestArgs {
    /// The document's name, e.g. "Keep the Spectron memory for two weeks". A document of
    /// the same name you wrote before is replaced.
    pub name: String,
    /// The document's text (Markdown).
    pub text: String,
    /// "decision", "handoff" or "note" (prefixes the name, e.g. "Decision: …"), or
    /// "text" (default) / "code".
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChangesArgs {
    /// The generation returned by an earlier recent_changes (0 or absent: everything).
    #[serde(default)]
    pub since: Option<u64>,
}

/// A hit as the tools return it: the passage, where it is, and how to cite it.
#[derive(Debug, Serialize)]
struct ToolHit {
    document: String,
    document_id: Id,
    #[serde(skip_serializing_if = "String::is_empty")]
    section_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    lines: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page: Option<u32>,
    citation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    score: Option<f32>,
    text: String,
}

/// `"library"`, `"topic:<id>"`, `"chat:<id>"`, `"workspace:<id>"`, `"sources:<a>,<b>"`.
pub fn parse_scope(scope: Option<&str>) -> Result<Scope, String> {
    let s = scope.map(str::trim).unwrap_or("");
    if s.is_empty() || s.eq_ignore_ascii_case("library") {
        return Ok(Scope::Library);
    }
    let (kind, id) = s
        .split_once(':')
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim()))
        .filter(|(_, v)| !v.is_empty())
        .ok_or_else(|| format!("scope {s:?}: use library, topic:<id>, chat:<id>, workspace:<id> or sources:<id>,<id>"))?;
    Ok(match kind.as_str() {
        "topic" => Scope::Topic { id: id.into() },
        "chat" => Scope::Chat { id: id.into() },
        "workspace" | "folder" => Scope::Workspace { id: id.into() },
        "sources" | "source" => Scope::Sources {
            ids: id
                .split(',')
                .map(str::trim)
                .filter(|i| !i.is_empty())
                .map(Into::into)
                .collect(),
        },
        _ => {
            return Err(format!(
                "unknown scope kind {kind:?}: use library, topic, chat, workspace or sources"
            ));
        }
    })
}

fn lines(hit: &Hit) -> Option<String> {
    match (hit.start_line, hit.end_line) {
        (Some(a), Some(b)) if a != b => Some(format!("{a}-{b}")),
        (Some(a), _) => Some(a.to_string()),
        _ => None,
    }
}

/// "docs/plan.md@abc123 § Safety > Backups, lines 40-52": what a reader needs to find it.
pub fn citation(document: &str, hit: &Hit) -> String {
    let mut out = document.to_string();
    if !hit.section_path.is_empty() {
        out.push_str(" § ");
        out.push_str(&hit.section_path);
    }
    if let Some(page) = hit.page {
        out.push_str(&format!(", p. {page}"));
    }
    if let Some(lines) = lines(hit) {
        out.push_str(&format!(", lines {lines}"));
    }
    if let (Some(a), Some(b)) = (hit.start_ms, hit.end_ms) {
        out.push_str(&format!(", {}–{}", clock(a), clock(b)));
    }
    out
}

fn clock(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

fn origin_label(origin: &Origin) -> String {
    match origin {
        Origin::Source { .. } => "source".into(),
        Origin::FolderFile { path, .. } => format!("folder file {path}"),
        Origin::Library { kind, .. } => format!("{kind:?}").to_lowercase(),
        Origin::Agent { name } => format!("agent {name}"),
    }
}

fn pretty(value: &impl Serialize) -> Result<String, String> {
    serde_json::to_string_pretty(value).map_err(|e| e.to_string())
}

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

impl Margin {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            names: Arc::new(Mutex::new(Names::default())),
            tool_router: Self::tool_router(),
        }
    }

    fn remember(names: &mut Names, docs: &[Document]) {
        for doc in docs {
            names.by_id.insert(doc.id.clone(), doc.name.clone());
        }
    }

    /// Names of `ids`, refreshing the cache from the change feed when one is unknown.
    async fn names_of(&self, ids: impl Iterator<Item = &Id>) -> HashMap<Id, String> {
        let ids: Vec<&Id> = ids.collect();
        let mut names = self.names.lock().await;
        if ids.iter().any(|id| !names.by_id.contains_key(*id))
            && let Ok(changes) = self.client.changes(names.generation).await
        {
            Self::remember(&mut names, &changes.changed);
            for id in &changes.deleted {
                names.by_id.remove(id);
            }
            names.generation = names.generation.max(changes.generation);
        }
        ids.into_iter()
            .map(|id| {
                let name = names.by_id.get(id).cloned().unwrap_or_else(|| id.clone());
                (id.clone(), name)
            })
            .collect()
    }

    async fn tool_hits(&self, hits: Vec<Hit>, with_score: bool) -> Vec<ToolHit> {
        let names = self.names_of(hits.iter().map(|h| &h.document_id)).await;
        hits.into_iter()
            .map(|hit| {
                let document = names[&hit.document_id].clone();
                ToolHit {
                    citation: citation(&document, &hit),
                    lines: lines(&hit),
                    document,
                    document_id: hit.document_id,
                    section_path: hit.section_path,
                    page: hit.page,
                    score: with_score.then_some(hit.score),
                    text: hit.text,
                }
            })
            .collect()
    }
}

#[tool_router]
impl Margin {
    #[tool(
        description = "Search Margin (the project's knowledge base: docs, briefs, decisions, chats, session summaries) for passages about something. Returns hits with the document name, section, lines, a citation and the passage text. Search here before reading docs for context."
    )]
    async fn search_context(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<String, String> {
        if args.query.trim().is_empty() {
            return Err("query is empty".into());
        }
        let k = args
            .k
            .unwrap_or(DEFAULT_K)
            .clamp(1, TOOL_SEARCH_K_MAX as u32);
        let query = ContextQuery {
            text: args.query,
            scope: parse_scope(args.scope.as_deref())?,
            modalities: Vec::new(),
            k,
            budget: None,
            access: None,
        };
        let hits = self.client.search(&query).await.map_err(err)?;
        if hits.is_empty() {
            return Ok("No passages matched. Try other words, or a wider scope.".into());
        }
        pretty(&self.tool_hits(hits, true).await)
    }

    #[tool(
        description = "A document's sections in order, each with the passage positions (from_ord, to_ord) to pass to read_passages and its lines or pages."
    )]
    async fn document_outline(
        &self,
        Parameters(args): Parameters<OutlineArgs>,
    ) -> Result<String, String> {
        let outline = self.client.outline(&args.document_id).await.map_err(err)?;
        let names = self.names_of(std::iter::once(&outline.document_id)).await;
        pretty(&json!({
            "document": names[&outline.document_id],
            "document_id": outline.document_id,
            "sections": outline.sections,
        }))
    }

    #[tool(
        description = "Read a document's passages from position `from` to `to` (inclusive, at most 32), e.g. a section from document_outline or the text around a search hit."
    )]
    async fn read_passages(
        &self,
        Parameters(args): Parameters<PassagesArgs>,
    ) -> Result<String, String> {
        if args.to < args.from {
            return Err(format!(
                "`to` ({}) is before `from` ({})",
                args.to, args.from
            ));
        }
        if args.to - args.from + 1 > READ_PASSAGES_LIMIT {
            return Err(format!(
                "that is {} passages; read at most {READ_PASSAGES_LIMIT} per call",
                args.to - args.from + 1
            ));
        }
        let step = TOOL_READ_PASSAGES_MAX as u32;
        let mut hits = Vec::new();
        let mut from = args.from;
        while from <= args.to {
            let to = args.to.min(from + step - 1);
            let part = self
                .client
                .passages(&args.document_id, from, to)
                .await
                .map_err(err)?;
            let short = part.len() < (to - from + 1) as usize;
            hits.extend(part);
            if short {
                break; // past the document's end
            }
            from = to + 1;
        }
        if hits.is_empty() {
            return Ok("No passages at those positions.".into());
        }
        pretty(&self.tool_hits(hits, false).await)
    }

    #[tool(
        description = "Add a document to Margin as agent:claude-code: a design decision (kind \"decision\"), a session handoff (kind \"handoff\") or a note. Write-back is deliberate: use it for what should be found later, not for tool output."
    )]
    async fn ingest_document(
        &self,
        Parameters(args): Parameters<IngestArgs>,
    ) -> Result<String, String> {
        let doc = ingest_text(
            &self.client,
            CLAUDE_CODE,
            &args.name,
            args.kind.as_deref(),
            args.text,
        )
        .await
        .map_err(err)?;
        Self::remember(&mut *self.names.lock().await, std::slice::from_ref(&doc));
        pretty(&json!({
            "document_id": doc.id,
            "name": doc.name,
            "actor": doc.actor,
            "status": doc.status,
            "generation": doc.generation,
        }))
    }

    #[tool(
        description = "What changed in Margin since a generation: documents added or re-indexed and documents deleted. Pass the returned generation next time; omit `since` for everything."
    )]
    async fn recent_changes(
        &self,
        Parameters(args): Parameters<ChangesArgs>,
    ) -> Result<String, String> {
        let changes = self
            .client
            .changes(args.since.unwrap_or(0))
            .await
            .map_err(err)?;
        {
            let mut names = self.names.lock().await;
            Self::remember(&mut names, &changes.changed);
        }
        let changed: Vec<_> = changes
            .changed
            .iter()
            .map(|d| {
                json!({
                    "document_id": d.id,
                    "name": d.name,
                    "actor": d.actor,
                    "origin": origin_label(&d.origin),
                    "status": d.status,
                    "generation": d.generation,
                    "updated_at": d.updated_at,
                })
            })
            .collect();
        pretty(&json!({
            "generation": changes.generation,
            "changed": changed,
            "deleted": changes.deleted,
        }))
    }
}

#[tool_handler]
impl ServerHandler for Margin {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("margin", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

/// Serves MCP on `reader`/`writer` (stdin/stdout in the binary) until the client closes.
pub async fn run<R, W>(client: Client, reader: R, writer: W) -> anyhow::Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let running = Margin::new(client).serve((reader, writer)).await?;
    running.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use margin_shared::context::Modality;

    fn hit() -> Hit {
        Hit {
            document_id: "d".into(),
            unit_id: "u".into(),
            passage_id: "p".into(),
            modality: Modality::Text,
            section_path: "Safety > Backups".into(),
            page: None,
            start_line: Some(40),
            end_line: Some(52),
            start_ms: None,
            end_ms: None,
            text: "t".into(),
            score: 0.5,
            highlight: None,
        }
    }

    #[test]
    fn citations_say_where() {
        assert_eq!(
            citation("docs/plan.md@abc", &hit()),
            "docs/plan.md@abc § Safety > Backups, lines 40-52"
        );
        let mut h = hit();
        h.section_path.clear();
        h.start_line = None;
        h.end_line = None;
        h.page = Some(3);
        h.start_ms = Some(61_000);
        h.end_ms = Some(71_000);
        assert_eq!(citation("talk", &h), "talk, p. 3, 1:01–1:11");
    }

    #[test]
    fn scopes_parse() {
        assert_eq!(parse_scope(None).unwrap(), Scope::Library);
        assert_eq!(parse_scope(Some(" Library ")).unwrap(), Scope::Library);
        assert_eq!(
            parse_scope(Some("topic:t1")).unwrap(),
            Scope::Topic { id: "t1".into() }
        );
        assert_eq!(
            parse_scope(Some("sources:a, b")).unwrap(),
            Scope::Sources {
                ids: vec!["a".into(), "b".into()]
            }
        );
        assert!(parse_scope(Some("topic:")).is_err());
        assert!(parse_scope(Some("galaxy:x")).is_err());
    }
}
