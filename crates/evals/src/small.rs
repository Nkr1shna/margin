//! The model-backed stages in the eval (R2, `docs/r2-brief.md` §4 R2-evals, §6): the
//! listwise rerank (`"listwise-small"`) and the query rewriter (`"small-expand-v1"`)
//! call the `Small` route, so CI can't run them. `margin-eval record-reranks` and
//! `record-rewrites` run them once with the owner's key and keep what each call
//! returned in `evals/recorded/{reranks,rewrites}.jsonl`, keyed by
//! `sha256(stage + params, prompt version, purpose, query [, candidate ids])`; CI
//! replays them through [`SmallReranker`]/[`SmallRewriter`] registered under the real
//! names. A search that wasn't recorded is an error naming it ("re-run `margin-eval
//! record-reranks`"), never a made-up answer, as `RecordedVectors` does for vectors.
//!
//! The same two stages wrap the real ones when the runner has a key (`Env.models`):
//! served from the recording where it has the answer, called live (and recorded)
//! otherwise. [`Meter`] carries what each search's model calls cost: from the trace
//! rows the real stages write (`StageDeps.tracer`) when live, from the recording when
//! replayed.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Context;
use margin_service::engines::BoxFuture;
use margin_service::pipeline::{
    Hit, Pipeline, Registry, RerankRequest, Reranker, RewriteRequest, Rewriter, StageCtor,
};
use margin_service::retrieval::QueryPlan;
use margin_service::trace::Tracer;
use margin_shared::context::{TraceKind, TraceRow};
use margin_shared::pipeline::{PipelineConfig, RetrievalPurpose, Route, StageCfg};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::index::sha256_hex;

/// `evals/recorded/rewrites.jsonl`.
pub const REWRITES_FILE: &str = "recorded/rewrites.jsonl";
/// `evals/recorded/reranks.jsonl`.
pub const RERANKS_FILE: &str = "recorded/reranks.jsonl";

/// The rerankers that run on the `Small` route, so are recorded and replayed. The
/// cross-encoder (`"onnx-crossencoder"`) runs locally and is never recorded.
pub const SMALL_RERANKERS: [&str; 1] = ["listwise-small"];
/// The rewriters that run on the `Small` route.
pub const SMALL_REWRITERS: [&str; 1] = ["small-expand-v1"];

/// Which stage a recording is of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmallKind {
    Rewrites,
    Reranks,
}

impl SmallKind {
    pub fn file(self) -> &'static str {
        match self {
            SmallKind::Rewrites => REWRITES_FILE,
            SmallKind::Reranks => RERANKS_FILE,
        }
    }
    /// The stage kind in `PipelineConfig` ("rewriter", "reranker").
    pub fn stage(self) -> &'static str {
        match self {
            SmallKind::Rewrites => "rewriter",
            SmallKind::Reranks => "reranker",
        }
    }
    /// The command that records it.
    pub fn command(self) -> &'static str {
        match self {
            SmallKind::Rewrites => "record-rewrites",
            SmallKind::Reranks => "record-reranks",
        }
    }
    /// The stage names of this kind that run on the `Small` route.
    pub fn names(self) -> &'static [&'static str] {
        match self {
            SmallKind::Rewrites => &SMALL_REWRITERS,
            SmallKind::Reranks => &SMALL_RERANKERS,
        }
    }
    fn prompt_version(self) -> &'static str {
        match self {
            SmallKind::Rewrites => margin_ai::prompts::REWRITE_VERSION,
            SmallKind::Reranks => margin_ai::prompts::RERANK_VERSION,
        }
    }
    /// The stage `cfg` configures for this kind.
    pub fn cfg(self, cfg: &PipelineConfig) -> &StageCfg {
        match self {
            SmallKind::Rewrites => &cfg.rewriter,
            SmallKind::Reranks => &cfg.reranker,
        }
    }
    /// Whether `cfg`'s stage of this kind runs on the `Small` route.
    pub fn is_small(self, cfg: &PipelineConfig) -> bool {
        self.names().contains(&self.cfg(cfg).name.as_str())
    }
}

/// Whether a variant's stage applies to `purpose`: its `params.apply_to` lists it, or
/// it has no `apply_to` (it applies to everything).
pub fn stage_applies(cfg: &StageCfg, purpose: RetrievalPurpose) -> bool {
    match cfg.params.get("apply_to").and_then(|v| v.as_array()) {
        Some(list) => list.iter().any(|v| v.as_str() == Some(purpose.as_str())),
        None => true,
    }
}

/// A stage's identity in a key: its name and its parameters (canonical JSON).
fn stage_id(cfg: &StageCfg) -> String {
    format!(
        "{}\n{}",
        cfg.name,
        serde_json::to_string(&cfg.params).unwrap_or_default()
    )
}

