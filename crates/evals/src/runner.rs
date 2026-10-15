//! The runner's commands (§3.4), with everything real passed in: the vectors, the
//! models, the judges, the budget resolver, the stage registry. The `margin-eval`
//! binary wires the real ones (`live.rs`); tests wire fakes, so every command is tested
//! without a model API, Hugging Face or the real embedding model.
//!
//! Reads come from `Paths::evals` (the checked-in `evals/`); writes go under
//! `Paths::out` (the same folder when run by hand, a temporary one in tests).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use chrono::Utc;
use margin_service::db::Store;
use margin_service::pipeline::{PackBudget, Pipeline, Registry, SmallModels, StageDeps};
use margin_shared::context::Scope;
use margin_shared::pipeline::{Budget, PipelineConfig, RetrievalPurpose};
use margin_shared::routes::RETRIEVAL_K_FUSED;

use crate::agent::{TaskResult, run_task};
use crate::compare::{Allow, Comparison, compare, providers_of};
use crate::corpus::{CorpusDoc, EXTRACTED_FILE, Extraction, corpus_hash, documents};
use crate::courses::{
    Generation, LessonScore, generate_lesson, generations_dir, load_generations, query_plan,
    save_generation, score,
};
use crate::gates::{BASELINE_FILE, Baseline};
use crate::golden::{AgentTask, CourseRequest, QueryKind, RetrievalQuery, read_jsonl};
use crate::index::{EvalIndex, scope_of, sha256_hex};
use crate::judge::{
    Agreement, CanaryReport, Judge, QuestionCase, Span, grader_agreement, judge_agreement,
    lesson_canaries, pass_agreement, question_canaries,
};
use crate::model::EvalModel;
use crate::recorded::{EMBEDDINGS_FILE, Recording, RecordingVectors, Vectors};
use crate::report::{ReportSection, Table, judging_section, questions_section, retrieval_sections};
use crate::retrieval::{RetrievalReport, kind_name};
use crate::small::{Meter, SmallKind, SmallStages, recorded_rewrite_queries};
use crate::store::{EvalResult, EvalRun, EvalStore};

#[derive(Debug, Clone)]
pub struct Paths {
    pub evals: PathBuf,
    pub out: PathBuf,
    /// The corpus when it isn't `evals/corpus` (the dogfood suite's `docs/`).
    pub corpus_dir: Option<PathBuf>,
    /// The retrieval questions when they aren't `golden/retrieval.jsonl`.
    pub retrieval_file: Option<PathBuf>,
}

impl Paths {
    pub fn same(evals: &Path) -> Self {
        Self {
            evals: evals.into(),
            out: evals.into(),
            corpus_dir: None,
            retrieval_file: None,
        }
    }
    /// The dogfood suite (`docs/plan-dogfood.md` §6): the repository's `docs/` as the
    /// corpus and the promoted `golden/dogfood.jsonl` as the questions.
    pub fn dogfood(&self, docs: &Path) -> Self {
        Self {
            corpus_dir: Some(docs.into()),
            retrieval_file: Some(self.evals.join(crate::harvest::DOGFOOD_FILE)),
            ..self.clone()
        }
    }
    pub fn corpus(&self) -> PathBuf {
        self.corpus_dir
            .clone()
            .unwrap_or_else(|| self.evals.join("corpus"))
    }
    pub fn retrieval_golden(&self) -> anyhow::Result<Vec<RetrievalQuery>> {
        if let Some(file) = &self.retrieval_file {
            anyhow::ensure!(
                file.is_file(),
                "no questions at {} yet: run `margin-eval harvest`, then promote the serious \
                 candidates from {} into it",
                file.display(),
                crate::harvest::CANDIDATES_FILE
            );
            return read_jsonl(file);
        }
        // Generated rows are appended under `out`; read them from there when present.
        let out = self.out.join("golden/retrieval.jsonl");
        let path = if out.is_file() {
            out
        } else {
            self.evals.join("golden/retrieval.jsonl")
        };
        read_jsonl(&path)
    }
    pub fn courses(&self) -> anyhow::Result<Vec<CourseRequest>> {
        read_jsonl(&self.evals.join("golden/courses.jsonl"))
    }
    pub fn agent_tasks(&self) -> anyhow::Result<Vec<AgentTask>> {
        read_jsonl(&self.evals.join("golden/agent.jsonl"))
    }
    /// `variants/<name>.toml`: the config and its text.
    pub fn variant(&self, name: &str) -> anyhow::Result<(PipelineConfig, String)> {
        let path = self.evals.join("variants").join(format!("{name}.toml"));
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("no variant \"{name}\" ({})", path.display()))?;
        Ok((
            toml::from_str(&text).with_context(|| format!("bad {}", path.display()))?,
            text,
        ))
    }
    pub fn variant_names(&self) -> anyhow::Result<Vec<String>> {
        let mut names: Vec<String> = std::fs::read_dir(self.evals.join("variants"))?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let p = e.path();
                if p.extension()? == "toml" {
                    Some(p.file_stem()?.to_string_lossy().into_owned())
                } else {
                    None
                }
            })
            .collect();
        names.sort();
        Ok(names)
    }
}

/// What a command needs to build pipelines and indexes.
pub struct Env<'a> {
    pub paths: Paths,
    pub vectors: &'a dyn Vectors,
    pub extraction: Extraction,
    /// Extra stage registrations (tests fake stages under their names).
    pub register: &'a (dyn Fn(&mut Registry) + Sync),
    /// `retrieval::pack_budget` in the runner.
    pub pack_budget: &'a (dyn Fn(&Budget, usize, usize) -> PackBudget + Sync),
    /// The small-tasks models the model-backed stages call (R2: the listwise rerank, the
    /// query rewriter): the runner's clients over the owner's keys (`live::small_models`).
    /// `None` in CI and tests: those stages replay `evals/recorded/{reranks,rewrites}.jsonl`.
    pub models: Option<Arc<dyn SmallModels>>,
}

impl<'a> Env<'a> {
    pub fn docs(&self) -> anyhow::Result<Vec<CorpusDoc>> {
        documents(&self.paths.corpus())
    }

    /// Where the small-stage recordings are read from: `out`, then `evals`.
    pub fn recording_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = vec![self.paths.out.clone()];
        if self.paths.evals != self.paths.out {
            dirs.push(self.paths.evals.clone());
        }
        dirs
    }

    /// The small-stage recordings and models for an index.
    pub fn small_stages(&self) -> anyhow::Result<SmallStages> {
        SmallStages::open(&self.recording_dirs(), self.models.clone())
    }

    /// `cfg`'s pipeline over the corpus. Its model-backed stages replay the recordings
    /// without `models`, and with them are served from the recordings where they have
    /// the answer and called live otherwise.
    pub async fn index(&self, cfg: &PipelineConfig) -> anyhow::Result<EvalIndex<'a>> {
        self.index_with(cfg, &self.small_stages()?).await
    }

    /// `index` with these recordings (what `record-*` extends and saves).
    pub async fn index_with(
        &self,
        cfg: &PipelineConfig,
        stages: &SmallStages,
    ) -> anyhow::Result<EvalIndex<'a>> {
        let store = Store::memory().await?;
        let live = stages.models.is_some();
        let meter = Arc::new(if live { Meter::traced() } else { Meter::new() });
        let deps = StageDeps {
            store: store.clone(),
            models: stages.models.clone(),
            tracer: meter.tracer(),
        };
        let mut reg = Registry::standard(deps);
        (self.register)(&mut reg);
        let small = SmallKind::Reranks.is_small(cfg) || SmallKind::Rewrites.is_small(cfg);
        // With models, the real stages are built to be wrapped by the recording ones.
        let real = if live && small {
            Some(Pipeline::build(cfg, &reg)?)
        } else {
            None
        };
        stages.register(&mut reg, cfg, &meter, real.as_ref());
        let pipeline = Pipeline::build(cfg, &reg)?;
        EvalIndex::build_with(
            store,
            pipeline,
            meter,
            self.vectors,
            &self.paths.corpus(),
            &self.docs()?,
            &self.extraction,
        )
        .await
    }
}

