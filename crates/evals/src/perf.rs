//! `margin-eval perf --service-url …` (`docs/phase5-brief.md` D8): ingests
//! `evals/corpus` through the service under test, runs the retrieval golden set over
//! `POST /api/context/search`, runs one agent turn and one lesson, reads the trace rows
//! back and prints each budget of `evals/budgets.toml` as met or missed. Reported, not
//! gated: machines differ.
//!
//! What it leaves behind on the service: nothing it made. The corpus goes in as an
//! agent's documents (`perf/<path>`, agent `perf`), the turn runs in a new chat and the
//! lesson in a new topic; all of them are deleted at the end, also after a failure.
//! The trace rows stay (they are the measurement).

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use chrono::Utc;
use margin_service::version::VersionInfo;
use margin_shared::context::{
    AGENT_HEADER, ContextIngest, ContextQuery, Document, Hit, Origin, Scope, TraceKind, TraceRow,
};
use margin_shared::pipeline::Route;
use margin_shared::routes;
use margin_shared::{
    AcceptProposal, Card, ChatDetail, ChatItemKind, ChatState, LessonStatus, Library, NewChat,
    NewMessage, OutlineChips, Proposal, ProposalStatus, SourceKind, Topic,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::corpus;
use crate::golden::{AgentTask, RetrievalQuery, read_jsonl};
use crate::metrics::percentile;
use crate::traces::{self, Overhead, TraceFilter};

/// The budgets, under the evals folder.
pub const BUDGETS_FILE: &str = "budgets.toml";
/// The agent name the corpus is ingested and searched as (`agent:perf` in traces).
pub const AGENT: &str = "perf";
/// The documents' name prefix.
pub const DOC_PREFIX: &str = "perf/";
/// Hits per search.
const SEARCH_K: u32 = 10;
/// How often waits look again.
const POLL: Duration = Duration::from_millis(500);
/// Longest waits.
const INGEST_WAIT: Duration = Duration::from_secs(30 * 60);
const TURN_WAIT: Duration = Duration::from_secs(10 * 60);
const TITLE_WAIT: Duration = Duration::from_secs(60);
const PROPOSAL_WAIT: Duration = Duration::from_secs(5 * 60);
const LESSON_WAIT: Duration = Duration::from_secs(15 * 60);
/// Trace rows are read from this long before the run started, so a service clock a
/// little behind the runner's loses nothing (rows are told apart by id, not time).
const CLOCK_SLACK: chrono::Duration = chrono::Duration::minutes(15);

/// `evals/budgets.toml` (D8's table).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Budgets {
    pub ingest: IngestBudget,
    pub retrieval: RetrievalBudget,
    pub context_search: SearchBudget,
    pub agent_step: StepBudget,
    pub cache: CacheBudget,
    pub title: TitleBudget,
    pub memory: MemoryBudget,
    pub folder_sync: FolderSyncBudget,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct IngestBudget {
    pub min_chunks_per_s: f64,
    pub unchanged_reingest_max_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RetrievalBudget {
    pub p50_max_ms: f64,
    pub p95_max_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SearchBudget {
    pub p95_max_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct StepBudget {
    pub overhead_max_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CacheBudget {
    pub min_cached_share: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TitleBudget {
    pub max_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MemoryBudget {
    pub idle_max_bytes: u64,
    pub loaded_max_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct FolderSyncBudget {
    pub manifest_max_ms: f64,
    pub min_uploads_per_s: f64,
}

impl Budgets {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("couldn't read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("{} isn't the budget table", path.display()))
    }
}

/// What one run measured; `None` where it couldn't be measured (the run says why in
/// `notes`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Measured {
    pub docs: usize,
    pub docs_failed: usize,
    pub chunks_new: u64,
    /// New chunks over the wall time from the first ingest request to the last
    /// document's ingest row.
    pub chunks_per_s: Option<f64>,
    /// The ingest row of an unchanged document sent again.
    pub unchanged_reingest_ms: Option<f64>,
    pub retrievals: usize,
    pub retrieval_p50_ms: Option<f64>,
    pub retrieval_p95_ms: Option<f64>,
    pub searches: usize,
    /// The runner's own timing of each `POST /api/context/search`.
    pub search_p95_ms: Option<f64>,
    pub step_overhead: Option<Overhead>,
    /// `usage.cached / usage.input` over the turn's steps from step 2.
    pub cached_share: Option<f64>,
    /// The title call's input + output tokens.
    pub title_tokens: Option<u64>,
    /// `rss_bytes` once the corpus is read and searched (the text embedder loaded).
    pub rss_idle: Option<u64>,
    /// The most `rss_bytes` seen over the run.
    pub rss_peak: Option<u64>,
    /// The lesson's wall time and model-call time.
    pub lesson_ms: Option<f64>,
    pub lesson_model_ms: Option<f64>,
    pub notes: Vec<String>,
}

/// Met, missed, not measured, or checked by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Met,
    Missed,
    NotMeasured,
    Manual,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Verdict::Met => "met",
            Verdict::Missed => "MISSED",
            Verdict::NotMeasured => "not measured",
            Verdict::Manual => "manual",
        }
    }
}

/// One line of the printed table.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub what: String,
    pub budget: String,
    pub measured: String,
    pub verdict: Verdict,
}

fn at_most(v: Option<f64>, max: f64) -> Verdict {
    match v {
        Some(v) if v <= max => Verdict::Met,
        Some(_) => Verdict::Missed,
        None => Verdict::NotMeasured,
    }
}

fn at_least(v: Option<f64>, min: f64) -> Verdict {
    match v {
        Some(v) if v >= min => Verdict::Met,
        Some(_) => Verdict::Missed,
        None => Verdict::NotMeasured,
    }
}

fn ms(v: Option<f64>) -> String {
    v.map_or_else(|| "–".into(), |v| format!("{v:.0} ms"))
}

fn gib(b: u64) -> String {
    format!("{:.2} GiB", b as f64 / (1024.0 * 1024.0 * 1024.0))
}

/// Each budget against what was measured.
pub fn verdicts(b: &Budgets, m: &Measured) -> Vec<Line> {
    let line = |what: &str, budget: String, measured: String, verdict| Line {
        what: what.into(),
        budget,
        measured,
        verdict,
    };
    vec![
        line(
            "Ingest, text (chunks/s, end to end)",
            format!("≥ {}", b.ingest.min_chunks_per_s),
            m.chunks_per_s.map_or_else(
                || "–".into(),
                |v| format!("{v:.2} ({} chunks, {} documents)", m.chunks_new, m.docs),
            ),
            at_least(m.chunks_per_s, b.ingest.min_chunks_per_s),
        ),
        line(
            "Unchanged re-ingest",
            format!("≤ {} ms", b.ingest.unchanged_reingest_max_ms),
            ms(m.unchanged_reingest_ms),
            at_most(m.unchanged_reingest_ms, b.ingest.unchanged_reingest_max_ms),
        ),
        line(
            "Retrieval p50",
            format!("≤ {} ms", b.retrieval.p50_max_ms),
            ms(m.retrieval_p50_ms),
            at_most(m.retrieval_p50_ms, b.retrieval.p50_max_ms),
        ),
        line(
            "Retrieval p95",
            format!("≤ {} ms", b.retrieval.p95_max_ms),
            m.retrieval_p95_ms.map_or_else(
                || "–".into(),
                |v| format!("{v:.0} ms over {} retrievals", m.retrievals),
            ),
            at_most(m.retrieval_p95_ms, b.retrieval.p95_max_ms),
        ),
        line(
            "Context search p95, end to end",
            format!("≤ {} ms", b.context_search.p95_max_ms),
            m.search_p95_ms.map_or_else(
                || "–".into(),
                |v| format!("{v:.0} ms over {} searches", m.searches),
            ),
            at_most(m.search_p95_ms, b.context_search.p95_max_ms),
        ),
        line(
            "Agent step overhead (mean)",
            format!("≤ {} ms", b.agent_step.overhead_max_ms),
            m.step_overhead.map_or_else(
                || "–".into(),
                |o| {
                    format!(
                        "{:.0} ms over {} calls (p95 {:.0}, max {:.0})",
                        o.mean_ms, o.steps, o.p95_ms, o.max_ms
                    )
                },
            ),
            at_most(
                m.step_overhead.map(|o| o.mean_ms),
                b.agent_step.overhead_max_ms,
            ),
        ),
        line(
            "Cached input share from step 2",
            format!("≥ {:.0} %", b.cache.min_cached_share * 100.0),
            m.cached_share
                .map_or_else(|| "–".into(), |v| format!("{:.1} %", v * 100.0)),
            at_least(m.cached_share, b.cache.min_cached_share),
        ),
        line(
            "Title tokens",
            format!("≤ {}", b.title.max_tokens),
            m.title_tokens.map_or_else(|| "–".into(), |v| v.to_string()),
            at_most(m.title_tokens.map(|t| t as f64), b.title.max_tokens as f64),
        ),
        line(
            "Service RSS, text embedder loaded",
            format!("≤ {}", gib(b.memory.idle_max_bytes)),
            m.rss_idle.map_or_else(|| "–".into(), gib),
            at_most(m.rss_idle.map(|v| v as f64), b.memory.idle_max_bytes as f64),
        ),
        line(
            "Service RSS, most seen in the run",
            format!("≤ {}", gib(b.memory.loaded_max_bytes)),
            m.rss_peak.map_or_else(|| "–".into(), gib),
            at_most(
                m.rss_peak.map(|v| v as f64),
                b.memory.loaded_max_bytes as f64,
            ),
        ),
        line(
            "Folder sync manifest, 1 000 files",
            format!("≤ {} ms", b.folder_sync.manifest_max_ms),
            "on an iPhone: app log + WorkspaceChanged timings".into(),
            Verdict::Manual,
        ),
        line(
            "Folder sync uploads",
            format!("≥ {} files/s", b.folder_sync.min_uploads_per_s),
            "on an iPhone: app log + WorkspaceChanged timings".into(),
            Verdict::Manual,
        ),
    ]
}

/// The table `run` prints, then the notes.
pub fn render(b: &Budgets, m: &Measured) -> String {
    let lines = verdicts(b, m);
    let mut s = String::new();
    let _ = writeln!(s, "| Budget | Target | Measured | |\n|---|---|---|---|");
    for l in &lines {
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} |",
            l.what,
            l.budget,
            l.measured,
            l.verdict.as_str()
        );
    }
    let missed = lines
        .iter()
        .filter(|l| l.verdict == Verdict::Missed)
        .count();
    let met = lines.iter().filter(|l| l.verdict == Verdict::Met).count();
    let _ = writeln!(
        s,
        "\n{met} met, {missed} missed (reported, not gated: machines differ)."
    );
    if let Some(l) = m.lesson_ms {
        let _ = writeln!(
            s,
            "Lesson: {:.1} s end to end, {} in model calls.",
            l / 1000.0,
            m.lesson_model_ms
                .map_or_else(|| "–".into(), |v| format!("{:.1} s", v / 1000.0))
        );
    }
    for n in &m.notes {
        let _ = writeln!(s, "Note: {n}");
    }
    s
}

/// The agent numbers of one turn's trace rows: the step overhead, the cached share
/// from step 2, and the title call's tokens.
pub fn turn_numbers(rows: &[TraceRow]) -> (Option<Overhead>, Option<f64>, Option<u64>) {
    let steps: Vec<&TraceRow> = rows
        .iter()
        .filter(|r| r.kind == TraceKind::ModelCall && r.route == Some(Route::Chat))
        .collect();
    let owned: Vec<TraceRow> = steps.iter().map(|r| (*r).clone()).collect();
    let overhead = Overhead::of(&owned);
    let (cached, input) = steps
        .iter()
        .filter(|r| r.step.unwrap_or(0) >= 2)
        .filter_map(|r| r.usage)
        .fold((0u64, 0u64), |(c, i), u| (c + u.cached, i + u.input));
    let cached_share = (input > 0).then(|| cached as f64 / input as f64);
    let title = rows
        .iter()
        .filter(|r| r.kind == TraceKind::ModelCall)
        .find(|r| {
            r.prompt_version
                .as_deref()
                .is_some_and(|v| v.starts_with("title-"))
        })
        .and_then(|r| r.usage)
        .map(|u| u.input + u.output);
    (overhead, cached_share, title)
}

/// The service under test.
struct Service<'a> {
    http: &'a reqwest::Client,
    url: &'a str,
    token: &'a str,
}