/// The key of one rerank: the stage and its parameters, the rerank prompt's version,
/// the purpose, the reranker's query and the candidates' ids in their fused order.
pub fn rerank_key(
    cfg: &StageCfg,
    purpose: RetrievalPurpose,
    query: &str,
    candidates: &[String],
) -> String {
    sha256_hex(&format!(
        "rerank\n{}\n{}\n{}\n{query}\n{}",
        stage_id(cfg),
        margin_ai::prompts::RERANK_VERSION,
        purpose.as_str(),
        candidates.join("\n")
    ))
}

/// The key of one rewrite: the stage and its parameters, the rewrite prompt's version,
/// the purpose, the plan's strings and the scope's document names (all the prompt sees).
pub fn rewrite_key(
    cfg: &StageCfg,
    purpose: RetrievalPurpose,
    plan: &[String],
    scope_names: &[String],
) -> String {
    sha256_hex(&format!(
        "rewrite\n{}\n{}\n{}\n{}\n--\n{}",
        stage_id(cfg),
        margin_ai::prompts::REWRITE_VERSION,
        purpose.as_str(),
        plan.join("\n"),
        scope_names.join("\n")
    ))
}

/// One hit as a reranker returned it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RankedId {
    pub id: String,
    #[serde(default)]
    pub rerank: Option<f32>,
    pub score: f32,
}

/// What the model calls of one recorded stage call cost and who answered.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CallCost {
    /// Wall time of the stage call when it was recorded.
    pub ms: f64,
    pub cost_usd: f64,
    /// Small-route model calls it made (0 when the stage didn't apply).
    pub calls: u32,
    /// The prompt versions of those calls ("rerank-v1").
    pub prompt_versions: Vec<String>,
    /// "openai", "claude", … (trace rows' names).
    pub providers: Vec<String>,
    pub models: Vec<String>,
}

/// One recorded rerank.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RerankRecord {
    pub key: String,
    pub stage: String,
    #[serde(default)]
    pub params: serde_json::Map<String, serde_json::Value>,
    pub prompt_version: String,
    pub purpose: RetrievalPurpose,
    pub query: String,
    /// The fused candidates' ids, in order.
    pub candidates: Vec<String>,
    /// What the reranker returned, in order (every hit it returned, seen or not).
    pub ranked: Vec<RankedId>,
    /// The stage's error when it failed (the fused order was kept).
    #[serde(default)]
    pub error: Option<String>,
    #[serde(flatten)]
    pub cost: CallCost,
}

impl RerankRecord {
    /// Whether the reranker reordered anything (set `rerank` on a hit).
    pub fn reranked(&self) -> bool {
        self.ranked.iter().any(|r| r.rerank.is_some())
    }
}

/// One recorded rewrite.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RewriteRecord {
    pub key: String,
    pub stage: String,
    #[serde(default)]
    pub params: serde_json::Map<String, serde_json::Value>,
    pub prompt_version: String,
    pub purpose: RetrievalPurpose,
    /// The plan as given.
    pub plan: Vec<String>,
    pub scope_names: Vec<String>,
    /// The plan the rewriter returned (the given strings first).
    pub queries: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(flatten)]
    pub cost: CallCost,
}

/// A record keyed in a recording file.
pub trait Keyed: Serialize + DeserializeOwned + Clone + Send + 'static {
    fn key(&self) -> &str;
    fn stage(&self) -> (&str, &serde_json::Map<String, serde_json::Value>);
    fn cost(&self) -> &CallCost;
}

impl Keyed for RerankRecord {
    fn key(&self) -> &str {
        &self.key
    }
    fn stage(&self) -> (&str, &serde_json::Map<String, serde_json::Value>) {
        (&self.stage, &self.params)
    }
    fn cost(&self) -> &CallCost {
        &self.cost
    }
}

impl Keyed for RewriteRecord {
    fn key(&self) -> &str {
        &self.key
    }
    fn stage(&self) -> (&str, &serde_json::Map<String, serde_json::Value>) {
        (&self.stage, &self.params)
    }
    fn cost(&self) -> &CallCost {
        &self.cost
    }
}

/// A recording file's records by key.
#[derive(Debug, Clone)]
pub struct Records<T> {
    pub by_key: BTreeMap<String, T>,
}

impl<T> Default for Records<T> {
    fn default() -> Self {
        Self {
            by_key: BTreeMap::new(),
        }
    }
}

