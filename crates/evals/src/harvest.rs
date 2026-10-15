//! `margin-eval harvest` (`docs/plan-dogfood.md` §6): an agent's searches of Margin and
//! the passages it read after them, from the service's trace rows, become candidate
//! retrieval questions. A search through the context API (`margin-mcp`) is a
//! `retrieval` row with `via: "mcp"`, `actor: agent:<name>` (`margin-mcp` names its
//! agent in `margin_shared::context::AGENT_HEADER`) and the query; each
//! `read_passages` that followed it (same agent, within `READ_WINDOW`) marks its document
//! relevant, anchored on the first line read. Candidates go to
//! `evals/golden/candidates.jsonl`; the owner (or `grade-questions`) promotes the serious
//! ones into `evals/golden/dogfood.jsonl`, which `run --suite dogfood` runs over `docs/`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::Context;
use chrono::{DateTime, Duration, Utc};
use margin_shared::context::{CHANGES_PAGE_MAX, ContextChanges, Hit, TraceKind, TraceRow};
use margin_shared::routes;
use serde::{Deserialize, Serialize};

use crate::golden::{QueryKind, Relevant, read_jsonl};
use crate::traces::{self, TraceFilter};

/// Where candidates are appended, under `evals/`.
pub const CANDIDATES_FILE: &str = "golden/candidates.jsonl";
/// The promoted dogfood questions `run --suite dogfood` reads, under `evals/`.
pub const DOGFOOD_FILE: &str = "golden/dogfood.jsonl";
/// The tool a passage read's trace row names.
pub const READ_TOOL: &str = "read_passages";
/// How long after a search a read still counts toward it.
pub const READ_WINDOW: Duration = Duration::minutes(30);
/// The longest anchor kept (the start of the first line read).
const ANCHOR_MAX: usize = 120;

/// One line of `candidates.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub query: String,
    pub relevant: Vec<Relevant>,
    pub kind: QueryKind,
    /// Always "dogfood".
    pub source: String,
    /// `agent:<name>`: who asked.
    pub actor: String,
    pub asked_at: DateTime<Utc>,
}

/// A read that followed a search: the document and the passages read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Read {
    pub document_id: String,
    pub from: u32,
    pub to: u32,
}

/// A search and the documents read after it, in the order first read.
#[derive(Debug, Clone, PartialEq)]
pub struct Pair {
    pub query: String,
    pub actor: String,
    pub at: DateTime<Utc>,
    pub reads: Vec<Read>,
}

fn agent_actor(r: &TraceRow) -> Option<&str> {
    r.actor
        .as_deref()
        .filter(|a| a.starts_with("agent:") && r.via.is_some())
}

/// Pairs each agent search with the reads by the same agent after it (before its next
/// search, within `READ_WINDOW`); searches nothing was read after are left out. Pure.
pub fn pair(rows: &[TraceRow]) -> Vec<Pair> {
    let mut rows: Vec<&TraceRow> = rows.iter().collect();
    rows.sort_by_key(|r| r.at);
    let mut pairs: Vec<Pair> = Vec::new();
    // Per agent, the index in `pairs` of its latest search.
    let mut open: HashMap<&str, usize> = HashMap::new();
    for r in rows {
        let Some(actor) = agent_actor(r) else {
            continue;
        };
        match r.kind {
            TraceKind::Retrieval => {
                let Some(query) = r.query.as_deref().map(str::trim).filter(|q| !q.is_empty())
                else {
                    continue;
                };
                pairs.push(Pair {
                    query: query.to_string(),
                    actor: actor.to_string(),
                    at: r.at,
                    reads: Vec::new(),
                });
                open.insert(actor, pairs.len() - 1);
            }
            TraceKind::ToolCall if r.tool.as_deref() == Some(READ_TOOL) => {
                let (Some(&i), Some(doc), Some([from, to])) =
                    (open.get(actor), r.document_id.as_deref(), r.read_range)
                else {
                    continue;
                };
                let p = &mut pairs[i];
                if r.at - p.at > READ_WINDOW || p.reads.iter().any(|x| x.document_id == doc) {
                    continue;
                }
                p.reads.push(Read {
                    document_id: doc.to_string(),
                    from,
                    to,
                });
            }
            _ => {}
        }
    }
    pairs.retain(|p| !p.reads.is_empty());
    pairs
}