impl Service<'_> {
    fn u(&self, path: &str) -> String {
        format!("{}{path}", self.url.trim_end_matches('/'))
    }

    async fn check(res: reqwest::Response, what: &str) -> anyhow::Result<reqwest::Response> {
        let status = res.status();
        if status.is_success() {
            return Ok(res);
        }
        let body = res.text().await.unwrap_or_default();
        bail!(
            "{what} answered {status}: {}",
            body.chars().take(300).collect::<String>()
        )
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        let res = self
            .http
            .get(self.u(path))
            .bearer_auth(self.token)
            .send()
            .await
            .with_context(|| format!("couldn't reach {}", self.u(path)))?;
        Ok(Self::check(res, path).await?.json().await?)
    }

    async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &impl serde::Serialize,
    ) -> anyhow::Result<T> {
        let res = self
            .http
            .post(self.u(path))
            .bearer_auth(self.token)
            .header(AGENT_HEADER, AGENT)
            .json(body)
            .send()
            .await
            .with_context(|| format!("couldn't reach {}", self.u(path)))?;
        Ok(Self::check(res, path).await?.json().await?)
    }

    async fn delete(&self, path: &str) -> anyhow::Result<()> {
        let res = self
            .http
            .delete(self.u(path))
            .bearer_auth(self.token)
            .send()
            .await?;
        if res.status().is_success() || res.status() == reqwest::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Self::check(res, path).await.map(|_| ())
        }
    }

    async fn rows(&self, filter: TraceFilter) -> anyhow::Result<Vec<TraceRow>> {
        traces::fetch(self.http, self.url, self.token, &filter).await
    }

    async fn rss(&self) -> Option<u64> {
        let v: VersionInfo = self.get(routes::VERSION).await.ok()?;
        v.rss_bytes
    }
}