impl<T: Keyed> Records<T> {
    /// The records in `path`; `None` when there is no such file.
    pub fn read(path: &Path) -> anyhow::Result<Option<Self>> {
        if !path.is_file() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("couldn't read {}", path.display()))?;
        let mut by_key = BTreeMap::new();
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let r: T = serde_json::from_str(line)
                .with_context(|| format!("{} line {}", path.display(), i + 1))?;
            by_key.insert(r.key().to_string(), r);
        }
        Ok(Some(Self { by_key }))
    }

    /// Writes one record per line, sorted by key (stable diffs).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut out = String::new();
        for r in self.by_key.values() {
            out.push_str(&serde_json::to_string(r)?);
            out.push('\n');
        }
        std::fs::write(path, out).with_context(|| format!("couldn't write {}", path.display()))
    }

    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }
}

/// A recording shared by the stages that read and extend it.
pub struct Recorded<T> {
    /// Where it was read from (`None`: there was no file).
    pub source: Option<PathBuf>,
    pub records: Mutex<Records<T>>,
    /// Keys served or recorded in this process.
    used: Mutex<HashSet<String>>,
    /// Stage calls made live (not served from the recording), and their cost.
    fresh: AtomicUsize,
    fresh_cost: Mutex<f64>,
}

impl<T: Keyed> Recorded<T> {
    pub fn new(source: Option<PathBuf>, records: Records<T>) -> Self {
        Self {
            source,
            records: Mutex::new(records),
            used: Mutex::new(HashSet::new()),
            fresh: AtomicUsize::new(0),
            fresh_cost: Mutex::new(0.0),
        }
    }

    /// The recording at the first of `paths` that exists, or an empty one.
    pub fn open(paths: &[PathBuf]) -> anyhow::Result<Self> {
        for p in paths {
            if let Some(r) = Records::read(p)? {
                return Ok(Self::new(Some(p.clone()), r));
            }
        }
        Ok(Self::new(None, Records::default()))
    }

    fn get(&self, key: &str) -> Option<T> {
        let r = self.records.lock().unwrap().by_key.get(key).cloned();
        if r.is_some() {
            self.used.lock().unwrap().insert(key.to_string());
        }
        r
    }

    fn put(&self, r: T) {
        self.used.lock().unwrap().insert(r.key().to_string());
        self.fresh.fetch_add(1, Ordering::SeqCst);
        *self.fresh_cost.lock().unwrap() += r.cost().cost_usd;
        self.records
            .lock()
            .unwrap()
            .by_key
            .insert(r.key().to_string(), r);
    }

    /// Stage calls made live so far.
    pub fn fresh(&self) -> usize {
        self.fresh.load(Ordering::SeqCst)
    }

    /// USD of the stage calls made live so far.
    pub fn fresh_cost(&self) -> f64 {
        *self.fresh_cost.lock().unwrap()
    }

    /// Keys served or recorded so far.
    pub fn used(&self) -> usize {
        self.used.lock().unwrap().len()
    }

    /// The recording to save after recording `cfg`'s stage: what it had, plus what was
    /// recorded, less the records of this same stage configuration this run never
    /// asked for (stale: their candidates or queries are gone). Other variants'
    /// records are kept.
    pub fn finish(&self, cfg: &StageCfg) -> Records<T> {
        let used = self.used.lock().unwrap();
        let mut records = self.records.lock().unwrap().clone();
        records.by_key.retain(|k, r| {
            let (name, params) = r.stage();
            used.contains(k) || name != cfg.name || params != &cfg.params
        });
        records
    }
}

/// The recordings a run reads and extends, and the models it may call.
pub struct SmallStages {
    pub rewrites: Arc<Recorded<RewriteRecord>>,
    pub reranks: Arc<Recorded<RerankRecord>>,
    /// The small-tasks models; `None` replays (CI).
    pub models: Option<Arc<dyn margin_service::pipeline::SmallModels>>,
}

impl SmallStages {
    /// Reads the recordings from the first of `dirs` that has each.
    pub fn open(
        dirs: &[PathBuf],
        models: Option<Arc<dyn margin_service::pipeline::SmallModels>>,
    ) -> anyhow::Result<Self> {
        let files = |kind: SmallKind| -> Vec<PathBuf> {
            dirs.iter().map(|d| d.join(kind.file())).collect()
        };
        Ok(Self {
            rewrites: Arc::new(Recorded::open(&files(SmallKind::Rewrites))?),
            reranks: Arc::new(Recorded::open(&files(SmallKind::Reranks))?),
            models,
        })
    }