/// Sorted, each once.
fn sorted_versions(v: impl IntoIterator<Item = String>) -> Vec<String> {
    v.into_iter()
        .filter(|s| !s.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The folder a variant's recorded lessons go in under `recorded/generations/`: the
/// prompt version for the baseline (what the citation gate reads), `<version>+<variant>`
/// for any other, so a variant's lessons never overwrite the baseline's and `judge
/// --against recorded/generations/lesson-v2` compares them.
pub fn generations_set(prompt_version: &str, variant: &str) -> String {
    if variant == "baseline" {
        prompt_version.to_string()
    } else {
        format!("{prompt_version}+{variant}")
    }
}

/// `sha256(variant toml + corpus hash + embedder version)`.
pub fn run_key(variant_toml: &str, corpus_hash: &str, embedder: &str) -> String {
    sha256_hex(&format!("{variant_toml}\n{corpus_hash}\n{embedder}"))
}

fn new_run(
    suite: &str,
    variant: &str,
    cfg: &PipelineConfig,
    toml: &str,
    embedder: &str,
    corpus: &str,
) -> EvalRun {
    let now = Utc::now();
    EvalRun {
        id: format!("{suite}-{variant}-{}", now.format("%Y%m%dT%H%M%S%.3f")),
        suite: suite.into(),
        variant: variant.into(),
        pipeline_hash: cfg.hash(),
        embedder_version: embedder.into(),
        corpus_hash: corpus.into(),
        key: run_key(toml, corpus, embedder),
        started_at: now,
        finished_at: None,
        judges: Vec::new(),
        providers: Vec::new(),
        prompt_versions: Vec::new(),
        valid: true,
        notes: Vec::new(),
        summary: serde_json::Value::Null,
    }
}

fn result(
    run: &EvalRun,
    item: &str,
    group: &str,
    metric: &str,
    value: f64,
    judge: Option<String>,
) -> EvalResult {
    EvalResult {
        run_id: run.id.clone(),
        at: run.started_at,
        item: item.into(),
        group: group.into(),
        metric: metric.into(),
        value,
        judge,
    }
}

async fn finish(
    store: &EvalStore,
    mut run: EvalRun,
    sections: Vec<ReportSection>,
    extra: serde_json::Value,
) -> anyhow::Result<EvalRun> {
    run.finished_at = Some(Utc::now());
    run.summary = serde_json::json!({ "sections": sections, "data": extra });
    store.put_run(&run).await?;
    Ok(run)
}

/// What `record-embeddings` did.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordSummary {
    pub vectors: usize,
    pub embedded: usize,
    pub chunkers: Vec<String>,
}

/// `record-embeddings`: records the extraction, then every vector the CI gates and
/// every variant's chunker need (corpus chunks and pictures, golden queries, the
/// course lessons' query plans), embedding only what the existing recording lacks.
pub async fn record_embeddings(env: &Env<'_>) -> anyhow::Result<RecordSummary> {
    let out_rec = env.paths.out.join(EMBEDDINGS_FILE);
    let seed = match Recording::load(&out_rec) {
        Ok(r) if r.version == env.vectors.version() => r,
        _ => Recording::for_version(env.vectors.version()),
    };
    let recording = RecordingVectors::new(env.vectors, seed)?;
    let mut chunkers = BTreeSet::new();
    for name in env.paths.variant_names()? {
        let (cfg, _) = env.paths.variant(&name)?;
        if !chunkers.insert(serde_json::to_string(&cfg.chunker)?) {
            continue;
        }
        EvalIndex::build(
            Store::memory().await?,
            &cfg,
            |r| (env.register)(r),
            &recording,
            &env.paths.corpus(),
            &env.docs()?,
            &env.extraction,
        )
        .await?;
    }
    let mut queries: Vec<String> = env
        .paths
        .retrieval_golden()?
        .into_iter()
        .map(|q| q.query)
        .collect();
    for c in env.paths.courses()? {
        for i in 0..c.themes.len() {
            queries.extend(crate::courses::query_plan(&c, i));
        }
    }
    // Every string the recorded rewrites retrieve with (R2), so a rewriting variant
    // replays in CI.
    queries.extend(recorded_rewrite_queries(&env.recording_dirs())?);
    queries.sort();
    queries.dedup();
    recording.texts(&queries, &margin_embed::Purpose::Query)?;
    let embedded = recording.embedded();
    let rec = recording.finish();
    env.extraction.save(&env.paths.out.join(EXTRACTED_FILE))?;
    rec.save(&out_rec)?;
    Ok(RecordSummary {
        vectors: rec.len().0,
        embedded,
        chunkers: chunkers.into_iter().collect(),
    })
}

/// What `record-rewrites` or `record-reranks` did.
#[derive(Debug, Clone, PartialEq)]
pub struct SmallRecordSummary {
    pub file: PathBuf,
    /// Searches run: every golden query (the retrieval suite's purpose) and every golden
    /// lesson's plan (purpose lesson).
    pub searches: usize,
    /// Stage calls made live now (the rest were served from the recording).
    pub called: usize,
    /// Records in the file written.
    pub records: usize,
    /// Searches the reranker applies to that it left in the fused order (a timeout, a
    /// refusal, bad JSON): the CI gate fails on them, so re-record.
    pub fallbacks: usize,
    /// USD of the small-route calls made now.
    pub cost_usd: f64,
}

/// `record-rewrites` / `record-reranks` (R2, `docs/r2-brief.md` §6): runs `variant`'s
/// rewriter or reranker on the small-tasks model for every golden query (as the
/// retrieval suite searches it) and every golden lesson's plan (as the courses suite
/// does), and records each answer in `recorded/{rewrites,reranks}.jsonl` under `out`.
/// Answers already recorded are reused, not paid for again.
pub async fn record_small(
    env: &Env<'_>,
    variant: &str,
    kind: SmallKind,
) -> anyhow::Result<SmallRecordSummary> {
    let (cfg, _) = env.paths.variant(variant)?;
    anyhow::ensure!(
        kind.is_small(&cfg),
        "variant {variant}'s {} is \"{}\", which doesn't run on the small-tasks model; `{}` \
         records {}",
        kind.stage(),
        kind.cfg(&cfg).name,
        kind.command(),
        kind.names().join(", ")
    );
    anyhow::ensure!(
        env.models.is_some(),
        "`{}` calls the small-tasks model once per golden query and lesson plan: it needs a \
         model key in the service's secret store",
        kind.command()
    );
    let stages = env.small_stages()?;
    let index = env.index_with(&cfg, &stages).await?;
    let mut s = SmallRecordSummary {
        file: env.paths.out.join(kind.file()),
        searches: 0,
        called: 0,
        records: 0,
        fallbacks: 0,
        cost_usd: 0.0,
    };
    let mut count = |r: &crate::index::Ranked| {
        s.searches += 1;
        s.fallbacks += usize::from(r.stages.rerank_fallback);
    };
    let purpose = index.retrieval_purpose();
    for q in env.paths.retrieval_golden()? {
        let r = index
            .search_as(std::slice::from_ref(&q.query), &scope_of(q.scope), purpose)
            .await
            .map_err(|e| e.context(format!("query {}", q.id)))?;
        count(&r);
    }
    for c in env.paths.courses()? {
        let scope = Scope::Sources {
            ids: c.documents.clone(),
        };
        for i in 0..c.themes.len() {
            let r = index
                .search_as(&query_plan(&c, i), &scope, RetrievalPurpose::Lesson)
                .await
                .map_err(|e| e.context(format!("course {} lesson {i}", c.id)))?;
            count(&r);
        }
    }
    s.records = stages.save(kind, &cfg, &env.paths.out)?;
    (s.called, s.cost_usd) = match kind {
        SmallKind::Rewrites => (stages.rewrites.fresh(), stages.rewrites.fresh_cost()),
        SmallKind::Reranks => (stages.reranks.fresh(), stages.reranks.fresh_cost()),
    };
    Ok(s)
}

/// `run --suite retrieval`.
pub async fn run_retrieval(
    env: &Env<'_>,
    variant: &str,
    store: &EvalStore,
    write_baseline: bool,
) -> anyhow::Result<(EvalRun, RetrievalReport)> {
    run_retrieval_suite(env, "retrieval", variant, store, write_baseline).await
}

/// The dogfood suite (`docs/plan-dogfood.md` §6): the retrieval suite over `docs/` with
/// the questions an agent really asked (`env.paths` from `Paths::dogfood`). It has no
/// baseline of its own: CI never runs it.
pub async fn run_dogfood(
    env: &Env<'_>,
    variant: &str,
    store: &EvalStore,
) -> anyhow::Result<(EvalRun, RetrievalReport)> {
    anyhow::ensure!(
        env.paths.retrieval_file.is_some(),
        "the dogfood suite runs on Paths::dogfood"
    );
    run_retrieval_suite(env, "dogfood", variant, store, false).await
}

async fn run_retrieval_suite(
    env: &Env<'_>,
    suite: &str,
    variant: &str,
    store: &EvalStore,
    write_baseline: bool,
) -> anyhow::Result<(EvalRun, RetrievalReport)> {
    let (cfg, toml) = env.paths.variant(variant)?;
    let corpus = corpus_hash(&env.paths.corpus())?;
    let mut run = new_run(suite, variant, &cfg, &toml, env.vectors.version(), &corpus);
    let index = env.index(&cfg).await?;
    let queries = env.paths.retrieval_golden()?;
    let report = crate::retrieval::run(&index, &queries).await?;
    // The model-backed stages' calls (R2): their prompt versions and providers.
    run.prompt_versions = index.meter.prompt_versions();
    run.providers = index.meter.providers();
    let reranks = report.stages.reranker != "none";
    let mut rows = Vec::new();
    for q in &report.queries {
        let g = kind_name(q.kind);
        let mut put = |m: &str, v: Option<f64>| {
            if let Some(v) = v {
                rows.push(result(&run, &q.id, g, m, v, None));
            }
        };
        put("recall_at_5", q.recall_at_5);
        put("recall_at_10", q.recall_at_10);
        put("ndcg_at_10", q.ndcg_at_10);
        put(
            "reciprocal_rank",
            (q.n_relevant > 0).then_some(q.reciprocal_rank),
        );
        put(
            "negative_false_positive",
            (q.kind == QueryKind::Negative)
                .then(|| f64::from(u8::from(q.top_cosine >= report.fp_threshold))),
        );
        put("latency_ms", Some(q.ms));
        let s = &q.stages;
        put("queries", Some(s.queries as f64));
        put("rewrite_ms", Some(s.rewrite_ms));
        put("rerank_ms", Some(s.rerank_ms));
        put("small_cost_usd", Some(s.small_cost_usd));
        if reranks {
            put("rerank_moved_at_10", s.rerank_moved_at_10);
            put(
                "rerank_fallback",
                Some(f64::from(u8::from(s.rerank_fallback))),
            );
        }
    }
    store.add_results(&rows).await?;
    if write_baseline {
        Baseline::from_report(variant, &corpus, Utc::now(), &report)
            .save(&env.paths.out.join(BASELINE_FILE))?;
    }
    let run = finish(
        store,
        run,
        retrieval_sections(&report),
        serde_json::to_value(&report)?,
    )
    .await?;
    Ok((run, report))
}

/// Runs every judge's canaries; the run is invalid if any fails.
async fn canaries(judges: &[&dyn Judge], questions: bool) -> anyhow::Result<Vec<CanaryReport>> {
    let mut out = Vec::new();
    for j in judges {
        out.push(lesson_canaries(*j).await?);
        if questions {
            out.push(question_canaries(*j).await?);
        }
    }
    Ok(out)
}

fn agreements(scores: &[LessonScore], judges: &[&dyn Judge]) -> Vec<(String, Agreement)> {
    let mut out = Vec::new();
    for (i, j) in judges.iter().enumerate() {
        let js: Vec<_> = scores
            .iter()
            .filter_map(|s| s.judgements.get(i).cloned())
            .collect();
        if !js.is_empty() {
            out.push((format!("{} passage orders", j.id()), pass_agreement(&js)));
        }
    }
    if judges.len() >= 2 {
        let a: Vec<_> = scores
            .iter()
            .filter_map(|s| s.judgements.first().cloned())
            .collect();
        let b: Vec<_> = scores
            .iter()
            .filter_map(|s| s.judgements.get(1).cloned())
            .collect();
        out.push((
            format!("{} vs {}", judges[0].id(), judges[1].id()),
            judge_agreement(&a, &b),
        ));
    }
    out
}

fn lesson_table(scores: &[LessonScore]) -> Table {
    let mut means: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for s in scores {
        for (k, v) in s.metrics() {
            means.entry(k).or_default().push(v);
        }
    }
    Table {
        title: format!("Means over {} lessons", scores.len()),
        columns: vec!["Metric".into(), "Mean".into()],
        rows: means
            .into_iter()
            .map(|(k, v)| vec![k, format!("{:.3}", crate::metrics::mean(&v))])
            .collect(),
    }
}

/// `run --suite courses`: with a writer, generates and records every golden lesson;
/// without one, scores the recorded generations of the variant's prompt version.
/// Judged by every judge given.
pub async fn run_courses(
    env: &Env<'_>,
    variant: &str,
    writer: Option<&dyn EvalModel>,
    judges: &[&dyn Judge],
    window_tokens: usize,
    store: &EvalStore,
) -> anyhow::Result<(EvalRun, Vec<LessonScore>)> {
    let (cfg, toml) = env.paths.variant(variant)?;
    let corpus = corpus_hash(&env.paths.corpus())?;
    let mut run = new_run(
        "courses",
        variant,
        &cfg,
        &toml,
        env.vectors.version(),
        &corpus,
    );
    let set = generations_set(&cfg.prompts, variant);
    let mut stage_versions = Vec::new();
    let gens: Vec<Generation> = match writer {
        Some(w) => {
            let index = env.index(&cfg).await?;
            let budget = (env.pack_budget)(&cfg.budget, window_tokens, RETRIEVAL_K_FUSED);
            let dir = generations_dir(&env.paths.out, &set, &w.id());
            let mut gens = Vec::new();
            for c in env.paths.courses()? {
                for i in 0..c.themes.len() {
                    let g = generate_lesson(&index, w, &c, i, &budget).await?;
                    save_generation(&dir, &g)?;
                    gens.push(g);
                }
            }
            run.notes.push(format!(
                "lessons written by {} and recorded under {}",
                w.id(),
                dir.display()
            ));
            stage_versions = index.meter.prompt_versions();
            gens
        }
        None => {
            let dir = env
                .paths
                .out
                .join(crate::courses::GENERATIONS_DIR)
                .join(&set);
            let gens = load_generations(&dir)?;
            anyhow::ensure!(
                !gens.is_empty(),
                "no recorded generations under {} (run with a writer, or `record-generations`)",
                dir.display()
            );
            run.notes.push(format!(
                "scored the {} recorded generations under {}",
                gens.len(),
                dir.display()
            ));
            gens
        }
    };
    run.providers = providers_of(gens.iter().map(|g| g.model.as_str()));
    run.prompt_versions = sorted_versions(
        gens.iter()
            .map(|g| g.prompt_version.clone())
            .chain(stage_versions),
    );
    let canary_reports = canaries(judges, false).await?;
    run.judges = judges.iter().map(|j| j.id()).collect();
    run.valid = canary_reports.iter().all(CanaryReport::valid);
    let scores = score(&gens, judges).await?;
    let mut rows = Vec::new();
    for s in &scores {
        for (m, v) in s.metrics() {
            rows.push(result(&run, &s.id, &s.course, &m, v, None));
        }
    }
    store.add_results(&rows).await?;
    let agree = agreements(&scores, judges);
    let mut sections = vec![ReportSection {
        heading: "Courses".into(),
        text: vec![format!(
            "{} lessons of {} golden courses.",
            gens.len(),
            env.paths.courses()?.len()
        )],
        tables: vec![lesson_table(&scores)],
        bars: Vec::new(),
    }];
    sections.push(judging_section(&canary_reports, &agree));
    let run = finish(store, run, sections, serde_json::to_value(&scores)?).await?;
    Ok((run, scores))
}

/// `run --suite agent`.
#[allow(clippy::too_many_arguments)]
pub async fn run_agent<M: margin_agent::ModelClient + margin_ai::TextModel>(
    env: &Env<'_>,
    variant: &str,
    index: Arc<EvalIndex<'static>>,
    model: &M,
    model_id: &str,
    judges: &[&dyn Judge],
    retry: margin_agent::RetryPolicy,
    store: &EvalStore,
) -> anyhow::Result<(EvalRun, Vec<TaskResult>)> {
    let (cfg, toml) = env.paths.variant(variant)?;
    let corpus = corpus_hash(&env.paths.corpus())?;
    let mut run = new_run(
        "agent",
        variant,
        &cfg,
        &toml,
        index.vectors.version(),
        &corpus,
    );
    run.notes.push(format!("agent model {model_id}"));
    run.providers = vec![margin_ai::Provider::kind(model).as_str().to_string()];
    let mut results = Vec::new();
    let mut rows = Vec::new();
    let mut judged = Vec::new();
    for t in env.paths.agent_tasks()? {
        let r = run_task(model, index.clone(), &t, retry).await;
        let mut put = |m: &str, v: f64| rows.push(result(&run, &t.id, "agent", m, v, None));
        put("finished", f64::from(u8::from(r.finished)));
        put("steps", f64::from(r.steps));
        put("searches", r.searches as f64);
        put("citation_resolution", r.citations.resolution_pct() / 100.0);
        put(
            "citation_support",
            r.citations.lexical_support_pct() / 100.0,
        );
        put("must_cite", r.citations.must_cite_pct() / 100.0);
        put("anchors_cited", r.anchors_cited);
        put("tool_output_max_bytes", r.tool_output_max_bytes as f64);
        put("input_tokens", r.input_tokens as f64);
        put(
            "cached_share",
            if r.input_tokens > 0 {
                r.cached_tokens as f64 / r.input_tokens as f64
            } else {
                0.0
            },
        );
        if let Some(a) = r.abstained {
            put("abstained", f64::from(u8::from(a)));
        }
        if r.finished {
            let case = crate::judge::LessonCase {
                id: t.id.clone(),
                title: t.question.clone(),
                summary: t.question.clone(),
                level: margin_shared::Level::Intermediate,
                body: r.answer.clone(),
                passages: r.passages.clone(),
                must_cite: t.must_cite.clone(),
            };
            for j in judges {
                let jl = crate::judge::judge_lesson(*j, &case).await?;
                rows.push(result(
                    &run,
                    &t.id,
                    "agent",
                    "faithfulness",
                    jl.faithfulness_mean() / 3.0,
                    Some(j.id()),
                ));
                rows.push(result(
                    &run,
                    &t.id,
                    "agent",
                    "relevance_grade",
                    jl.grade(crate::judge::Criterion::Relevance) / 3.0,
                    Some(j.id()),
                ));
                judged.push(jl);
            }
        }
        results.push(r);
    }
    let canary_reports = canaries(judges, false).await?;
    run.judges = judges.iter().map(|j| j.id()).collect();
    run.valid = canary_reports.iter().all(CanaryReport::valid);
    run.prompt_versions = sorted_versions(
        std::iter::once(margin_agent::AGENT_PROMPT_VERSION.to_string())
            .chain(index.meter.prompt_versions()),
    );
    store.add_results(&rows).await?;
    let table = Table {
        title: "Agent tasks".into(),
        columns: [
            "Task",
            "Done",
            "Steps",
            "Searches",
            "Resolved %",
            "Must-cite %",
            "Anchors",
            "Max tool bytes",
        ]
        .map(String::from)
        .to_vec(),
        rows: results
            .iter()
            .map(|r| {
                vec![
                    r.id.clone(),
                    if r.finished {
                        "yes".into()
                    } else {
                        format!("no: {}", r.error.clone().unwrap_or_default())
                    },
                    r.steps.to_string(),
                    r.searches.to_string(),
                    format!("{:.0}", r.citations.resolution_pct()),
                    format!("{:.0}", r.citations.must_cite_pct()),
                    format!("{:.2}", r.anchors_cited),
                    r.tool_output_max_bytes.to_string(),
                ]
            })
            .collect(),
    };
    let agree = if judged.is_empty() {
        vec![]
    } else {
        vec![("passage orders".to_string(), pass_agreement(&judged))]
    };
    let sections = vec![
        ReportSection {
            heading: "Agent".into(),
            text: vec![],
            tables: vec![table],
            bars: vec![],
        },
        judging_section(&canary_reports, &agree),
    ];
    let run = finish(store, run, sections, serde_json::to_value(&results)?).await?;
    Ok((run, results))
}

/// `synth-questions`: generate, check, grade; kept rows are appended to the golden set
/// and every candidate to the log.
#[allow(clippy::too_many_arguments)]
pub async fn synth_questions(
    env: &Env<'_>,
    variant: &str,
    generator: &dyn Judge,
    grader: &dyn Judge,
    per_request: u32,
    limit: usize,
    store: &EvalStore,
) -> anyhow::Result<(EvalRun, crate::synth::SynthSummary)> {
    let (cfg, toml) = env.paths.variant(variant)?;
    let corpus = corpus_hash(&env.paths.corpus())?;
    let mut run = new_run(
        "synth",
        variant,
        &cfg,
        &toml,
        env.vectors.version(),
        &corpus,
    );
    let grader_canaries = question_canaries(grader).await?;
    run.judges = vec![generator.id(), grader.id()];
    run.providers = providers_of([generator.id().as_str()]);
    anyhow::ensure!(
        grader_canaries.valid(),
        "the grader {} failed its canaries ({}); no questions were generated",
        grader.id(),
        grader_canaries
            .failures()
            .iter()
            .map(|f| f.detail.clone())
            .collect::<Vec<_>>()
            .join("; ")
    );
    let index = env.index(&cfg).await?;
    let docs = env.docs()?;
    let sections = crate::synth::sections(&index, &docs, &env.extraction)?;
    let existing = env.paths.retrieval_golden()?;
    let (cands, rows, summary) = crate::synth::generate(
        generator,
        grader,
        &env.paths.corpus(),
        &docs,
        &sections,
        &existing,
        per_request,
        limit,
    )
    .await?;
    let golden_out = env.paths.out.join("golden/retrieval.jsonl");
    if !golden_out.is_file() {
        std::fs::create_dir_all(golden_out.parent().unwrap())?;
        std::fs::copy(env.paths.evals.join("golden/retrieval.jsonl"), &golden_out)?;
    }
    crate::synth::append_jsonl(&golden_out, &rows)?;
    crate::synth::append_jsonl(&env.paths.out.join(crate::synth::SYNTH_LOG), &cands)?;
    let mut results = Vec::new();
    for (i, c) in cands.iter().enumerate() {
        if let Some(g) = &c.grade {
            results.push(result(
                &run,
                &format!("c{i:04}"),
                &c.question.kind,
                "seriousness",
                f64::from(g.grade),
                Some(g.grader.clone()),
            ));
        }
    }
    store.add_results(&results).await?;
    let hand = existing.iter().filter(|q| q.generated_by == "hand").count();
    let sections = vec![
        questions_section(summary.grades, hand, None),
        judging_section(&[grader_canaries], &[]),
    ];
    let run = finish(store, run, sections, serde_json::to_value(&summary)?).await?;
    Ok((run, summary))
}

/// The golden questions that can be graded: those with anchors to quote.
pub fn gradable(queries: &[RetrievalQuery]) -> Vec<QuestionCase> {
    queries
        .iter()
        .filter(|q| q.relevant.iter().any(|r| r.anchor.is_some()))
        .map(|q| QuestionCase {
            id: q.id.clone(),
            question: q.query.clone(),
            answer: String::new(),
            spans: q
                .relevant
                .iter()
                .filter_map(|r| {
                    r.anchor.as_ref().map(|a| Span {
                        document: r.document.clone(),
                        section_path: String::new(),
                        page: r.page,
                        quote: a.clone(),
                    })
                })
                .collect(),
        })
        .collect()
}

/// `grade-questions`: every gradable golden question (hand-written and generated)
/// through every grader, with the graders' canaries and their agreement; `prune`
/// drops generated rows whose mean grade is now under 2.
pub async fn grade_questions(
    env: &Env<'_>,
    graders: &[&dyn Judge],
    prune: bool,
    store: &EvalStore,
) -> anyhow::Result<EvalRun> {
    anyhow::ensure!(!graders.is_empty(), "grade-questions needs a grader");
    let cfg = PipelineConfig::default();
    let corpus = corpus_hash(&env.paths.corpus())?;
    let mut run = new_run(
        "questions",
        "golden",
        &cfg,
        "",
        env.vectors.version(),
        &corpus,
    );
    let canary_reports = canaries(graders, true).await?;
    run.judges = graders.iter().map(|g| g.id()).collect();
    run.valid = canary_reports
        .iter()
        .filter(|c| c.set == "questions")
        .all(CanaryReport::valid);
    let queries = env.paths.retrieval_golden()?;
    let cases = gradable(&queries);
    let mut grades: Vec<Vec<u8>> = vec![Vec::new(); graders.len()];
    let mut rows = Vec::new();
    for c in &cases {
        for (i, g) in graders.iter().enumerate() {
            let s = g.seriousness(c).await?;
            grades[i].push(s.grade);
            rows.push(result(
                &run,
                &c.id,
                "seriousness",
                "seriousness",
                f64::from(s.grade),
                Some(s.grader),
            ));
        }
    }
    store.add_results(&rows).await?;
    let mean_of =
        |i: usize| grades.iter().map(|g| f64::from(g[i])).sum::<f64>() / graders.len() as f64;
    let mut hist_hand = [0usize; 4];
    let mut hist_gen = [0usize; 4];
    let mut drop: HashSet<String> = HashSet::new();
    for (i, c) in cases.iter().enumerate() {
        let m = mean_of(i);
        let hand = queries
            .iter()
            .any(|q| q.id == c.id && q.generated_by == "hand");
        let h = if hand { &mut hist_hand } else { &mut hist_gen };
        h[m.round().clamp(0.0, 3.0) as usize] += 1;
        if !hand && m < 2.0 {
            drop.insert(c.id.clone());
        }
    }
    let kappa = (graders.len() >= 2).then(|| grader_agreement(&grades[0], &grades[1]));
    if prune && !drop.is_empty() {
        let kept: Vec<&RetrievalQuery> = queries.iter().filter(|q| !drop.contains(&q.id)).collect();
        let path = env.paths.out.join("golden/retrieval.jsonl");
        std::fs::create_dir_all(path.parent().unwrap())?;
        let mut text = String::new();
        for q in kept {
            text.push_str(&serde_json::to_string(q)?);
            text.push('\n');
        }
        std::fs::write(&path, text)?;
        run.notes.push(format!(
            "pruned {} generated questions now graded under 2",
            drop.len()
        ));
    }
    let mut hand_sec = questions_section(hist_gen, hist_hand.iter().sum(), kappa);
    hand_sec.bars.push(crate::report::Bars {
        title: "Seriousness of the hand-written questions (mean grade)".into(),
        unit: "questions".into(),
        max: hist_hand.iter().copied().max().unwrap_or(1).max(1) as f64,
        bars: (0..4)
            .map(|g| (format!("grade {g}"), hist_hand[g] as f64))
            .collect(),
    });
    let sections = vec![hand_sec, judging_section(&canary_reports, &[])];
    finish(
        store,
        run,
        sections,
        serde_json::json!({ "hand": hist_hand, "generated": hist_gen, "kappa": kappa }),
    )
    .await
}

/// `judge`: the judges' canaries, then every recorded generation under `dir` judged;
/// with `against`, each lesson also compared pairwise with the same lesson recorded
/// there (blind to which variant wrote which, both orders).
pub async fn judge_generations(
    env: &Env<'_>,
    dir: &Path,
    against: Option<&Path>,
    judges: &[&dyn Judge],
    store: &EvalStore,
) -> anyhow::Result<EvalRun> {
    anyhow::ensure!(!judges.is_empty(), "judge needs a judge");
    let gens = load_generations(dir)?;
    anyhow::ensure!(
        !gens.is_empty(),
        "no recorded generations under {}",
        dir.display()
    );
    let cfg = PipelineConfig::default();
    let corpus = corpus_hash(&env.paths.corpus())?;
    let mut run = new_run(
        "judges",
        &dir.file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default(),
        &cfg,
        "",
        env.vectors.version(),
        &corpus,
    );
    run.providers = providers_of(gens.iter().map(|g| g.model.as_str()));
    let canary_reports = canaries(judges, true).await?;
    run.judges = judges.iter().map(|j| j.id()).collect();
    run.valid = canary_reports.iter().all(CanaryReport::valid);
    let scores = score(&gens, judges).await?;
    let mut rows = Vec::new();
    for s in &scores {
        for (m, v) in s.metrics() {
            rows.push(result(&run, &s.id, &s.course, &m, v, None));
        }
    }
    let mut pair_table = None;
    if let Some(other) = against {
        let theirs: BTreeMap<String, Generation> = load_generations(other)?
            .into_iter()
            .map(|g| (g.case().id, g))
            .collect();
        let mut wins = BTreeMap::<String, usize>::new();
        for g in &gens {
            let Some(o) = theirs.get(&g.case().id) else {
                continue;
            };
            for j in judges {
                let r = j
                    .pair(
                        crate::judge::Criterion::Faithfulness,
                        &g.case(),
                        &g.body,
                        &o.body,
                    )
                    .await?;
                let w = format!("{:?}", r.winner);
                *wins.entry(w.clone()).or_default() += 1;
                rows.push(result(
                    &run,
                    &g.case().id,
                    "pairwise",
                    "prefers_this",
                    match r.winner {
                        crate::judge::Winner::A => 1.0,
                        crate::judge::Winner::B => 0.0,
                        crate::judge::Winner::Tie => 0.5,
                    },
                    Some(j.id()),
                ));
            }
        }
        pair_table = Some(Table {
            title: format!(
                "Pairwise: {} (A) vs {} (B), faithfulness first",
                dir.display(),
                other.display()
            ),
            columns: vec!["Outcome".into(), "Count".into()],
            rows: wins
                .into_iter()
                .map(|(k, v)| vec![k, v.to_string()])
                .collect(),
        });
    }
    store.add_results(&rows).await?;
    let mut tables = vec![lesson_table(&scores)];
    tables.extend(pair_table);
    let sections = vec![
        ReportSection {
            heading: "Judged generations".into(),
            text: vec![format!("{} lessons from {}.", gens.len(), dir.display())],
            tables,
            bars: vec![],
        },
        judging_section(&canary_reports, &agreements(&scores, judges)),
    ];
    finish(store, run, sections, serde_json::to_value(&scores)?).await
}

/// `compare A B`.
pub async fn compare_runs(
    store: &EvalStore,
    a: &str,
    b: &str,
    allow: Allow,
) -> anyhow::Result<Comparison> {
    let ra = store.run(a).await?.with_context(|| format!("no run {a}"))?;
    let rb = store.run(b).await?.with_context(|| format!("no run {b}"))?;
    compare(
        &ra,
        &store.results(a).await?,
        &rb,
        &store.results(b).await?,
        allow,
    )
}

/// `report`: writes `reports/<run>.md` and `.html` under `out`.
pub async fn write_report(
    paths: &Paths,
    store: &EvalStore,
    run_id: Option<&str>,
    efficiency: Option<&crate::traces::Efficiency>,
) -> anyhow::Result<(PathBuf, PathBuf)> {
    let run = match run_id {
        Some(id) => store
            .run(id)
            .await?
            .with_context(|| format!("no run {id}"))?,
        None => store.latest(None).await?.context("no runs yet")?,
    };
    let mut sections: Vec<ReportSection> =
        serde_json::from_value(run.summary["sections"].clone()).unwrap_or_default();
    if let Some(e) = efficiency {
        sections.push(crate::report::efficiency_section(e));
    }
    let dir = paths.out.join("reports");
    std::fs::create_dir_all(&dir)?;
    let md = dir.join(format!("{}.md", run.id));
    let html = dir.join(format!("{}.html", run.id));
    std::fs::write(&md, crate::report::markdown(&run, &sections))?;
    std::fs::write(&html, crate::report::html(&run, &sections))?;
    Ok((md, html))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::golden::evals_dir;
    use crate::index::testing::register_stand_ins;
    use crate::judge::testing::RuleJudge;
    use crate::model::ScriptedModel;
    use crate::recorded::{FakeVectors, RecordedVectors};
    use margin_service::pipeline::{Diversity, Hit, PlanInput, Prompt, PromptSet, StageCtor};

    struct FakePrompts;
    impl PromptSet for FakePrompts {
        fn version(&self) -> &'static str {
            "lesson-v2"
        }
        fn lesson(&self, ctx: &margin_ai::LessonContext) -> Prompt {
            let mut v = format!("Write: {}\n", ctx.outline[ctx.index].title);
            for (i, p) in ctx.passages.iter().enumerate() {
                v.push_str(&format!("[^{}] {}\n", i + 1, p.text));
            }
            Prompt {
                version: "lesson-v2".into(),
                constant: "Rules.".into(),
                variable: v,
                cache_key: "lesson:lesson-v2".into(),
            }
        }
        fn plan(&self, _: &PlanInput) -> Prompt {
            unreachable!()
        }
        fn rerank(&self, _: &str, _: &[Hit]) -> Prompt {
            unreachable!()
        }
    }

    fn register(reg: &mut Registry) {
        register_stand_ins(reg);
        reg.register(
            "lesson-v2",
            StageCtor::Prompts(Arc::new(|_, _| Ok(Arc::new(FakePrompts)))),
        );
    }

    fn budget(b: &Budget, window: usize, k: usize) -> PackBudget {
        PackBudget {
            tokens: (b.share_of_window * window as f32) as usize,
            images_max: b.images_max as usize,
            min_score_pct: b.min_score_pct,
            passages_min: b.passages_min as usize,
            k: k.min(8),
            diversity: Diversity::default(),
        }
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "margin-evals-{name}-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    static FAKE: FakeVectors = FakeVectors;

    fn env<'a>(out: &Path, vectors: &'a dyn Vectors) -> Env<'a> {
        let paths = Paths {
            out: out.into(),
            ..Paths::same(&evals_dir())
        };
        let docs = documents(&paths.corpus()).unwrap();
        Env {
            extraction: Extraction::fixture(&paths.corpus(), &docs).unwrap(),
            paths,
            vectors,
            register: &register,
            pack_budget: &budget,
            models: None,
        }
    }

    #[tokio::test]
    async fn record_then_replay_gives_the_same_retrieval_numbers() {
        let out = temp("record");
        let e = env(&out, &FAKE);
        let s = record_embeddings(&e).await.unwrap();
        assert!(s.vectors > 100);
        assert_eq!(s.embedded, s.vectors, "everything new the first time");
        assert_eq!(s.chunkers.len(), 1, "all four variants share a chunker");
        // Recording again embeds nothing.
        assert_eq!(record_embeddings(&e).await.unwrap().embedded, 0);
        let store = EvalStore::memory().await.unwrap();
        let (_, live) = run_retrieval(&e, "baseline", &store, true).await.unwrap();
        let replay = RecordedVectors::open(&out.join(EMBEDDINGS_FILE)).unwrap();
        let mut e2 = env(&out, &replay);
        e2.extraction = Extraction::load(&out.join(EXTRACTED_FILE)).unwrap();
        let (run, rec) = run_retrieval(&e2, "baseline", &store, false).await.unwrap();
        assert_eq!(
            rec.overall, live.overall,
            "f16 vectors rank like the originals here"
        );
        assert_eq!(rec.embedder_version, "fake-bag-of-words");
        // The baseline written by the first run passes the gates against the replay.
        let gates: crate::Gates =
            toml::from_str(&std::fs::read_to_string(evals_dir().join("gates.toml")).unwrap())
                .unwrap();
        let base = Baseline::load(&out.join(BASELINE_FILE)).unwrap();
        let mut rec2 = rec.clone();
        rec2.latency_p95_ms = 0.0;
        assert!(crate::gates::check_retrieval(&gates, &base, &rec2).is_empty());
        // Stored results and a report.
        assert!(store.results(&run.id).await.unwrap().len() > 100);
        let (md, html) = write_report(&Paths::same(&out), &store, Some(&run.id), None)
            .await
            .unwrap();
        assert!(
            std::fs::read_to_string(md)
                .unwrap()
                .contains("Retrieval by query kind")
        );
        assert!(html.is_file());
        // Compare the two runs (same embedder: allowed).
        let runs = store.runs().await.unwrap();
        let c = compare_runs(&store, &runs[1].id, &runs[0].id, Allow::default())
            .await
            .unwrap();
        assert!(
            c.deltas
                .iter()
                .any(|d| d.metric == "recall_at_5" && d.interval.mean.abs() < 1e-9)
        );
        std::fs::remove_dir_all(&out).unwrap();
    }

    fn register_small(reg: &mut Registry) {
        register(reg);
        crate::small::testing::register_fake_small(reg);
    }

    /// R2: `record-reranks` and `record-rewrites` call the (fake) small model once per
    /// golden query and lesson plan and record the answers; `record-embeddings` embeds the
    /// recorded rewrites; a run without models replays them (never calling the model),
    /// with the stages' metrics, prompt versions and providers; the CI gate passes on the
    /// replay; a search that wasn't recorded fails loudly.
    #[tokio::test]
    async fn small_stages_record_then_replay() {
        use crate::small::testing::{CountingModel, FakeSmall};
        use std::sync::atomic::Ordering;
        let out = temp("small");
        let model = Arc::new(CountingModel::default());
        // As the runner records: the vectors served from the embeddings recording (f16,
        // what CI replays), so the candidates recorded are the ones replayed.
        let e0 = env(&out, &FAKE);
        record_embeddings(&e0).await.unwrap();
        let seeded =
            RecordingVectors::new(&FAKE, Recording::load(&out.join(EMBEDDINGS_FILE)).unwrap())
                .unwrap();
        let mut e = env(&out, &seeded);
        e.register = &register_small;
        e.models = Some(Arc::new(FakeSmall {
            model: model.clone(),
        }));
        let golden = e.paths.retrieval_golden().unwrap().len();
        let lessons: usize = e
            .paths
            .courses()
            .unwrap()
            .iter()
            .map(|c| c.themes.len())
            .sum();

        // Recording the reranks: every search; the rerank applies to lessons (the variant's
        // params.purpose for the golden queries, and the lesson plans).
        let s = record_small(&e, "rerank-small", SmallKind::Reranks)
            .await
            .unwrap();
        assert_eq!(s.searches, golden + lessons);
        // One call per distinct search (a repeated one is served from the recording).
        assert!(s.called > golden, "{s:?}");
        assert_eq!(s.records, s.called);
        assert_eq!(s.fallbacks, 0);
        assert!(s.cost_usd > 0.0, "priced from the trace rows");
        assert!(out.join(crate::small::RERANKS_FILE).is_file());
        let calls = model.calls.load(Ordering::SeqCst);
        assert_eq!(calls, s.called);
        // Again: served from the recording, nothing called.
        let again = record_small(&e, "rerank-small", SmallKind::Reranks)
            .await
            .unwrap();
        assert_eq!((again.called, again.records), (0, s.records));
        assert_eq!(model.calls.load(Ordering::SeqCst), calls);
        // A variant without the stage is refused.
        let err = record_small(&e, "baseline", SmallKind::Reranks)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("doesn't run on the small-tasks model"),
            "{err}"
        );

        // Recording the rewrites: only plans whose first string is short are rewritten.
        let s = record_small(&e, "rewrite-small", SmallKind::Rewrites)
            .await
            .unwrap();
        assert_eq!(s.records, s.searches);
        assert!(model.calls.load(Ordering::SeqCst) > calls);
        // record-embeddings embeds the recorded rewrites.
        record_embeddings(&e0).await.unwrap();
        let rec = Recording::load(&out.join(EMBEDDINGS_FILE)).unwrap();
        let rewrites = crate::small::recorded_rewrite_queries(std::slice::from_ref(&out)).unwrap();
        let added: Vec<&String> = rewrites
            .iter()
            .filter(|q| q.ends_with(" explained"))
            .collect();
        assert!(!added.is_empty());
        for q in added {
            assert!(
                rec.get_input(&crate::recorded::model_input(
                    &margin_embed::Purpose::Query,
                    q
                ))
                .is_some(),
                "{q} not embedded"
            );
        }

        // Replay: no models; the recorded stages stand in under the real names.
        let calls = model.calls.load(Ordering::SeqCst);
        let replay = crate::recorded::RecordedVectors::open(&out.join(EMBEDDINGS_FILE)).unwrap();
        let mut r = env(&out, &replay);
        r.extraction = Extraction::load(&out.join(EXTRACTED_FILE)).unwrap();
        r.register = &register_small;
        let store = EvalStore::memory().await.unwrap();
        let (run, rep) = run_retrieval(&r, "rerank-small", &store, false)
            .await
            .unwrap();
        assert_eq!(
            model.calls.load(Ordering::SeqCst),
            calls,
            "replayed, not called"
        );
        let st = &rep.stages;
        assert_eq!(
            (st.reranker.as_str(), st.purpose.as_str()),
            ("listwise-small", "lesson")
        );
        assert_eq!(st.reranked, golden);
        // The fake reverses the first three: places 1 and 3 change (of 10, or of fewer).
        assert!(
            st.rerank_moved_at_10 >= 20.0 - 1e-6 && st.rerank_moved_at_10 < 60.0,
            "{st:?}"
        );
        assert_eq!(st.rerank_fallback_rate, 0.0);
        assert!(st.rerank_ms_p95 >= st.rerank_ms_p50);
        assert!(st.small_cost_usd > 0.0, "the recording's cost");
        assert_eq!(st.small_calls as usize, golden);
        assert_eq!(run.prompt_versions, ["rerank-v1"]);
        assert_eq!(run.providers, ["claude"]);
        let rows = store.results(&run.id).await.unwrap();
        assert!(rows.iter().any(|x| x.metric == "rerank_moved_at_10"));
        let (md, _) = write_report(&Paths::same(&out), &store, Some(&run.id), None)
            .await
            .unwrap();
        let md = std::fs::read_to_string(md).unwrap();
        assert!(md.contains("## Reranking"), "{md}");
        assert!(md.contains("**Prompt versions:** rerank-v1"), "{md}");
        let (_, rw) = run_retrieval(&r, "rewrite-small", &store, false)
            .await
            .unwrap();
        assert!(rw.stages.queries_mean > 1.0, "{:?}", rw.stages);
        assert_eq!(model.calls.load(Ordering::SeqCst), calls);
        // The baseline compares with the variant: it adds a job, it doesn't change one.
        let (base, _) = run_retrieval(&r, "baseline", &store, false).await.unwrap();
        assert!(base.prompt_versions.is_empty());
        let c = compare_runs(&store, &base.id, &run.id, Allow::default())
            .await
            .unwrap();
        assert!(c.deltas.iter().any(|d| d.metric == "ndcg_at_10"));

        // The CI gate on the replay.
        let gates: crate::Gates =
            toml::from_str(&std::fs::read_to_string(evals_dir().join("gates.toml")).unwrap())
                .unwrap();
        for v in ["rerank-small", "rewrite-small"] {
            let o = crate::ci::run_small_gates(&r, &gates, v).await.unwrap();
            assert_eq!(o.violations, Vec::<String>::new(), "{v}");
        }
        let strict = crate::Gates {
            rerank: crate::RerankGates {
                moved_at_10_max: 10.0,
                ..gates.rerank
            },
            ..gates.clone()
        };
        let o = crate::ci::run_small_gates(&r, &strict, "rerank-small")
            .await
            .unwrap();
        assert!(
            o.violations[0].contains("is a bug, not a win"),
            "{:?}",
            o.violations
        );

        // A search that wasn't recorded (another variant's reranker settings) fails loudly.
        let err = run_retrieval(&r, "r2-full", &store, false)
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("re-run `margin-eval record-reranks"),
            "{err:#}"
        );
        // Without a recording or a model, the real stage won't build.
        let empty = temp("small-empty");
        let none = SmallStages::open(std::slice::from_ref(&empty), None).unwrap();
        let (cfg, _) = r.paths.variant("rerank-small").unwrap();
        let err = r.index_with(&cfg, &none).await.err().unwrap();
        assert!(
            format!("{err:#}").contains("needs a small-tasks model"),
            "{err:#}"
        );
        std::fs::remove_dir_all(&empty).unwrap();
        std::fs::remove_dir_all(&out).unwrap();
    }

    /// R2: a recorded rerank that failed (the stage kept the fused order) replays as a
    /// fallback, which the gate fails.
    #[tokio::test]
    async fn a_recorded_fallback_fails_the_gate() {
        use crate::small::testing::{CountingModel, FakeSmall};
        let out = temp("small-fallback");
        let mut e = env(&out, &FAKE);
        e.register = &register_small;
        e.models = Some(Arc::new(FakeSmall {
            model: Arc::new(CountingModel::default()),
        }));
        // Golden questions whose rerank the fake refuses.
        let mut golden = e.paths.retrieval_golden().unwrap();
        golden.truncate(3);
        golden[0].query = format!("{} fail", golden[0].query);
        let file = out.join("golden/retrieval.jsonl");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let mut text = String::new();
        for q in &golden {
            text.push_str(&serde_json::to_string(q).unwrap());
            text.push('\n');
        }
        std::fs::write(&file, text).unwrap();
        let s = record_small(&e, "rerank-small", SmallKind::Reranks)
            .await
            .unwrap();
        assert_eq!(s.fallbacks, 1);
        let records = crate::small::Records::<crate::small::RerankRecord>::read(
            &out.join(crate::small::RERANKS_FILE),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            records
                .by_key
                .values()
                .filter(|r| r.error.as_deref() == Some("the model refused"))
                .count(),
            1
        );
        e.models = None;
        let gates: crate::Gates =
            toml::from_str(&std::fs::read_to_string(evals_dir().join("gates.toml")).unwrap())
                .unwrap();
        let o = crate::ci::run_small_gates(&e, &gates, "rerank-small")
            .await
            .unwrap();
        assert!(
            o.violations
                .iter()
                .any(|v| v.contains("kept the fused order")),
            "{:?}",
            o.violations
        );
        std::fs::remove_dir_all(&out).unwrap();
    }

    /// The dogfood suite: the retrieval suite over a `docs/` folder with the promoted
    /// questions an agent asked, stored as its own suite.
    #[tokio::test]
    async fn the_dogfood_suite_runs_over_docs_with_the_promoted_questions() {
        let out = temp("dogfood");
        let docs = out.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(
            docs.join("plan-deploy.md"),
            "# Deploys\n\n## Prod\n\nProd is promoted by hand from main after dev passes.\n",
        )
        .unwrap();
        std::fs::write(
            docs.join("agent-contract.md"),
            "# Agent contract\n\n## Tools\n\nThe folder tools list and find files.\n",
        )
        .unwrap();
        let questions = out.join("dogfood.jsonl");
        // Without promoted questions it says what to do.
        let base = Paths {
            out: out.clone(),
            ..Paths::same(&evals_dir())
        };
        let missing = Paths {
            retrieval_file: Some(out.join("none.jsonl")),
            ..base.dogfood(&docs)
        };
        let err = missing.retrieval_golden().unwrap_err().to_string();
        assert!(err.contains("margin-eval harvest"), "{err}");
        std::fs::write(
            &questions,
            r#"{"id":"d1","query":"how does prod get deployed","scope":{"scope":"corpus"},"relevant":[{"document":"plan-deploy.md","anchor":"Prod is promoted by hand"}],"kind":"semantic","generated_by":"dogfood"}
"#,
        )
        .unwrap();
        let paths = Paths {
            retrieval_file: Some(questions),
            ..base.dogfood(&docs)
        };
        assert_eq!(paths.corpus(), docs);
        let d = documents(&paths.corpus()).unwrap();
        let e = Env {
            extraction: Extraction::fixture(&paths.corpus(), &d).unwrap(),
            paths,
            vectors: &FAKE,
            register: &register,
            pack_budget: &budget,
            models: None,
        };
        let store = EvalStore::memory().await.unwrap();
        let (run, report) = run_dogfood(&e, "baseline", &store).await.unwrap();
        assert_eq!(run.suite, "dogfood");
        assert_eq!(report.queries.len(), 1);
        assert_eq!(report.queries[0].recall_at_5, Some(1.0));
        // The ordinary suite won't take dogfood paths' place.
        assert!(
            run_dogfood(&env(&out, &FAKE), "baseline", &store)
                .await
                .is_err()
        );
        std::fs::remove_dir_all(&out).unwrap();
    }

    #[tokio::test]
    async fn courses_generate_record_score_and_judge() {
        let out = temp("courses");
        let e = env(&out, &FAKE);
        let store = EvalStore::memory().await.unwrap();
        let writer = ScriptedModel::new("anthropic", "writer", |req| {
            Ok(format!(
                "Why this matters.\n\n## Worked example\n\nAs the first passage says, {}.[^1]\n\n> **Key idea.** It holds.\n",
                req.user
                    .lines()
                    .nth(1)
                    .unwrap_or("")
                    .trim_start_matches("[^1] ")
                    .chars()
                    .take(80)
                    .collect::<String>()
            ))
        });
        let judge = RuleJudge::named("j");
        let (run, scores) = run_courses(&e, "baseline", Some(&writer), &[&judge], 200_000, &store)
            .await
            .unwrap();
        let lessons: usize = e
            .paths
            .courses()
            .unwrap()
            .iter()
            .map(|c| c.themes.len())
            .sum();
        assert_eq!(scores.len(), lessons);
        assert_eq!(writer.calls(), lessons);
        assert!(run.valid);
        assert!(scores.iter().all(|s| s.citations.resolution_pct() == 100.0));
        // Recorded, then scored again from the recordings without a writer.
        let (run2, again) = run_courses(&e, "baseline", None, &[], 200_000, &store)
            .await
            .unwrap();
        assert_eq!(again.len(), lessons);
        assert!(run2.notes[0].contains("recorded generations"));
        // Both runs record the writer's provider (from the recordings for the second),
        // so `compare` can refuse a provider change.
        assert_eq!(run.providers, ["claude"]);
        assert_eq!(run2.providers, ["claude"]);
        // Judging the recordings against themselves: every comparison is decided by
        // length, equal here, so all ties.
        let dir = out.join("recorded/generations/lesson-v2");
        let jr = judge_generations(&e, &dir, Some(&dir), &[&judge], &store)
            .await
            .unwrap();
        assert!(jr.valid);
        assert!(
            serde_json::to_string(&jr.summary)
                .unwrap()
                .contains("Pairwise")
        );
        std::fs::remove_dir_all(&out).unwrap();
    }

    #[tokio::test]
    async fn synth_and_grade_questions() {
        let out = temp("synth");
        let e = env(&out, &FAKE);
        let store = EvalStore::memory().await.unwrap();
        let writer = RuleJudge::named("writer");
        let grader = RuleJudge::named("grader");
        let (_, s) = synth_questions(&e, "baseline", &writer, &grader, 1, 8, &store)
            .await
            .unwrap();
        assert!(s.kept > 0);
        let golden: Vec<RetrievalQuery> = read_jsonl(&out.join("golden/retrieval.jsonl")).unwrap();
        assert_eq!(golden.len(), 60 + s.kept);
        assert!(out.join(crate::synth::SYNTH_LOG).is_file());
        // A lenient grader fails its canaries and stops generation.
        let lenient = RuleJudge {
            lenient: true,
            ..RuleJudge::named("lenient")
        };
        let err = synth_questions(&e, "baseline", &writer, &lenient, 1, 2, &store)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("failed its canaries"), "{err}");
        // Two graders: agreement, distribution, canaries.
        let other = RuleJudge::named("other");
        let run = grade_questions(&e, &[&grader, &other], true, &store)
            .await
            .unwrap();
        assert!(run.valid);
        assert_eq!(run.summary["data"]["kappa"], 1.0);
        let hand: u64 = run.summary["data"]["hand"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap())
            .sum();
        assert!(hand >= 40, "{hand}");
        assert!(
            gradable(&golden).len() < golden.len(),
            "negatives and anchorless pictures aren't graded"
        );
        std::fs::remove_dir_all(&out).unwrap();
    }

    #[tokio::test]
    async fn the_agent_suite_runs_on_a_scripted_model() {
        use margin_agent::scripted::{Scripted, ScriptedModel as Agent, text_reply};
        let out = temp("agent");
        let e = env(&out, &FAKE);
        let index = Arc::new(e.index(&PipelineConfig::default()).await.unwrap());
        let tasks = e.paths.agent_tasks().unwrap();
        let model = Agent::new(
            tasks
                .iter()
                .map(|_| Scripted::Events(text_reply(&["I can't find that in your sources."]))),
        );
        let store = EvalStore::memory().await.unwrap();
        let j = RuleJudge::named("j");
        let (run, results) = run_agent(
            &e,
            "baseline",
            index,
            &model,
            "gpt-test",
            &[&j],
            margin_agent::RetryPolicy::immediate(),
            &store,
        )
        .await
        .unwrap();
        assert_eq!(results.len(), tasks.len());
        assert!(results.iter().all(|r| r.finished));
        assert_eq!(run.providers, ["openai"], "the agent model's provider");
        assert_eq!(
            results.iter().filter(|r| r.abstained == Some(true)).count(),
            1
        );
        assert!(
            store
                .results(&run.id)
                .await
                .unwrap()
                .iter()
                .any(|r| r.metric == "faithfulness")
        );
        std::fs::remove_dir_all(&out).unwrap();
    }
}