/// What the run made, deleted at the end.
#[derive(Default)]
struct Made {
    documents: Vec<String>,
    chats: Vec<String>,
    topics: Vec<String>,
}

/// Measures the service at `url` against `evals/budgets.toml`.
pub async fn run(
    http: &reqwest::Client,
    url: &str,
    token: &str,
    evals: &Path,
) -> anyhow::Result<()> {
    let budgets = Budgets::load(&evals.join(BUDGETS_FILE))?;
    let svc = Service { http, url, token };
    let mut made = Made::default();
    let measured = measure(&svc, evals, &mut made).await;
    let cleaned = clean_up(&svc, &made).await;
    let measured = measured?;
    print!("{}", render(&budgets, &measured));
    if let Err(e) = cleaned {
        eprintln!("margin-eval perf: cleaning up: {e:#}");
    }
    Ok(())
}

async fn clean_up(svc: &Service<'_>, made: &Made) -> anyhow::Result<()> {
    let mut first: Option<anyhow::Error> = None;
    let mut keep = |r: anyhow::Result<()>| {
        if let Err(e) = r {
            first.get_or_insert(e);
        }
    };
    for id in &made.chats {
        keep(svc.delete(&routes::chat(id)).await);
    }
    for id in &made.topics {
        keep(svc.delete(&routes::topic(id)).await);
    }
    for id in &made.documents {
        keep(svc.delete(&routes::context_document(id)).await);
    }
    first.map_or(Ok(()), Err)
}