    /// Registers the recorded (or recording) stages under `cfg`'s names, for the stages
    /// of `cfg` that run on the `Small` route. With models, `real` must be `cfg`'s
    /// pipeline built with them: its stages are wrapped (served from the recording where
    /// it has the answer, called and recorded otherwise). Without, the stages replay the
    /// recording; a stage with no recording file is left to the registry (whose real
    /// constructor fails without a model: "needs a small-tasks model").
    pub fn register(
        &self,
        reg: &mut Registry,
        cfg: &PipelineConfig,
        meter: &Arc<Meter>,
        real: Option<&Pipeline>,
    ) {
        let live = self.models.is_some();
        if let Some(name) = SMALL_RERANKERS
            .into_iter()
            .find(|n| *n == cfg.reranker.name)
            && (live || self.reranks.source.is_some())
        {
            let stage = Arc::new(SmallReranker {
                name,
                cfg: cfg.reranker.clone(),
                recorded: self.reranks.clone(),
                inner: real.map(|p| p.reranker.clone()),
                meter: meter.clone(),
            });
            reg.register(
                name,
                StageCtor::Reranker(Arc::new(move |_, _| Ok(stage.clone()))),
            );
        }
        if let Some(name) = SMALL_REWRITERS
            .into_iter()
            .find(|n| *n == cfg.rewriter.name)
            && (live || self.rewrites.source.is_some())
        {
            let stage = Arc::new(SmallRewriter {
                name,
                cfg: cfg.rewriter.clone(),
                recorded: self.rewrites.clone(),
                inner: real.map(|p| p.rewriter.clone()),
                meter: meter.clone(),
            });
            reg.register(
                name,
                StageCtor::Rewriter(Arc::new(move |_, _| Ok(stage.clone()))),
            );
        }
    }

    /// Writes the recording of `kind` to `dir/<file>` (what `record-*` saves).
    pub fn save(&self, kind: SmallKind, cfg: &PipelineConfig, dir: &Path) -> anyhow::Result<usize> {
        let path = dir.join(kind.file());
        let stage = kind.cfg(cfg);
        let n = match kind {
            SmallKind::Rewrites => {
                let r = self.rewrites.finish(stage);
                r.save(&path)?;
                r.len()
            }
            SmallKind::Reranks => {
                let r = self.reranks.finish(stage);
                r.save(&path)?;
                r.len()
            }
        };
        Ok(n)
    }
}

/// What one search's model-backed stages spent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Spent {
    pub cost_usd: f64,
    pub calls: u32,
    /// The rerank's time as recorded, when the reranker was served from a recording.
    pub rerank_ms: Option<f64>,
    /// The rewrite's time as recorded, likewise.
    pub rewrite_ms: Option<f64>,
}

/// Collects what model-backed stages spend: the `model_call` rows of the tracer they
/// write to (live) or the costs a recording carries (replay).
pub struct Meter {
    tracer: Option<Arc<Tracer>>,
    rows: Mutex<Option<tokio::sync::mpsc::Receiver<TraceRow>>>,
    spent: Mutex<Spent>,
    versions: Mutex<BTreeSet<String>>,
    providers: Mutex<BTreeSet<String>>,
}

impl Default for Meter {
    fn default() -> Self {
        Self::new()
    }
}

impl Meter {
    /// No tracer: replays and stages that make no calls.
    pub fn new() -> Self {
        Self {
            tracer: None,
            rows: Mutex::new(None),
            spent: Mutex::new(Spent::default()),
            versions: Mutex::new(BTreeSet::new()),
            providers: Mutex::new(BTreeSet::new()),
        }
    }

    /// A tracer whose rows this meter reads, for stages that call models.
    pub fn traced() -> Self {
        let tracer = Arc::new(Tracer::new());
        let rows = tracer.take_receiver();
        Self {
            tracer: Some(tracer),
            rows: Mutex::new(rows),
            ..Self::new()
        }
    }

    /// The tracer to give stages (`StageDeps.tracer`).
    pub fn tracer(&self) -> Option<Arc<Tracer>> {
        self.tracer.clone()
    }

    /// Reads the small-route model-call rows written since the last time, adds them to
    /// this search's spend and returns them as a cost.
    pub fn collect(&self) -> CallCost {
        let mut cost = CallCost::default();
        let mut rows = self.rows.lock().unwrap();
        let Some(rx) = rows.as_mut() else {
            return cost;
        };
        while let Ok(row) = rx.try_recv() {
            if row.kind != TraceKind::ModelCall || row.route != Some(Route::Small) {
                continue;
            }
            cost.calls += 1;
            cost.cost_usd += row.cost_usd.unwrap_or(0.0);
            cost.prompt_versions.extend(row.prompt_version.clone());
            cost.providers
                .extend(row.provider.as_deref().map(crate::compare::provider_name));
            cost.models.extend(row.model.clone());
        }
        drop(rows);
        self.add(&cost);
        cost
    }

    /// Adds a recorded (or collected) cost to this search's spend.
    fn add(&self, c: &CallCost) {
        let mut s = self.spent.lock().unwrap();
        s.cost_usd += c.cost_usd;
        s.calls += c.calls;
        self.versions
            .lock()
            .unwrap()
            .extend(c.prompt_versions.iter().cloned());
        self.providers
            .lock()
            .unwrap()
            .extend(c.providers.iter().cloned());
    }