/// A document's path in the dogfood corpus (`docs/`): its name without a leading
/// `docs/` (the repository's documents are named by their path).
pub fn corpus_path(name: &str) -> String {
    name.strip_prefix("docs/").unwrap_or(name).to_string()
}

/// The first non-empty line of `text`, trimmed and cut to `ANCHOR_MAX` characters (a
/// relevant hit's text contains it).
pub fn anchor_of(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    Some(
        line.chars()
            .take(ANCHOR_MAX)
            .collect::<String>()
            .trim_end()
            .to_string(),
    )
}

/// What `harvest` reads from the service: the trace rows, document names and passages.
pub struct Service<'a> {
    pub http: &'a reqwest::Client,
    pub url: &'a str,
    pub token: &'a str,
}

impl Service<'_> {
    /// Every document's name, by id (the change feed from the beginning, page by page).
    async fn names(&self) -> anyhow::Result<HashMap<String, String>> {
        let mut names = HashMap::new();
        let mut since = 0;
        loop {
            let url = format!("{}{}", self.url, routes::CONTEXT_CHANGES);
            let page: ContextChanges = self
                .http
                .get(&url)
                .bearer_auth(self.token)
                .query(&[("since", since), ("limit", CHANGES_PAGE_MAX as u64)])
                .send()
                .await
                .with_context(|| format!("couldn't reach {url}"))?
                .error_for_status()?
                .json()
                .await?;
            for d in page.changed {
                names.insert(d.id, d.name);
            }
            for id in page.deleted {
                names.remove(&id);
            }
            if !page.more || page.generation <= since {
                return Ok(names);
            }
            since = page.generation;
        }
    }

    /// The first line of the passages `from..=to` of a document; `None` when the
    /// document is gone or empty. Sent without the agent header: harvesting isn't an
    /// agent reading.
    async fn first_line(
        &self,
        document_id: &str,
        from: u32,
        to: u32,
    ) -> anyhow::Result<Option<String>> {
        let url = format!(
            "{}{}",
            self.url,
            routes::context_document_passages(document_id)
        );
        let res = self
            .http
            .get(&url)
            .bearer_auth(self.token)
            .query(&[("from", from), ("to", to)])
            .send()
            .await
            .with_context(|| format!("couldn't reach {url}"))?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let hits: Vec<Hit> = res.error_for_status()?.json().await?;
        Ok(hits.iter().find_map(|h| anchor_of(&h.text)))
    }
}

/// The candidates of the agents' searches since `since`: their trace rows paired up,
/// each read document named and anchored.
pub async fn harvest(
    service: &Service<'_>,
    since: Option<DateTime<Utc>>,
) -> anyhow::Result<Vec<Candidate>> {
    let filter = TraceFilter {
        from: since,
        ..Default::default()
    };
    let rows = traces::fetch(service.http, service.url, service.token, &filter).await?;
    let pairs = pair(&rows);
    if pairs.is_empty() {
        return Ok(Vec::new());
    }
    let names = service.names().await?;
    let mut out = Vec::new();
    for p in pairs {
        let mut relevant = Vec::new();
        for r in &p.reads {
            let Some(name) = names.get(&r.document_id) else {
                continue;
            };
            let Some(anchor) = service.first_line(&r.document_id, r.from, r.to).await? else {
                continue;
            };
            relevant.push(Relevant {
                document: corpus_path(name),
                page: None,
                anchor: Some(anchor),
            });
        }
        if relevant.is_empty() {
            continue;
        }
        out.push(Candidate {
            query: p.query,
            relevant,
            kind: QueryKind::Semantic,
            source: "dogfood".into(),
            actor: p.actor,
            asked_at: p.at,
        });
    }
    Ok(out)
}