/// Waits until rows of `kind` not in `known` satisfy `done`, and returns them.
async fn new_rows(
    svc: &Service<'_>,
    kind: TraceKind,
    from: chrono::DateTime<Utc>,
    known: &HashSet<String>,
    wait: Duration,
    done: impl Fn(&[TraceRow]) -> bool,
) -> anyhow::Result<Vec<TraceRow>> {
    let started = Instant::now();
    loop {
        let rows: Vec<TraceRow> = svc
            .rows(TraceFilter {
                kind: Some(kind),
                from: Some(from),
                ..Default::default()
            })
            .await?
            .into_iter()
            .filter(|r| !known.contains(&r.id))
            .collect();
        if done(&rows) {
            return Ok(rows);
        }
        if started.elapsed() > wait {
            bail!(
                "waited {} s for {} trace rows",
                wait.as_secs(),
                kind.as_str()
            );
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn known_rows(
    svc: &Service<'_>,
    kind: TraceKind,
    from: chrono::DateTime<Utc>,
) -> anyhow::Result<HashSet<String>> {
    Ok(svc
        .rows(TraceFilter {
            kind: Some(kind),
            from: Some(from),
            ..Default::default()
        })
        .await?
        .into_iter()
        .map(|r| r.id)
        .collect())
}

fn ingest_rows_for<'a>(rows: &'a [TraceRow], ids: &'a HashSet<String>) -> Vec<&'a TraceRow> {
    rows.iter()
        .filter(|r| r.document_id.as_ref().is_some_and(|d| ids.contains(d)))
        .collect()
}

async fn measure(svc: &Service<'_>, evals: &Path, made: &mut Made) -> anyhow::Result<Measured> {
    let mut m = Measured::default();
    let from = Utc::now() - CLOCK_SLACK;
    let mut rss_seen: Vec<u64> = svc.rss().await.into_iter().collect();

    // 1. Ingest the corpus's text documents as agent documents.
    let corpus_dir = evals.join("corpus");
    let mut texts = Vec::new();
    for doc in corpus::documents(&corpus_dir)? {
        if !matches!(doc.kind, SourceKind::Text | SourceKind::Code) {
            continue;
        }
        match std::fs::read_to_string(corpus_dir.join(&doc.path)) {
            Ok(text) if !text.trim().is_empty() => texts.push((doc, text)),
            _ => {}
        }
    }
    anyhow::ensure!(
        !texts.is_empty(),
        "no text documents in {}",
        corpus_dir.display()
    );
    let known = known_rows(svc, TraceKind::Ingest, from).await?;
    let started = Instant::now();
    let mut ids = HashSet::new();
    for (doc, text) in &texts {
        let d: Document = svc
            .post(
                routes::CONTEXT_INGEST,
                &ContextIngest {
                    name: format!("{DOC_PREFIX}{}", doc.path),
                    origin: Origin::Agent { name: AGENT.into() },
                    kind: doc.kind,
                    text: text.clone(),
                    audience: None,
                },
            )
            .await?;
        made.documents.push(d.id.clone());
        ids.insert(d.id);
    }
    let rows = new_rows(svc, TraceKind::Ingest, from, &known, INGEST_WAIT, |rows| {
        let seen: HashSet<&String> = ingest_rows_for(rows, &ids)
            .iter()
            .filter_map(|r| r.document_id.as_ref())
            .collect();
        seen.len() == ids.len()
    })
    .await?;
    let wall = started.elapsed().as_secs_f64();
    let ours = ingest_rows_for(&rows, &ids);
    m.docs = ids.len();
    m.docs_failed = ours.iter().filter(|r| !r.ok).count();
    m.chunks_new = ours
        .iter()
        .map(|r| u64::from(r.chunks_new.unwrap_or(0)))
        .sum();
    m.chunks_per_s = (wall > 0.0).then(|| m.chunks_new as f64 / wall);
    if m.docs_failed > 0 {
        m.notes.push(format!(
            "{} of {} documents failed to ingest",
            m.docs_failed, m.docs
        ));
    }

    // 2. The same document again, unchanged.
    let (doc, text) = &texts[0];
    let known = known_rows(svc, TraceKind::Ingest, from).await?;
    let again: Document = svc
        .post(
            routes::CONTEXT_INGEST,
            &ContextIngest {
                name: format!("{DOC_PREFIX}{}", doc.path),
                origin: Origin::Agent { name: AGENT.into() },
                kind: doc.kind,
                text: text.clone(),
                audience: None,
            },
        )
        .await?;
    let one: HashSet<String> = [again.id.clone()].into();
    let rows = new_rows(svc, TraceKind::Ingest, from, &known, INGEST_WAIT, |rows| {
        !ingest_rows_for(rows, &one).is_empty()
    })
    .await?;
    if let Some(r) = ingest_rows_for(&rows, &one).first() {
        m.unchanged_reingest_ms = Some(r.duration_ms);
        if r.chunks_new.unwrap_or(0) > 0 {
            m.notes.push(format!(
                "the unchanged re-ingest embedded {} chunks again",
                r.chunks_new.unwrap_or(0)
            ));
        }
    }

    // 3. The retrieval golden set over the context API.
    let queries: Vec<RetrievalQuery> = read_jsonl(&evals.join("golden/retrieval.jsonl"))?;
    let known = known_rows(svc, TraceKind::Retrieval, from).await?;
    let mut search_ms = Vec::new();
    for q in &queries {
        let t = Instant::now();
        let _: Vec<Hit> = svc
            .post(
                routes::CONTEXT_SEARCH,
                &ContextQuery {
                    text: q.query.clone(),
                    scope: Scope::Library,
                    modalities: Vec::new(),
                    k: SEARCH_K,
                    budget: None,
                    access: None,
                },
            )
            .await?;
        search_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    m.searches = search_ms.len();
    m.search_p95_ms = (!search_ms.is_empty()).then(|| percentile(&search_ms, 95.0));
    let agent = format!("agent:{AGENT}");
    let rows = new_rows(
        svc,
        TraceKind::Retrieval,
        from,
        &known,
        TITLE_WAIT,
        |rows| {
            rows.iter()
                .filter(|r| r.actor.as_deref() == Some(agent.as_str()))
                .count()
                >= queries.len()
        },
    )
    .await
    .unwrap_or_default();
    let retrieval_ms: Vec<f64> = rows
        .iter()
        .filter(|r| r.actor.as_deref() == Some(agent.as_str()))
        .map(|r| r.duration_ms)
        .collect();
    m.retrievals = retrieval_ms.len();
    if !retrieval_ms.is_empty() {
        m.retrieval_p50_ms = Some(percentile(&retrieval_ms, 50.0));
        m.retrieval_p95_ms = Some(percentile(&retrieval_ms, 95.0));
    }
    m.rss_idle = svc.rss().await;
    rss_seen.extend(m.rss_idle);

    // 4. One agent turn: a golden agent question.
    let tasks: Vec<AgentTask> = read_jsonl(&evals.join("golden/agent.jsonl"))?;
    let task = tasks.first().context("evals/golden/agent.jsonl is empty")?;
    let chat: ChatDetail = svc
        .post(
            routes::CHATS,
            &NewChat {
                message: Some(NewMessage {
                    text: task.question.clone(),
                    source_ids: Vec::new(),
                }),
                workspace_ids: Vec::new(),
                provider: None,
            },
        )
        .await?;
    let chat_id = chat.chat.id.clone();
    made.chats.push(chat_id.clone());
    let first_turn = turn_of(&chat).context("the chat started no turn")?;
    let d = wait_turn(svc, &chat_id, &mut m).await?;
    // The title call follows the first answer.
    let titled = Instant::now();
    let mut d = d;
    while d.chat.title_by != margin_shared::TitleSource::Model && titled.elapsed() < TITLE_WAIT {
        tokio::time::sleep(POLL).await;
        d = svc.get(&routes::chat(&chat_id)).await?;
    }
    let rows = svc
        .rows(TraceFilter {
            trace_id: Some(first_turn.clone()),
            from: Some(from),
            ..Default::default()
        })
        .await?;
    let (overhead, cached, title) = turn_numbers(&rows);
    m.step_overhead = overhead;
    m.cached_share = cached;
    m.title_tokens = title;
    if cached.is_none() {
        m.notes
            .push("the turn took one step: no cached share from step 2".into());
    }
    if title.is_none() {
        m.notes.push("no title call was traced for the chat".into());
    }
    rss_seen.extend(svc.rss().await);

    // 5. One lesson: ask for a one-lesson course, accept its outline with one lesson.
    match lesson(svc, &chat_id, task, &mut m, made).await {
        Ok(()) => {}
        Err(e) => m.notes.push(format!("lesson not measured: {e:#}")),
    }
    rss_seen.extend(svc.rss().await);
    m.rss_peak = rss_seen.into_iter().max();
    Ok(m)
}

/// The turn a new chat's first message started.
fn turn_of(d: &ChatDetail) -> Option<String> {
    match &d.chat.state {
        ChatState::Running { turn_id, .. } | ChatState::WaitingForUser { turn_id, .. } => {
            Some(turn_id.clone())
        }
        _ => d.items.last().map(|i| i.turn_id.clone()),
    }
}

/// Waits for the chat's turn to end; a confirmation card is denied (perf changes
/// nothing of the owner's).
async fn wait_turn(
    svc: &Service<'_>,
    chat_id: &str,
    m: &mut Measured,
) -> anyhow::Result<ChatDetail> {
    let started = Instant::now();
    loop {
        let d: ChatDetail = svc.get(&routes::chat(chat_id)).await?;
        match &d.chat.state {
            ChatState::Running { .. } => {}
            ChatState::WaitingForUser { .. } => {
                m.notes
                    .push("the agent asked for a confirmation; perf stopped the turn".into());
                let _: serde_json::Value = svc
                    .post(&routes::chat_stop(chat_id), &serde_json::json!({}))
                    .await?;
            }
            ChatState::Failed { error, .. } => bail!("the turn failed: {error}"),
            ChatState::Interrupted { .. } => bail!("the turn was interrupted"),
            _ => return Ok(d),
        }
        if started.elapsed() > TURN_WAIT {
            bail!("the turn ran past {} s", TURN_WAIT.as_secs());
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn lesson(
    svc: &Service<'_>,
    chat_id: &str,
    task: &AgentTask,
    m: &mut Measured,
    made: &mut Made,
) -> anyhow::Result<()> {
    let before: Library = svc.get(routes::LIBRARY).await?;
    let topics_before: HashSet<String> = before.topics.iter().map(|t| t.id.clone()).collect();
    let _: serde_json::Value = svc
        .post(
            &routes::chat_messages(chat_id),
            &NewMessage {
                text: format!(
                    "Propose a one-lesson course from my library that answers: {}",
                    task.question
                ),
                source_ids: Vec::new(),
            },
        )
        .await?;
    // The message may wait behind the title call's turn end; give the turn a moment.
    tokio::time::sleep(POLL).await;
    let d = wait_turn(svc, chat_id, m).await?;
    let proposal_id = d
        .items
        .iter()
        .rev()
        .find_map(|i| match &i.kind {
            ChatItemKind::Card {
                card: Card::Outline(o),
                ..
            } => Some(o.proposal_id.clone()),
            _ => None,
        })
        .context("the agent proposed no course")?;
    let started = Instant::now();
    let proposal = loop {
        let p: Proposal = svc.get(&routes::proposal(&proposal_id)).await?;
        match &p.status {
            ProposalStatus::Ready => break p,
            ProposalStatus::Drafting => {}
            other => bail!("the proposal ended {other:?}"),
        }
        if started.elapsed() > PROPOSAL_WAIT {
            bail!(
                "the outline wasn't ready after {} s",
                PROPOSAL_WAIT.as_secs()
            );
        }
        tokio::time::sleep(POLL).await;
    };
    let mut outline = proposal
        .outline
        .context("a ready proposal without an outline")?;
    outline.lessons.truncate(1);
    let started = Instant::now();
    let topic: Topic = svc
        .post(
            &routes::proposal_accept(&proposal_id),
            &AcceptProposal {
                outline,
                chips: OutlineChips {
                    with_videos: Some(false),
                    ..Default::default()
                },
            },
        )
        .await?;
    // Every topic the accept made (the topic, and its subject when that was new).
    let after: Library = svc.get(routes::LIBRARY).await?;
    let mut new_topics: Vec<&Topic> = after
        .topics
        .iter()
        .filter(|t| !topics_before.contains(&t.id))
        .collect();
    new_topics.sort_by_key(|t| t.parent_id.is_none());
    made.topics.extend(new_topics.iter().map(|t| t.id.clone()));
    if !made.topics.contains(&topic.id) {
        made.topics.push(topic.id.clone());
    }
    let lesson = loop {
        let lib: Library = svc.get(routes::LIBRARY).await?;
        let lessons: Vec<_> = lib
            .lessons
            .into_iter()
            .filter(|l| l.topic_id == topic.id)
            .collect();
        if let Some(l) = lessons.first() {
            match &l.status {
                LessonStatus::Ready => break l.clone(),
                LessonStatus::Failed { error } => bail!("the lesson failed: {error}"),
                _ => {}
            }
        }
        if started.elapsed() > LESSON_WAIT {
            bail!("the lesson wasn't ready after {} s", LESSON_WAIT.as_secs());
        }
        tokio::time::sleep(POLL).await;
    };
    m.lesson_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
    let rows = svc
        .rows(TraceFilter {
            kind: Some(TraceKind::ModelCall),
            trace_id: Some(lesson.id.clone()),
            ..Default::default()
        })
        .await?;
    let model: Vec<f64> = rows.iter().map(|r| r.duration_ms).collect();
    m.lesson_model_ms = (!model.is_empty()).then(|| model.iter().sum::<f64>());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::golden::evals_dir;
    use margin_shared::context::TraceUsage;

    /// `budgets.toml` parses and holds the D8 table.
    #[test]
    fn budgets_parse() {
        let text = std::fs::read_to_string(evals_dir().join(BUDGETS_FILE)).unwrap();
        let t: toml::Table = toml::from_str(&text).unwrap();
        for key in [
            "ingest",
            "retrieval",
            "context_search",
            "agent_step",
            "cache",
            "title",
            "memory",
            "folder_sync",
        ] {
            assert!(t.contains_key(key), "no [{key}]");
        }
        let b = Budgets::load(&evals_dir().join(BUDGETS_FILE)).unwrap();
        assert_eq!(b.retrieval.p95_max_ms, 500.0);
        assert_eq!(b.agent_step.overhead_max_ms, 150.0);
        assert_eq!(b.memory.idle_max_bytes, 3 * 1024 * 1024 * 1024);
        assert_eq!(b.title.max_tokens, 300);
    }

    fn row(kind: TraceKind, step: Option<u32>) -> TraceRow {
        let mut r = TraceRow::new(kind, format!("u{step:?}"), Utc::now());
        r.step = step;
        r
    }

    #[test]
    fn turn_numbers_from_rows() {
        let mut s1 = row(TraceKind::ModelCall, Some(1));
        s1.route = Some(Route::Chat);
        s1.duration_ms = 1_000.0;
        s1.provider_ms = Some(950.0);
        s1.usage = Some(TraceUsage {
            input: 1_000,
            cached: 0,
            ..Default::default()
        });
        let mut s2 = s1.clone();
        s2.step = Some(2);
        s2.provider_ms = Some(850.0);
        s2.usage = Some(TraceUsage {
            input: 2_000,
            cached: 1_500,
            ..Default::default()
        });
        let mut title = row(TraceKind::ModelCall, None);
        title.route = Some(Route::Small);
        title.prompt_version = Some("title-0011aabb".into());
        title.usage = Some(TraceUsage {
            input: 180,
            output: 12,
            ..Default::default()
        });
        let tool = row(TraceKind::ToolCall, None);
        let (o, cached, t) = turn_numbers(&[s1.clone(), tool, s2, title]);
        let o = o.unwrap();
        assert_eq!(o.steps, 2);
        assert_eq!(o.mean_ms, 100.0);
        assert_eq!(cached, Some(0.75));
        assert_eq!(t, Some(192));
        // One step: no share from step 2; no title row.
        assert_eq!(turn_numbers(&[s1]).1, None);
        assert_eq!(turn_numbers(&[]), (None, None, None));
    }

    #[test]
    fn every_budget_gets_a_line() {
        let b = Budgets::load(&evals_dir().join(BUDGETS_FILE)).unwrap();
        let m = Measured {
            docs: 7,
            chunks_new: 70,
            chunks_per_s: Some(3.5),
            unchanged_reingest_ms: Some(1_200.0),
            retrievals: 40,
            retrieval_p50_ms: Some(120.0),
            retrieval_p95_ms: Some(480.0),
            searches: 40,
            search_p95_ms: Some(650.0),
            step_overhead: Some(Overhead {
                steps: 3,
                mean_ms: 90.0,
                p95_ms: 140.0,
                max_ms: 140.0,
            }),
            cached_share: Some(0.5),
            title_tokens: Some(192),
            rss_idle: Some(2 * 1024 * 1024 * 1024),
            rss_peak: None,
            lesson_ms: Some(42_000.0),
            lesson_model_ms: Some(40_000.0),
            notes: vec!["the turn took one step".into()],
            ..Default::default()
        };
        let lines = verdicts(&b, &m);
        let v: Vec<Verdict> = lines.iter().map(|l| l.verdict).collect();
        use Verdict::*;
        assert_eq!(
            v,
            [
                Met,
                Missed,
                Met,
                Met,
                Met,
                Met,
                Missed,
                Met,
                Met,
                NotMeasured,
                Manual,
                Manual
            ]
        );
        let out = render(&b, &m);
        for want in [
            "| Ingest, text (chunks/s, end to end) | ≥ 3 | 3.50 (70 chunks, 7 documents) | met |",
            "| Unchanged re-ingest | ≤ 1000 ms | 1200 ms | MISSED |",
            "| Agent step overhead (mean) | ≤ 150 ms | 90 ms over 3 calls (p95 140, max 140) | met |",
            "| Cached input share from step 2 | ≥ 60 % | 50.0 % | MISSED |",
            "| Service RSS, text embedder loaded | ≤ 3.00 GiB | 2.00 GiB | met |",
            "| Service RSS, most seen in the run | ≤ 6.00 GiB | – | not measured |",
            "7 met, 2 missed",
            "Lesson: 42.0 s end to end, 40.0 s in model calls.",
            "Note: the turn took one step",
        ] {
            assert!(out.contains(want), "{want:?} not in\n{out}");
        }
    }
}