    fn replayed(&self, kind: SmallKind, c: &CallCost) {
        self.add(c);
        let mut s = self.spent.lock().unwrap();
        match kind {
            SmallKind::Reranks => s.rerank_ms = Some(c.ms),
            SmallKind::Rewrites => s.rewrite_ms = Some(c.ms),
        }
    }

    /// This search's spend so far, and a fresh start for the next.
    pub fn take(&self) -> Spent {
        self.collect();
        std::mem::take(&mut *self.spent.lock().unwrap())
    }

    /// Every prompt version the stages' calls used so far, sorted.
    pub fn prompt_versions(&self) -> Vec<String> {
        self.versions.lock().unwrap().iter().cloned().collect()
    }

    /// Every provider that answered so far, sorted.
    pub fn providers(&self) -> Vec<String> {
        self.providers.lock().unwrap().iter().cloned().collect()
    }
}

fn short(s: &str) -> String {
    let t: String = s.chars().take(80).collect();
    if t.len() < s.len() {
        format!("{t}…")
    } else {
        t
    }
}

/// The recorded (or recording) listwise reranker.
pub struct SmallReranker {
    name: &'static str,
    cfg: StageCfg,
    recorded: Arc<Recorded<RerankRecord>>,
    /// The real stage, called on a miss; `None` replays only.
    inner: Option<Arc<dyn Reranker>>,
    meter: Arc<Meter>,
}

fn apply(rec: &RerankRecord, hits: Vec<Hit>) -> anyhow::Result<Vec<Hit>> {
    let mut by_id: BTreeMap<String, Hit> = hits
        .into_iter()
        .map(|h| (h.passage.id.clone(), h))
        .collect();
    rec.ranked
        .iter()
        .map(|r| {
            let mut h = by_id.remove(&r.id).with_context(|| {
                format!(
                    "the recorded rerank of \"{}\" ranks {} which this search didn't find",
                    short(&rec.query),
                    r.id
                )
            })?;
            h.rerank = r.rerank;
            h.score = r.score;
            Ok(h)
        })
        .collect()
}

impl Reranker for SmallReranker {
    fn name(&self) -> &'static str {
        self.name
    }
    fn rerank<'a>(
        &'a self,
        req: &'a RerankRequest<'a>,
        hits: Vec<Hit>,
    ) -> BoxFuture<'a, anyhow::Result<Vec<Hit>>> {
        Box::pin(async move {
            let candidates: Vec<String> = hits.iter().map(|h| h.passage.id.clone()).collect();
            let key = rerank_key(&self.cfg, req.purpose, req.query, &candidates);
            if let Some(rec) = self.recorded.get(&key) {
                self.meter.replayed(SmallKind::Reranks, &rec.cost);
                return apply(&rec, hits);
            }
            let Some(inner) = &self.inner else {
                anyhow::bail!(
                    "no recorded rerank for \"{}\" ({}, {} candidates, reranker {}): the \
                     query, the candidates or the reranker changed since the recording; \
                     re-run `margin-eval record-reranks --variant <this variant>`",
                    short(req.query),
                    req.purpose.as_str(),
                    candidates.len(),
                    self.name
                );
            };
            self.meter.collect();
            let started = Instant::now();
            let (out, error) = match inner.rerank(req, hits.clone()).await {
                Ok(out) => (out, None),
                Err(e) => (hits, Some(format!("{e:#}"))),
            };
            let mut cost = self.meter.collect();
            cost.ms = started.elapsed().as_secs_f64() * 1e3;
            let rec = RerankRecord {
                key,
                stage: self.cfg.name.clone(),
                params: self.cfg.params.clone(),
                prompt_version: cost
                    .prompt_versions
                    .first()
                    .cloned()
                    .unwrap_or_else(|| SmallKind::Reranks.prompt_version().to_string()),
                purpose: req.purpose,
                query: req.query.to_string(),
                candidates,
                ranked: out
                    .iter()
                    .map(|h| RankedId {
                        id: h.passage.id.clone(),
                        rerank: h.rerank,
                        score: h.score,
                    })
                    .collect(),
                error,
                cost: cost.clone(),
            };
            self.meter.spent.lock().unwrap().rerank_ms = Some(cost.ms);
            self.recorded.put(rec);
            Ok(out)
        })
    }
}

/// The recorded (or recording) query rewriter.
pub struct SmallRewriter {
    name: &'static str,
    cfg: StageCfg,
    recorded: Arc<Recorded<RewriteRecord>>,
    inner: Option<Arc<dyn Rewriter>>,
    meter: Arc<Meter>,
}