fn query_key(q: &str) -> String {
    q.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Appends the candidates whose query isn't in `path` yet (nor twice in `new`); returns
/// how many were added.
pub fn append_candidates(path: &Path, new: &[Candidate]) -> anyhow::Result<usize> {
    let existing: Vec<Candidate> = if path.is_file() {
        read_jsonl(path)?
    } else {
        Vec::new()
    };
    let mut seen: HashSet<String> = existing.iter().map(|c| query_key(&c.query)).collect();
    let mut text = String::new();
    let mut added = 0;
    for c in new {
        if seen.insert(query_key(&c.query)) {
            text.push_str(&serde_json::to_string(c)?);
            text.push('\n');
            added += 1;
        }
    }
    if added > 0 {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("couldn't open {}", path.display()))?;
        f.write_all(text.as_bytes())?;
    }
    Ok(added)
}

/// Candidates per agent, for the summary line.
pub fn by_actor(c: &[Candidate]) -> BTreeMap<&str, usize> {
    let mut m = BTreeMap::new();
    for x in c {
        *m.entry(x.actor.as_str()).or_insert(0) += 1;
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Path as AxPath, Query};
    use axum::routing::get;
    use axum::{Json, Router};
    use margin_shared::context::{Document, DocumentStatus, Modality, Origin};

    fn at(min: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_760_000_000 + min * 60, 0).unwrap()
    }

    fn search(actor: &str, q: &str, min: i64) -> TraceRow {
        let mut r = TraceRow::new(TraceKind::Retrieval, format!("s{min}{actor}"), at(min));
        r.actor = Some(actor.into());
        r.via = Some("mcp".into());
        r.query = Some(q.into());
        r
    }

    fn read(actor: &str, doc: &str, min: i64) -> TraceRow {
        let mut r = TraceRow::new(TraceKind::ToolCall, format!("r{min}{actor}{doc}"), at(min));
        r.actor = Some(actor.into());
        r.via = Some("mcp".into());
        r.tool = Some(READ_TOOL.into());
        r.document_id = Some(doc.into());
        r.read_range = Some([2, 4]);
        r
    }

    #[test]
    fn searches_pair_with_the_reads_that_follow_them() {
        let cc = "agent:claude-code";
        let other = "agent:other";
        let mut app = TraceRow::new(TraceKind::Retrieval, "app", at(0));
        app.query_hash = Some("h".into());
        let rows = vec![
            app,
            search(cc, "how is prod deployed", 0),
            read(cc, "deploy", 1),
            read(cc, "deploy", 2),       // the same document again: once
            read(other, "elsewhere", 2), // another agent, no search of its own
            read(cc, "runbook", 3),
            search(cc, "nothing read after this", 10),
            search(cc, "late reads", 20),
            read(cc, "too-late", 20 + 31),
        ];
        let pairs = pair(&rows);
        assert_eq!(pairs.len(), 1, "{pairs:?}");
        assert_eq!(pairs[0].query, "how is prod deployed");
        assert_eq!(pairs[0].actor, cc);
        let docs: Vec<&str> = pairs[0]
            .reads
            .iter()
            .map(|r| r.document_id.as_str())
            .collect();
        assert_eq!(docs, ["deploy", "runbook"]);
        assert_eq!((pairs[0].reads[0].from, pairs[0].reads[0].to), (2, 4));
    }

    #[test]
    fn anchors_and_paths() {
        assert_eq!(
            anchor_of("\n\n  ## Deploys  \nmore").as_deref(),
            Some("## Deploys")
        );
        assert_eq!(anchor_of("   \n"), None);
        assert_eq!(anchor_of(&"x".repeat(300)).unwrap().len(), ANCHOR_MAX);
        assert_eq!(corpus_path("docs/plan-deploy.md"), "plan-deploy.md");
        assert_eq!(corpus_path("Decision: x"), "Decision: x");
    }

    #[test]
    fn appending_skips_queries_already_there() {
        let dir = std::env::temp_dir().join(format!("margin-harvest-{}", std::process::id()));
        let path = dir.join("golden/candidates.jsonl");
        let _ = std::fs::remove_file(&path);
        let c = |q: &str| Candidate {
            query: q.into(),
            relevant: vec![Relevant {
                document: "plan-deploy.md".into(),
                page: None,
                anchor: Some("## Deploys".into()),
            }],
            kind: QueryKind::Semantic,
            source: "dogfood".into(),
            actor: "agent:claude-code".into(),
            asked_at: at(0),
        };
        assert_eq!(
            append_candidates(&path, &[c("A b"), c("a  B"), c("c")]).unwrap(),
            2
        );
        assert_eq!(append_candidates(&path, &[c("C"), c("d")]).unwrap(), 1);
        let back: Vec<Candidate> = read_jsonl(&path).unwrap();
        let qs: Vec<&str> = back.iter().map(|c| c.query.as_str()).collect();
        assert_eq!(qs, ["A b", "c", "d"]);
        let line = std::fs::read_to_string(&path).unwrap();
        assert!(line.contains(r#""source":"dogfood""#) && line.contains(r#""kind":"semantic""#));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[derive(Deserialize)]
    struct Since {
        since: u64,
    }

    /// A fake service: two documents, the agent's traces, passages by range.
    async fn fake_service() -> String {
        let doc = |id: &str, name: &str, g: u64| Document {
            owner: None,
            id: id.into(),
            project_id: "p".into(),
            origin: Origin::Agent {
                name: "repo".into(),
            },
            name: name.into(),
            kind: margin_shared::SourceKind::Text,
            status: DocumentStatus::Ready { passages: 5 },
            generation: g,
            bytes: 1,
            actor: "agent:repo".into(),
            access: vec!["company".into()],
            created_at: at(0),
            updated_at: at(0),
        };
        let docs = vec![
            doc("deploy", "docs/plan-deploy.md", 1),
            doc("gone", "docs/old.md", 2),
        ];
        let traces: String = [
            search("agent:claude-code", "how is prod deployed", 0),
            read("agent:claude-code", "deploy", 1),
            read("agent:claude-code", "gone", 2),
        ]
        .iter()
        .map(|r| serde_json::to_string(r).unwrap() + "\n")
        .collect();
        let app = Router::new()
            .route(routes::TRACES, get(move || async move { traces.clone() }))
            .route(
                routes::CONTEXT_CHANGES,
                get(move |Query(q): Query<Since>| {
                    let docs = docs.clone();
                    async move {
                        // Two pages, to follow `more`.
                        let page: Vec<Document> = docs
                            .into_iter()
                            .filter(|d| d.generation > q.since)
                            .take(1)
                            .collect();
                        Json(ContextChanges {
                            generation: page.last().map_or(q.since, |d| d.generation),
                            more: q.since == 0,
                            changed: page,
                            deleted: Vec::new(),
                        })
                    }
                }),
            )
            .route(
                "/api/context/documents/{id}/passages",
                get(|AxPath(id): AxPath<String>| async move {
                    if id != "deploy" {
                        return Err(axum::http::StatusCode::NOT_FOUND);
                    }
                    Ok(Json(vec![Hit {
                        document_id: id,
                        unit_id: "u".into(),
                        passage_id: "p".into(),
                        modality: Modality::Text,
                        section_path: String::new(),
                        page: None,
                        start_line: Some(3),
                        end_line: Some(9),
                        start_ms: None,
                        end_ms: None,
                        text: "\n## Deploys\n\nProd is promoted by hand.".into(),
                        score: 1.0,
                        highlight: None,
                    }]))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    #[tokio::test]
    async fn harvest_turns_an_agents_search_and_reads_into_a_candidate() {
        let url = fake_service().await;
        let http = reqwest::Client::new();
        let service = Service {
            http: &http,
            url: &url,
            token: "t",
        };
        let got = harvest(&service, None).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].query, "how is prod deployed");
        assert_eq!(got[0].source, "dogfood");
        // The read of a document that's gone since is dropped.
        assert_eq!(
            got[0].relevant,
            vec![Relevant {
                document: "plan-deploy.md".into(),
                page: None,
                anchor: Some("## Deploys".into()),
            }]
        );
        assert_eq!(by_actor(&got)["agent:claude-code"], 1);
    }
}