impl Rewriter for SmallRewriter {
    fn name(&self) -> &'static str {
        self.name
    }
    fn rewrite<'a>(
        &'a self,
        req: &'a RewriteRequest<'a>,
        plan: QueryPlan,
    ) -> BoxFuture<'a, anyhow::Result<QueryPlan>> {
        Box::pin(async move {
            let key = rewrite_key(&self.cfg, req.purpose, &plan.queries, req.scope_names);
            if let Some(rec) = self.recorded.get(&key) {
                self.meter.replayed(SmallKind::Rewrites, &rec.cost);
                return Ok(QueryPlan {
                    queries: rec.queries,
                });
            }
            let Some(inner) = &self.inner else {
                anyhow::bail!(
                    "no recorded rewrite for \"{}\" ({}, rewriter {}): the query, the scope \
                     or the rewriter changed since the recording; re-run `margin-eval \
                     record-rewrites --variant <this variant>`",
                    short(&plan.queries.join(" | ")),
                    req.purpose.as_str(),
                    self.name
                );
            };
            self.meter.collect();
            let started = Instant::now();
            let (out, error) = match inner.rewrite(req, plan.clone()).await {
                Ok(out) if !out.queries.is_empty() => (out, None),
                Ok(_) => (plan.clone(), None),
                Err(e) => (plan.clone(), Some(format!("{e:#}"))),
            };
            let mut cost = self.meter.collect();
            cost.ms = started.elapsed().as_secs_f64() * 1e3;
            let rec = RewriteRecord {
                key,
                stage: self.cfg.name.clone(),
                params: self.cfg.params.clone(),
                prompt_version: cost
                    .prompt_versions
                    .first()
                    .cloned()
                    .unwrap_or_else(|| SmallKind::Rewrites.prompt_version().to_string()),
                purpose: req.purpose,
                plan: plan.queries,
                scope_names: req.scope_names.to_vec(),
                queries: out.queries.clone(),
                error,
                cost: cost.clone(),
            };
            self.meter.spent.lock().unwrap().rewrite_ms = Some(cost.ms);
            self.recorded.put(rec);
            Ok(out)
        })
    }
}

/// Every query string a rewrites recording holds (the plans and what was added), for
/// `record-embeddings` to embed.
pub fn recorded_rewrite_queries(dirs: &[PathBuf]) -> anyhow::Result<Vec<String>> {
    let rec: Recorded<RewriteRecord> = Recorded::open(
        &dirs
            .iter()
            .map(|d| d.join(REWRITES_FILE))
            .collect::<Vec<_>>(),
    )?;
    let records = rec.records.lock().unwrap();
    let mut out: Vec<String> = records
        .by_key
        .values()
        .flat_map(|r| r.plan.iter().chain(&r.queries).cloned())
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

/// The share (0–1) of the first `k` places whose passage differs from the fused
/// order's: 0 when the reranker kept the order, 1 when it changed every place.
pub fn moved_at_k(fused: &[String], reranked: &[Hit], k: usize) -> f64 {
    let n = reranked.len().min(k);
    if n == 0 {
        return 0.0;
    }
    let moved = (0..n)
        .filter(|i| fused.get(*i) != Some(&reranked[*i].passage.id))
        .count();
    moved as f64 / n as f64
}

#[cfg(test)]
pub(crate) mod testing {
    //! Fake "real" small stages for the harness's tests: registered under the real names,
    //! they need `StageDeps.models` as the real ones do, call the fake small model (so
    //! the meter sees a traced call) and behave predictably.

    use margin_ai::{AiError, CallOptions, InputPart, ModelWindow, TextModel, TextOutput};
    use margin_service::pipeline::{SmallModel, SmallModels};
    use margin_shared::{Provider, Usage};

    use super::*;

    /// A small model that answers "ok" and counts its calls.
    #[derive(Default)]
    pub struct CountingModel {
        pub calls: AtomicUsize,
    }

    impl TextModel for CountingModel {
        fn stream_parts<'a>(
            &'a self,
            _instructions: &'a str,
            _input: &'a [InputPart],
            _opts: &'a CallOptions,
            _on_delta: Box<dyn FnMut(&str) + Send + 'a>,
        ) -> margin_ai::model::BoxFuture<'a, Result<TextOutput, AiError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(TextOutput {
                    text: "ok".into(),
                    usage: Usage {
                        input_tokens: 2_000,
                        output_tokens: 100,
                        ..Default::default()
                    },
                    model: "claude-haiku-5-5".into(),
                    stop_reason: "end_turn".into(),
                })
            })
        }
    }

    pub struct FakeSmall {
        pub model: Arc<CountingModel>,
    }

    impl SmallModels for FakeSmall {
        fn small<'a>(
            &'a self,
            provider: Option<Provider>,
        ) -> BoxFuture<'a, anyhow::Result<SmallModel>> {
            Box::pin(async move {
                Ok(SmallModel {
                    model: self.model.clone(),
                    provider: provider.unwrap_or(Provider::Claude),
                    id: "claude-haiku-5-5".into(),
                    window: ModelWindow {
                        input: 200_000,
                        output: 32_000,
                    },
                })
            })
        }
    }

    /// Traces one small call the way the real stages do.
    async fn call(
        deps: &margin_service::pipeline::StageDeps,
        provider: Option<Provider>,
        version: &str,
    ) -> anyhow::Result<()> {
        let models = deps.models.as_ref().context("no small model")?;
        let m = models.small(provider).await?;
        let opts = CallOptions::new(Route::Small).with_prompt_version(version);
        let out = m
            .model
            .stream_parts("constant", &[], &opts, Box::new(|_| {}))
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        if let Some(t) = &deps.tracer {
            let mut row = TraceRow::new(TraceKind::ModelCall, String::new(), chrono::Utc::now());
            row.route = Some(Route::Small);
            row.prompt_version = Some(version.into());
            row.provider = Some(m.provider.as_str().into());
            row.model = Some(out.model.clone());
            row.usage = Some(margin_shared::context::TraceUsage {
                input: out.usage.input_tokens,
                output: out.usage.output_tokens,
                ..Default::default()
            });
            t.record(row);
        }
        Ok(())
    }

    /// Reverses the order of the first `max_candidates` (default 24) hits for purposes in
    /// `apply_to`, grading them 3, 2, 1 …; fails on a query containing "fail".
    pub struct FakeListwise {
        pub deps: margin_service::pipeline::StageDeps,
        pub cfg: StageCfg,
    }

    impl Reranker for FakeListwise {
        fn name(&self) -> &'static str {
            "listwise-small"
        }
        fn rerank<'a>(
            &'a self,
            req: &'a RerankRequest<'a>,
            mut hits: Vec<Hit>,
        ) -> BoxFuture<'a, anyhow::Result<Vec<Hit>>> {
            Box::pin(async move {
                if !stage_applies(&self.cfg, req.purpose) {
                    return Ok(hits);
                }
                call(&self.deps, req.provider, margin_ai::prompts::RERANK_VERSION).await?;
                anyhow::ensure!(!req.query.contains("fail"), "the model refused");
                let n = hits.len().min(3);
                hits[..n].reverse();
                for (i, h) in hits.iter_mut().take(n).enumerate() {
                    h.rerank = Some(3.0 - i as f32);
                    h.score = 3.0 - i as f32 + 0.5;
                }
                Ok(hits)
            })
        }
    }

    /// Adds "<first query> explained" to plans whose first string has fewer than 5 words.
    pub struct FakeExpand {
        pub deps: margin_service::pipeline::StageDeps,
    }

    impl Rewriter for FakeExpand {
        fn name(&self) -> &'static str {
            "small-expand-v1"
        }
        fn rewrite<'a>(
            &'a self,
            req: &'a RewriteRequest<'a>,
            plan: QueryPlan,
        ) -> BoxFuture<'a, anyhow::Result<QueryPlan>> {
            Box::pin(async move {
                let first = plan.queries.first().cloned().unwrap_or_default();
                if first.split_whitespace().count() >= 5 {
                    return Ok(plan);
                }
                call(
                    &self.deps,
                    req.provider,
                    margin_ai::prompts::REWRITE_VERSION,
                )
                .await?;
                Ok(QueryPlan::from_strings(
                    plan.queries
                        .iter()
                        .cloned()
                        .chain([format!("{first} explained")]),
                ))
            })
        }
    }

    /// Registers the fakes under the real names; like the real stages they fail to build
    /// without a small model.
    pub fn register_fake_small(reg: &mut Registry) {
        reg.register(
            "listwise-small",
            StageCtor::Reranker(Arc::new(|cfg, deps| {
                anyhow::ensure!(
                    deps.models.is_some(),
                    "the reranker \"listwise-small\" needs a small-tasks model; none is configured here"
                );
                Ok(Arc::new(FakeListwise {
                    deps: deps.clone(),
                    cfg: cfg.clone(),
                }))
            })),
        );
        reg.register(
            "small-expand-v1",
            StageCtor::Rewriter(Arc::new(|_, deps| {
                anyhow::ensure!(
                    deps.models.is_some(),
                    "the rewriter \"small-expand-v1\" needs a small-tasks model; none is configured here"
                );
                Ok(Arc::new(FakeExpand { deps: deps.clone() }))
            })),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use margin_shared::pipeline::StageCfg;

    fn cfg(name: &str, params: serde_json::Value) -> StageCfg {
        StageCfg {
            name: name.into(),
            params: params.as_object().cloned().unwrap_or_default(),
        }
    }

    #[test]
    fn keys_change_with_everything_the_answer_depends_on() {
        let a = cfg(
            "listwise-small",
            serde_json::json!({"apply_to": ["lesson"]}),
        );
        let ids = vec!["a".to_string(), "b".to_string()];
        let k = rerank_key(&a, RetrievalPurpose::Lesson, "q", &ids);
        assert_eq!(k, rerank_key(&a, RetrievalPurpose::Lesson, "q", &ids));
        assert_ne!(k, rerank_key(&a, RetrievalPurpose::Agent, "q", &ids));
        assert_ne!(k, rerank_key(&a, RetrievalPurpose::Lesson, "q2", &ids));
        let swapped = vec!["b".to_string(), "a".to_string()];
        assert_ne!(k, rerank_key(&a, RetrievalPurpose::Lesson, "q", &swapped));
        let b = cfg("listwise-small", serde_json::json!({"apply_to": ["agent"]}));
        assert_ne!(k, rerank_key(&b, RetrievalPurpose::Lesson, "q", &ids));
        let p = vec!["gibbs".to_string()];
        let w = cfg("small-expand-v1", serde_json::json!({}));
        let rk = rewrite_key(&w, RetrievalPurpose::Agent, &p, &[]);
        assert_ne!(
            rk,
            rewrite_key(&w, RetrievalPurpose::Agent, &p, &["a.md".into()])
        );
        assert_ne!(rk, rewrite_key(&w, RetrievalPurpose::Context, &p, &[]));
    }

    #[test]
    fn apply_to_gates_by_purpose() {
        let c = cfg("x", serde_json::json!({"apply_to": ["lesson", "plan"]}));
        assert!(stage_applies(&c, RetrievalPurpose::Lesson));
        assert!(!stage_applies(&c, RetrievalPurpose::Agent));
        assert!(stage_applies(
            &cfg("x", serde_json::json!({})),
            RetrievalPurpose::Agent
        ));
    }

    #[test]
    fn records_round_trip_and_finish_drops_only_this_stages_stale_ones() {
        let dir = std::env::temp_dir().join(format!(
            "margin-evals-small-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let rec = |key: &str, stage: &StageCfg| RewriteRecord {
            key: key.into(),
            stage: stage.name.clone(),
            params: stage.params.clone(),
            prompt_version: "rewrite-v1".into(),
            purpose: RetrievalPurpose::Agent,
            plan: vec!["q".into()],
            scope_names: vec![],
            queries: vec!["q".into(), "q2".into()],
            error: None,
            cost: CallCost {
                ms: 12.5,
                cost_usd: 0.001,
                calls: 1,
                ..Default::default()
            },
        };
        let mine = cfg("small-expand-v1", serde_json::json!({"min_words": 5}));
        let other = cfg("small-expand-v1", serde_json::json!({"min_words": 3}));
        let mut records = Records::default();
        for (k, s) in [("used", &mine), ("stale", &mine), ("theirs", &other)] {
            records.by_key.insert(k.to_string(), rec(k, s));
        }
        let path = dir.join(REWRITES_FILE);
        records.save(&path).unwrap();
        let back: Records<RewriteRecord> = Records::read(&path).unwrap().unwrap();
        assert_eq!(back.len(), 3);
        assert_eq!(back.by_key["used"], rec("used", &mine));
        assert!(
            Records::<RewriteRecord>::read(&dir.join("none.jsonl"))
                .unwrap()
                .is_none()
        );
        let shared: Recorded<RewriteRecord> =
            Recorded::open(&[dir.join("missing.jsonl"), path.clone()]).unwrap();
        assert_eq!(shared.source.as_deref(), Some(path.as_path()));
        assert!(shared.get("used").is_some());
        let kept = shared.finish(&mine);
        assert!(kept.by_key.contains_key("used"));
        assert!(kept.by_key.contains_key("theirs"), "another variant's");
        assert!(!kept.by_key.contains_key("stale"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn moved_counts_places_that_changed() {
        use crate::index::testing::hit;
        let fused: Vec<String> = ["a", "b", "c", "d"].map(String::from).to_vec();
        let same: Vec<Hit> = fused.iter().map(|id| hit(id)).collect();
        assert_eq!(moved_at_k(&fused, &same, 10), 0.0);
        let swapped: Vec<Hit> = ["b", "a", "c", "d"].iter().map(|id| hit(id)).collect();
        assert_eq!(moved_at_k(&fused, &swapped, 10), 0.5);
        assert_eq!(moved_at_k(&fused, &[], 10), 0.0);
    }
}
