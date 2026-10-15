//! Rerankers (R2, `docs/r2-brief.md` D1, D2, D5): `"listwise-small"`, a listwise grade
//! of the fused candidates on the `Small` route for lessons and the planner, and
//! `"onnx-crossencoder"`, a local cross-encoder (`margin_embed::Reranker`) on the CPU.
//! Neither is the default (`PipelineConfig::default()` keeps `"none"`); the eval decides.
//!
//! Every failure (no model, a timeout, a refusal, bad JSON, a missing download at call
//! time) is an error the caller answers with the fused order unchanged; `retrieve`
//! traces it as `rerank_failed`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use margin_ai::prompts::{RERANK_CONSTANT, RERANK_VERSION};
use margin_ai::{CallOptions, Ranked};
use margin_shared::context::Document;
use margin_shared::pipeline::{RetrievalPurpose, Route, StageCfg};
use margin_shared::routes::{RERANK_MAX_CANDIDATES, RERANK_MAX_OUTPUT_TOKENS};
use serde_json::Value;

use crate::db;
use crate::engines::BoxFuture;
use crate::pipeline::{
    Hit, Registry, RerankRequest, Reranker, SmallCall, SmallModels, StageCtor, StageDeps,
    rerank_candidates, retrieval_ctx,
};

/// The stage names.
pub const LISTWISE_SMALL: &str = "listwise-small";
pub const ONNX_CROSSENCODER: &str = "onnx-crossencoder";

/// A cross-encoder's logits are min-max scaled into `[0, CROSS_ENCODER_SCALE)` so they
/// share the listwise grades' scale (0–3, plus the fused score in `[0, 1)`).
pub const CROSS_ENCODER_SCALE: f32 = 4.0;

/// `"listwise-small"`'s params (`StageCfg.params`).
#[derive(Debug, Clone, PartialEq)]
pub struct ListwiseParams {
    /// Purposes it reranks; others pass through untouched with no call. Default
    /// `["lesson", "plan"]`.
    pub apply_to: Vec<RetrievalPurpose>,
    /// Default `routes::RERANK_MAX_CANDIDATES`.
    pub max_candidates: usize,
    /// Drop grade-0 hits (never below `Budget.passages_min`). Default true.
    pub drop_grade_0: bool,
    /// Default 20 s.
    pub timeout: Duration,
}

/// `"onnx-crossencoder"`'s params.
#[derive(Debug, Clone, PartialEq)]
pub struct CrossEncoderParams {
    /// `margin_embed::RerankerModel::name`; default `ms-marco-minilm-l6-v2`.
    pub model: String,
    /// Default every purpose.
    pub apply_to: Vec<RetrievalPurpose>,
    pub max_candidates: usize,
    /// Pictures (hits whose modality isn't text) keep their place by fused score
    /// (`"keep"`, the default) so a text-only model can't demote every picture; with
    /// `"score"` the model scores their OCR text like any passage.
    pub keep_pictures: bool,
}

/// `params[key]` as a list of purposes; `default` when absent. An unknown purpose is an
/// error naming it.
pub(crate) fn purposes_param(
    params: &serde_json::Map<String, Value>,
    key: &str,
    default: &[RetrievalPurpose],
) -> anyhow::Result<Vec<RetrievalPurpose>> {
    let Some(v) = params.get(key) else {
        return Ok(default.to_vec());
    };
    let items = v
        .as_array()
        .with_context(|| format!("params.{key} must be a list of purposes"))?;
    items
        .iter()
        .map(|item| {
            let name = item.as_str().unwrap_or_default();
            RetrievalPurpose::ALL
                .into_iter()
                .find(|p| p.as_str() == name)
                .with_context(|| {
                    format!(
                        "params.{key} names \"{name}\", not a purpose (lesson, plan, agent, context)"
                    )
                })
        })
        .collect()
}

/// `params[key]` as a whole number; `default` when absent.
pub(crate) fn usize_param(
    params: &serde_json::Map<String, Value>,
    key: &str,
    default: usize,
) -> anyhow::Result<usize> {
    match params.get(key) {
        None => Ok(default),
        Some(v) => v
            .as_u64()
            .map(|n| n as usize)
            .with_context(|| format!("params.{key} must be a whole number")),
    }
}

/// `params[key]` in seconds (a number above 0); `default` when absent.
pub(crate) fn secs_param(
    params: &serde_json::Map<String, Value>,
    key: &str,
    default: Duration,
) -> anyhow::Result<Duration> {
    match params.get(key) {
        None => Ok(default),
        Some(v) => v
            .as_f64()
            .filter(|s| s.is_finite() && *s > 0.0)
            .map(Duration::from_secs_f64)
            .with_context(|| format!("params.{key} must be a number of seconds above 0")),
    }
}

fn bool_param(
    params: &serde_json::Map<String, Value>,
    key: &str,
    default: bool,
) -> anyhow::Result<bool> {
    match params.get(key) {
        None => Ok(default),
        Some(v) => v
            .as_bool()
            .with_context(|| format!("params.{key} must be true or false")),
    }
}

impl ListwiseParams {
    pub fn from_cfg(cfg: &StageCfg) -> anyhow::Result<Self> {
        let p = &cfg.params;
        let max_candidates = usize_param(p, "max_candidates", RERANK_MAX_CANDIDATES)?;
        anyhow::ensure!(
            max_candidates > 0,
            "params.max_candidates must be at least 1"
        );
        Ok(Self {
            apply_to: purposes_param(
                p,
                "apply_to",
                &[RetrievalPurpose::Lesson, RetrievalPurpose::Plan],
            )?,
            max_candidates,
            drop_grade_0: bool_param(p, "drop_grade_0", true)?,
            timeout: secs_param(p, "timeout_secs", Duration::from_secs(20))?,
        })
    }
}

impl CrossEncoderParams {
    pub fn from_cfg(cfg: &StageCfg) -> anyhow::Result<Self> {
        let p = &cfg.params;
        let model = match p.get("model") {
            None => margin_embed::RerankerModel::DEFAULT.name().to_string(),
            Some(v) => v
                .as_str()
                .context("params.model must be a reranker model's name")?
                .to_string(),
        };
        let max_candidates = usize_param(p, "max_candidates", RERANK_MAX_CANDIDATES)?;
        anyhow::ensure!(
            max_candidates > 0,
            "params.max_candidates must be at least 1"
        );
        let keep_pictures = match p.get("pictures").map(|v| v.as_str()) {
            None | Some(Some("keep")) => true,
            Some(Some("score")) => false,
            Some(_) => anyhow::bail!("params.pictures must be \"keep\" or \"score\""),
        };
        Ok(Self {
            model,
            apply_to: purposes_param(p, "apply_to", &RetrievalPurpose::ALL)?,
            max_candidates,
            keep_pictures,
        })
    }

    /// The model `model` names.
    pub fn reranker_model(&self) -> anyhow::Result<margin_embed::RerankerModel> {
        margin_embed::RerankerModel::from_name(&self.model).with_context(|| {
            let known: Vec<&str> = margin_embed::RerankerModel::ALL
                .iter()
                .map(|m| m.name())
                .collect();
            format!(
                "params.model \"{}\" isn't a reranker model (known: {})",
                self.model,
                known.join(", ")
            )
        })
    }
}

/// The listwise reranker on a provider's small model.
pub struct ListwiseSmall {
    pub params: ListwiseParams,
    pub models: Arc<dyn SmallModels>,
    pub deps: StageDeps,
}

/// The names of the documents `hits` come from, by id (missing ones are left out: the
/// label falls back to the id).
async fn document_names(deps: &StageDeps, hits: &[Hit]) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for h in hits {
        let id = &h.passage.document_id;
        if names.contains_key(id) {
            continue;
        }
        match deps.store.get::<Document>(db::DOCUMENT, id).await {
            Ok(Some(d)) => {
                names.insert(id.clone(), d.name);
            }
            Ok(None) => {}
            Err(e) => tracing::debug!("couldn't read document {id} for its name: {e:#}"),
        }
    }
    names
}

impl ListwiseSmall {
    async fn grade(&self, req: &RerankRequest<'_>, seen: &[Hit]) -> anyhow::Result<Vec<Ranked>> {
        let small = self.models.small(req.provider).await?;
        let names = document_names(&self.deps, seen).await;
        let candidates = rerank_candidates(seen, &names);
        let opts = CallOptions::new(Route::Small)
            .with_cache_key(format!("rerank:{RERANK_VERSION}"))
            .with_cache_prefix_len(RERANK_CONSTANT.len())
            .with_prompt_version(RERANK_VERSION)
            .with_max_output_tokens(RERANK_MAX_OUTPUT_TOKENS);
        let ctx = retrieval_ctx();
        let call = SmallCall {
            trace_id: req.trace_id,
            job_id: req.job_id,
            parent_id: ctx.as_ref().map(|c| c.row_id.as_str()),
        };
        self.models
            .rerank(&small, req.query, &candidates, &opts, call)
            .await
    }
}

impl Reranker for ListwiseSmall {
    fn name(&self) -> &'static str {
        LISTWISE_SMALL
    }
    fn rerank<'a>(
        &'a self,
        req: &'a RerankRequest<'a>,
        hits: Vec<Hit>,
    ) -> BoxFuture<'a, anyhow::Result<Vec<Hit>>> {
        Box::pin(async move {
            if !self.params.apply_to.contains(&req.purpose) || hits.is_empty() {
                return Ok(hits);
            }
            let n = self.params.max_candidates.min(hits.len());
            let ranked = tokio::time::timeout(self.params.timeout, self.grade(req, &hits[..n]))
                .await
                .map_err(|_| {
                    anyhow::anyhow!(
                        "the listwise rerank took longer than {:?}",
                        self.params.timeout
                    )
                })??;
            let seen: std::collections::HashSet<&str> =
                hits[..n].iter().map(|h| h.passage.id.as_str()).collect();
            let scored: Vec<(String, f32)> = ranked
                .into_iter()
                .filter(|r| seen.contains(r.id.as_str()))
                .map(|r| (r.id, f32::from(r.grade.min(3))))
                .collect();
            anyhow::ensure!(
                !scored.is_empty(),
                "the listwise rerank graded none of the candidates"
            );
            let mut out = rescore(hits, &scored);
            if self.params.drop_grade_0 {
                let floor = retrieval_ctx().map_or(0, |c| c.passages_min);
                out = drop_grade_0(out, floor);
            }
            Ok(out)
        })
    }
}

/// Drops the hits graded 0, the lowest first, while more than `passages_min` hits are
/// left.
pub fn drop_grade_0(mut hits: Vec<Hit>, passages_min: usize) -> Vec<Hit> {
    let mut i = hits.len();
    while i > 0 && hits.len() > passages_min {
        i -= 1;
        if hits[i].rerank == Some(0.0) {
            hits.remove(i);
        }
    }
    hits
}

/// The cross-encoder over `margin_embed::Reranker` (the ONNX model, or `FakeReranker`
/// in tests), run on a blocking thread.
pub struct CrossEncoder {
    pub params: CrossEncoderParams,
    pub model: Arc<dyn margin_embed::Reranker>,
}

impl Reranker for CrossEncoder {
    fn name(&self) -> &'static str {
        ONNX_CROSSENCODER
    }
    fn rerank<'a>(
        &'a self,
        req: &'a RerankRequest<'a>,
        hits: Vec<Hit>,
    ) -> BoxFuture<'a, anyhow::Result<Vec<Hit>>> {
        Box::pin(async move {
            if !self.params.apply_to.contains(&req.purpose) || hits.is_empty() {
                return Ok(hits);
            }
            let n = self.params.max_candidates.min(hits.len());
            let is_picture = |h: &Hit| self.params.keep_pictures && h.passage.modality != "text";
            let (ids, passages): (Vec<String>, Vec<String>) = hits[..n]
                .iter()
                .filter(|h| !is_picture(h))
                .map(|h| (h.passage.id.clone(), h.passage.text.clone()))
                .unzip();
            let pictures: Vec<String> = hits[..n]
                .iter()
                .filter(|h| is_picture(h))
                .map(|h| h.passage.id.clone())
                .collect();
            let logits = if passages.is_empty() {
                Vec::new()
            } else {
                let model = self.model.clone();
                let query = req.query.to_string();
                tokio::task::spawn_blocking(move || model.score(&query, &passages))
                    .await
                    .context("the cross-encoder stopped")?
                    .context("the cross-encoder couldn't score the candidates")?
            };
            anyhow::ensure!(
                logits.len() == ids.len(),
                "the cross-encoder scored {} of {} candidates",
                logits.len(),
                ids.len()
            );
            anyhow::ensure!(
                logits.iter().all(|l| l.is_finite()),
                "the cross-encoder returned a score that isn't a number"
            );
            let scaled = scale_logits(&logits);
            let mut by_id: HashMap<String, (Option<f32>, f32)> = ids
                .into_iter()
                .zip(logits.iter().zip(scaled))
                .map(|(id, (&logit, base))| (id, (Some(logit), base)))
                .collect();
            if !pictures.is_empty() {
                // A picture stands where its fused score puts it on the model's scale.
                let norm = fused_norm(&hits[..n].iter().collect::<Vec<_>>());
                for id in pictures {
                    let base = norm.get(id.as_str()).copied().unwrap_or(0.0) * CROSS_ENCODER_SCALE;
                    by_id.insert(id, (None, base));
                }
            }
            Ok(rescore_by(hits, &by_id))
        })
    }
}

/// Logits min-max scaled into `[0, CROSS_ENCODER_SCALE)`; all equal → all 0.
pub fn scale_logits(logits: &[f32]) -> Vec<f32> {
    let (lo, hi) = logits
        .iter()
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &l| {
            (lo.min(l), hi.max(l))
        });
    let span = hi - lo;
    logits
        .iter()
        .map(|&l| {
            if span > 0.0 {
                (l - lo) / span * CROSS_ENCODER_SCALE * 0.999_9
            } else {
                0.0
            }
        })
        .collect()
}

/// Each hit's fused score min-max scaled into `[0, 1)` by passage id (all equal → 0).
fn fused_norm<'h>(hits: &[&'h Hit]) -> HashMap<&'h str, f32> {
    let (lo, hi) = hits
        .iter()
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), h| {
            (lo.min(h.score), hi.max(h.score))
        });
    let span = hi - lo;
    hits.iter()
        .map(|h| {
            let n = if span > 0.0 && span.is_finite() {
                (h.score - lo) / span * 0.999_9
            } else {
                0.0
            };
            (h.passage.id.as_str(), n)
        })
        .collect()
}

/// D1's score rewrite: each hit in `scored` (by passage id) gets `rerank = Some(score)`
/// and `score = score + fused score min-max scaled into [0, 1)`; hits not in `scored`
/// keep their fused score and follow; the list comes back sorted by `score`
/// descending.
pub fn rescore(hits: Vec<Hit>, scored: &[(String, f32)]) -> Vec<Hit> {
    let by_id: HashMap<String, (Option<f32>, f32)> = scored
        .iter()
        .map(|(id, s)| (id.clone(), (Some(*s), *s)))
        .collect();
    rescore_by(hits, &by_id)
}

/// `rescore` with the `rerank` value apart from the base the score is built on (a
/// cross-encoder keeps its logit in `rerank` and scores on the scaled logit; a picture it
/// didn't score has no `rerank` and still takes its place). Hits not in `by_id` keep their
/// fused score, capped at the lowest rescored hit's so they follow.
fn rescore_by(hits: Vec<Hit>, by_id: &HashMap<String, (Option<f32>, f32)>) -> Vec<Hit> {
    let seen: Vec<&Hit> = hits
        .iter()
        .filter(|h| by_id.contains_key(&h.passage.id))
        .collect();
    let norm: HashMap<String, f32> = fused_norm(&seen)
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    let (mut rescored, mut rest): (Vec<Hit>, Vec<Hit>) = hits
        .into_iter()
        .partition(|h| by_id.contains_key(&h.passage.id));
    for h in &mut rescored {
        let (rerank, base) = by_id[&h.passage.id];
        h.rerank = rerank;
        h.score = base + norm.get(&h.passage.id).copied().unwrap_or(0.0);
    }
    rescored.sort_by(|a, b| b.score.total_cmp(&a.score));
    if let Some(floor) = rescored.last().map(|h| h.score) {
        for h in &mut rest {
            h.score = h.score.min(floor);
        }
    }
    rescored.append(&mut rest);
    rescored
}

/// Registers `"listwise-small"` and `"onnx-crossencoder"`. A constructor without
/// `deps.models` (or without the reranker download) fails with a message Settings
/// shows.
pub fn register(reg: &mut Registry) {
    reg.register(
        LISTWISE_SMALL,
        StageCtor::Reranker(Arc::new(|cfg, deps| {
            let params = ListwiseParams::from_cfg(cfg)?;
            let models = deps.models.clone().with_context(|| {
                format!(
                    "the reranker \"{LISTWISE_SMALL}\" needs a small-tasks model; none is configured here"
                )
            })?;
            Ok(Arc::new(ListwiseSmall {
                params,
                models,
                deps: deps.clone(),
            }))
        })),
    );
    reg.register(
        ONNX_CROSSENCODER,
        StageCtor::Reranker(Arc::new(|cfg, deps| {
            let params = CrossEncoderParams::from_cfg(cfg)?;
            let which = params.reranker_model()?;
            let models = deps.models.as_ref().with_context(|| {
                format!(
                    "the reranker \"{ONNX_CROSSENCODER}\" needs the reranker model; download the reranker model in Settings → Sources first"
                )
            })?;
            let model = models.cross_encoder(which)?;
            Ok(Arc::new(CrossEncoder { params, model }))
        })),
    );
}

/// Fakes for the model-backed stages' tests (here and in `rewrite.rs`).
#[cfg(test)]
pub(crate) mod fakes {
    use std::sync::Mutex;
    use std::time::Duration;

    use margin_ai::{
        AiError, CallOptions, InputPart, ModelWindow, Ranked, RerankCandidate, RewriteInput,
        TextModel, TextOutput,
    };
    use margin_shared::Provider;

    use crate::db::PassageRow;
    use crate::engines::BoxFuture;
    use crate::pipeline::{Hit, SmallCall, SmallModel, SmallModels};

    /// A text model nothing may call: the fakes answer the stages' calls themselves.
    pub struct NoText;

    impl TextModel for NoText {
        fn stream_parts<'a>(
            &'a self,
            _instructions: &'a str,
            _input: &'a [InputPart],
            _opts: &'a CallOptions,
            _on_delta: Box<dyn FnMut(&str) + Send + 'a>,
        ) -> BoxFuture<'a, Result<TextOutput, AiError>> {
            Box::pin(async { Err(AiError::Incomplete("no text calls in this test".into())) })
        }
    }

    /// One call a fake got.
    #[derive(Debug, Clone)]
    pub struct Call {
        pub provider: Provider,
        pub query: String,
        pub candidates: Vec<RerankCandidate>,
        pub rewrite: Option<RewriteInput>,
        pub opts: CallOptions,
        pub trace_id: String,
        pub job_id: Option<String>,
        pub parent_id: Option<String>,
    }

    /// Scripted small models: `None` replies fail the call; `no_model` fails `small`;
    /// `delay` holds every call that long.
    #[derive(Default)]
    pub struct FakeModels {
        pub no_model: bool,
        pub delay: Option<Duration>,
        pub rerank_reply: Mutex<Option<Vec<Ranked>>>,
        pub rewrite_reply: Mutex<Option<Vec<String>>>,
        pub calls: Mutex<Vec<Call>>,
    }

    impl FakeModels {
        pub fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        async fn hold(&self) {
            if let Some(d) = self.delay {
                tokio::time::sleep(d).await;
            }
        }
    }

    impl SmallModels for FakeModels {
        fn small<'a>(
            &'a self,
            provider: Option<Provider>,
        ) -> BoxFuture<'a, anyhow::Result<SmallModel>> {
            Box::pin(async move {
                anyhow::ensure!(!self.no_model, "Choose a model for small tasks first.");
                Ok(SmallModel {
                    model: std::sync::Arc::new(NoText),
                    provider: provider.unwrap_or(Provider::OpenAi),
                    id: "small-test".into(),
                    window: ModelWindow {
                        input: 100_000,
                        output: 1_000,
                    },
                })
            })
        }

        fn rerank<'a>(
            &'a self,
            small: &'a SmallModel,
            query: &'a str,
            candidates: &'a [RerankCandidate],
            opts: &'a CallOptions,
            call: SmallCall<'a>,
        ) -> BoxFuture<'a, anyhow::Result<Vec<Ranked>>> {
            Box::pin(async move {
                self.calls.lock().unwrap().push(Call {
                    provider: small.provider,
                    query: query.into(),
                    candidates: candidates.to_vec(),
                    rewrite: None,
                    opts: opts.clone(),
                    trace_id: call.trace_id.into(),
                    job_id: call.job_id.map(Into::into),
                    parent_id: call.parent_id.map(Into::into),
                });
                self.hold().await;
                let reply = self.rerank_reply.lock().unwrap().clone();
                reply.ok_or_else(|| anyhow::anyhow!("no rerank scripted"))
            })
        }

        fn rewrite_queries<'a>(
            &'a self,
            small: &'a SmallModel,
            input: &'a RewriteInput,
            opts: &'a CallOptions,
            call: SmallCall<'a>,
        ) -> BoxFuture<'a, anyhow::Result<Vec<String>>> {
            Box::pin(async move {
                self.calls.lock().unwrap().push(Call {
                    provider: small.provider,
                    query: input.query.clone(),
                    candidates: Vec::new(),
                    rewrite: Some(input.clone()),
                    opts: opts.clone(),
                    trace_id: call.trace_id.into(),
                    job_id: call.job_id.map(Into::into),
                    parent_id: call.parent_id.map(Into::into),
                });
                self.hold().await;
                let reply = self.rewrite_reply.lock().unwrap().clone();
                reply.ok_or_else(|| anyhow::anyhow!("no rewrite scripted"))
            })
        }

        fn cross_encoder(
            &self,
            model: margin_embed::RerankerModel,
        ) -> anyhow::Result<std::sync::Arc<dyn margin_embed::Reranker>> {
            anyhow::ensure!(
                !self.no_model,
                "the reranker model \"{}\" isn't downloaded; download the reranker model in Settings → Sources first",
                model.name()
            );
            Ok(std::sync::Arc::new(margin_embed::fake::FakeReranker))
        }
    }

    /// A text hit of document `doc` with fused score `score`.
    pub fn hit(id: &str, doc: &str, text: &str, score: f32) -> Hit {
        Hit {
            passage: PassageRow {
                id: id.into(),
                project_id: String::new(),
                document_id: doc.into(),
                origin: "source".into(),
                scope_ids: Vec::new(),
                access: Vec::new(),
                unit_id: format!("{doc}:{id}"),
                modality: "text".into(),
                ord: 0,
                page: None,
                start_line: None,
                end_line: None,
                start_ms: None,
                end_ms: None,
                section_path: String::new(),
                text: text.into(),
                file: None,
                chunk_hash: String::new(),
                embedder: String::new(),
                vector: Vec::new(),
            },
            score,
            dense_rank: Some(1),
            lex_rank: None,
            rerank: None,
            highlight: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fakes::{FakeModels, hit};
    use super::*;
    use crate::db::Store;
    use crate::pipeline::{Pipeline, RetrievalCtx, with_retrieval};
    use margin_shared::Provider;
    use margin_shared::pipeline::PipelineConfig;

    fn ids(hits: &[Hit]) -> Vec<&str> {
        hits.iter().map(|h| h.passage.id.as_str()).collect()
    }

    fn sorted(hits: &[Hit]) -> bool {
        hits.windows(2).all(|w| w[0].score >= w[1].score)
    }

    fn fused() -> Vec<Hit> {
        vec![
            hit("a", "d1", "alpha", 0.9),
            hit("b", "d1", "beta", 0.6),
            hit("c", "d2", "gamma", 0.3),
            hit("d", "d2", "delta", 0.1),
        ]
    }

    fn req<'a>(purpose: RetrievalPurpose) -> RerankRequest<'a> {
        RerankRequest {
            query: "gibbs ringing\nAlso asked: overshoot",
            purpose,
            provider: Some(Provider::Claude),
            trace_id: "lesson-1",
            job_id: Some("lesson-1"),
        }
    }

    fn ranked(grades: &[(&str, u8)]) -> Vec<Ranked> {
        grades
            .iter()
            .map(|(id, grade)| Ranked {
                id: id.to_string(),
                grade: *grade,
            })
            .collect()
    }

    async fn listwise(models: Arc<FakeModels>, params: serde_json::Value) -> ListwiseSmall {
        let cfg = StageCfg {
            name: LISTWISE_SMALL.into(),
            params: params.as_object().cloned().unwrap_or_default(),
        };
        ListwiseSmall {
            params: ListwiseParams::from_cfg(&cfg).unwrap(),
            models,
            deps: StageDeps::new(Store::memory().await.unwrap()),
        }
    }

    #[test]
    fn rescoring_puts_grades_first_and_keeps_the_list_sorted() {
        let out = rescore(
            fused(),
            &[("c".into(), 3.0), ("a".into(), 1.0), ("b".into(), 1.0)],
        );
        assert_eq!(ids(&out), ["c", "a", "b", "d"]);
        assert!(sorted(&out));
        // Graded hits: grade + fused score scaled into [0, 1).
        assert!(out[0].score >= 3.0 && out[0].score < 4.0);
        assert!(
            out[1].score > out[2].score,
            "a's fused score is higher than b's"
        );
        assert!(out[1].score >= 1.0 && out[1].score < 2.0);
        assert_eq!(out[0].rerank, Some(3.0));
        // Ranks from the legs are kept; the unseen hit has no rerank and follows.
        assert!(out.iter().all(|h| h.dense_rank == Some(1)));
        assert_eq!(out[3].rerank, None);
        assert!(out[3].score <= out[2].score);
    }

    #[test]
    fn grade_0_drops_never_go_below_passages_min() {
        let graded = rescore(
            fused(),
            &[
                ("a".into(), 0.0),
                ("b".into(), 2.0),
                ("c".into(), 0.0),
                ("d".into(), 0.0),
            ],
        );
        assert_eq!(ids(&drop_grade_0(graded.clone(), 0)), ["b"]);
        // The best grade-0 hits stay to make up passages_min.
        assert_eq!(ids(&drop_grade_0(graded.clone(), 2)), ["b", "a"]);
        assert_eq!(drop_grade_0(graded.clone(), 9).len(), 4);
    }

    #[tokio::test]
    async fn listwise_grades_the_candidates_on_the_small_route() {
        let models = Arc::new(FakeModels::default());
        *models.rerank_reply.lock().unwrap() =
            Some(ranked(&[("c", 3), ("b", 2), ("a", 1), ("d", 0)]));
        let stage = listwise(models.clone(), serde_json::json!({})).await;
        // Labels use the document's name and page.
        stage
            .deps
            .store
            .put(
                db::DOCUMENT,
                "d2",
                &Document {
                    id: "d2".into(),
                    project_id: "p".into(),
                    origin: margin_shared::context::Origin::Source {
                        source_id: "s2".into(),
                    },
                    name: "notes/paper.pdf".into(),
                    kind: margin_shared::SourceKind::Pdf,
                    status: margin_shared::context::DocumentStatus::Ready { passages: 2 },
                    generation: 1,
                    bytes: 1,
                    actor: "owner".into(),
                    access: Vec::new(),
                    created_at: chrono::Utc::now(),
                    updated_at: chrono::Utc::now(),
                },
            )
            .await
            .unwrap();
        let mut hits = fused();
        hits[2].passage.page = Some(3);
        hits[2].passage.section_path = "Fourier series > Coefficients".into();
        let ctx = RetrievalCtx {
            row_id: "row-1".into(),
            passages_min: 0,
        };
        let out = with_retrieval(ctx, stage.rerank(&req(RetrievalPurpose::Lesson), hits))
            .await
            .unwrap();
        assert_eq!(ids(&out), ["c", "b", "a"], "grade 0 dropped");
        assert!(sorted(&out));
        let calls = models.calls();
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert_eq!(call.provider, Provider::Claude);
        assert_eq!(call.query, "gibbs ringing\nAlso asked: overshoot");
        assert_eq!(call.opts.route, Route::Small);
        assert_eq!(call.opts.prompt_version.as_deref(), Some(RERANK_VERSION));
        assert_eq!(call.opts.cache_key.as_deref(), Some("rerank:rerank-v1"));
        assert_eq!(call.opts.cache_prefix_len, Some(RERANK_CONSTANT.len()));
        assert_eq!(call.opts.max_output_tokens, Some(RERANK_MAX_OUTPUT_TOKENS));
        assert_eq!(call.trace_id, "lesson-1");
        assert_eq!(call.job_id.as_deref(), Some("lesson-1"));
        assert_eq!(call.parent_id.as_deref(), Some("row-1"));
        let c = call.candidates.iter().find(|c| c.id == "c").unwrap();
        assert_eq!(
            c.label,
            "notes/paper.pdf, p. 3, Fourier series > Coefficients"
        );
        assert_eq!(
            call.candidates[0].label, "d1",
            "an unknown document keeps its id"
        );
    }

    #[tokio::test]
    async fn listwise_keeps_passages_min_and_honours_drop_grade_0() {
        let models = Arc::new(FakeModels::default());
        *models.rerank_reply.lock().unwrap() =
            Some(ranked(&[("c", 3), ("b", 0), ("a", 0), ("d", 0)]));
        let stage = listwise(models.clone(), serde_json::json!({})).await;
        let ctx = RetrievalCtx {
            row_id: "r".into(),
            passages_min: 2,
        };
        let out = with_retrieval(ctx, stage.rerank(&req(RetrievalPurpose::Plan), fused()))
            .await
            .unwrap();
        assert_eq!(ids(&out), ["c", "a"]);
        let stage = listwise(models, serde_json::json!({"drop_grade_0": false})).await;
        let out = stage
            .rerank(&req(RetrievalPurpose::Plan), fused())
            .await
            .unwrap();
        assert_eq!(out.len(), 4);
    }

    #[tokio::test]
    async fn listwise_sees_at_most_max_candidates_and_the_rest_follow() {
        let models = Arc::new(FakeModels::default());
        *models.rerank_reply.lock().unwrap() = Some(ranked(&[("b", 3), ("a", 2)]));
        let stage = listwise(models.clone(), serde_json::json!({"max_candidates": 2})).await;
        let out = stage
            .rerank(&req(RetrievalPurpose::Lesson), fused())
            .await
            .unwrap();
        assert_eq!(ids(&out), ["b", "a", "c", "d"]);
        assert!(sorted(&out));
        assert_eq!(models.calls()[0].candidates.len(), 2);
        assert_eq!(out[2].rerank, None);
    }

    #[tokio::test]
    async fn listwise_passes_other_purposes_through_without_a_call() {
        let models = Arc::new(FakeModels::default());
        let stage = listwise(models.clone(), serde_json::json!({})).await;
        for purpose in [RetrievalPurpose::Agent, RetrievalPurpose::Context] {
            let out = stage.rerank(&req(purpose), fused()).await.unwrap();
            assert_eq!(out, fused());
        }
        assert!(models.calls().is_empty());
        // A variant can widen it.
        *models.rerank_reply.lock().unwrap() = Some(ranked(&[("d", 3)]));
        let stage = listwise(models.clone(), serde_json::json!({"apply_to": ["agent"]})).await;
        let out = stage
            .rerank(&req(RetrievalPurpose::Agent), fused())
            .await
            .unwrap();
        assert_eq!(out[0].passage.id, "d");
        assert_eq!(models.calls().len(), 1);
        let out = stage
            .rerank(&req(RetrievalPurpose::Lesson), fused())
            .await
            .unwrap();
        assert_eq!(out, fused(), "lesson isn't in this apply_to");
    }

    #[tokio::test]
    async fn every_listwise_failure_is_an_error_the_caller_answers_with_the_fused_order() {
        // No small-tasks model.
        let models = Arc::new(FakeModels {
            no_model: true,
            ..Default::default()
        });
        let stage = listwise(models, serde_json::json!({})).await;
        assert!(
            stage
                .rerank(&req(RetrievalPurpose::Lesson), fused())
                .await
                .is_err()
        );
        // The call fails (a refusal, bad JSON: the model's error).
        let models = Arc::new(FakeModels::default());
        let stage = listwise(models.clone(), serde_json::json!({})).await;
        assert!(
            stage
                .rerank(&req(RetrievalPurpose::Lesson), fused())
                .await
                .is_err()
        );
        // Ids the model made up: nothing graded.
        *models.rerank_reply.lock().unwrap() = Some(ranked(&[("zz", 3)]));
        let err = stage
            .rerank(&req(RetrievalPurpose::Lesson), fused())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("none of the candidates"), "{err}");
        // A timeout.
        let models = Arc::new(FakeModels {
            delay: Some(Duration::from_millis(300)),
            ..Default::default()
        });
        *models.rerank_reply.lock().unwrap() = Some(ranked(&[("a", 3)]));
        let stage = listwise(models, serde_json::json!({"timeout_secs": 0.05})).await;
        let err = stage
            .rerank(&req(RetrievalPurpose::Lesson), fused())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("longer than"), "{err}");
    }

    #[tokio::test]
    async fn the_stages_need_models_and_valid_params() {
        let store = Store::memory().await.unwrap();
        let without = Registry::standard(StageDeps::new(store.clone()));
        let mut deps = StageDeps::new(store);
        deps.models = Some(Arc::new(FakeModels::default()));
        let with = Registry::standard(deps);
        for name in [LISTWISE_SMALL, ONNX_CROSSENCODER] {
            let cfg = PipelineConfig {
                reranker: StageCfg::named(name),
                ..PipelineConfig::default()
            };
            let err = Pipeline::build(&cfg, &without).err().unwrap();
            let err = format!("{err:#}");
            assert!(
                err.contains("small-tasks model") || err.contains("Settings → Sources"),
                "{err}"
            );
            assert_eq!(Pipeline::build(&cfg, &with).unwrap().reranker.name(), name);
        }
        for (name, params) in [
            (
                LISTWISE_SMALL,
                serde_json::json!({"apply_to": ["everything"]}),
            ),
            (LISTWISE_SMALL, serde_json::json!({"max_candidates": 0})),
            (LISTWISE_SMALL, serde_json::json!({"timeout_secs": -1})),
            (ONNX_CROSSENCODER, serde_json::json!({"model": "bert-huge"})),
            (ONNX_CROSSENCODER, serde_json::json!({"pictures": "drop"})),
        ] {
            let cfg = PipelineConfig {
                reranker: StageCfg {
                    name: name.into(),
                    params: params.as_object().cloned().unwrap(),
                },
                ..PipelineConfig::default()
            };
            assert!(Pipeline::build(&cfg, &with).is_err(), "{params}");
        }
        // A missing download: unavailable, with the message Settings shows.
        let mut deps = StageDeps::new(Store::memory().await.unwrap());
        deps.models = Some(Arc::new(FakeModels {
            no_model: true,
            ..Default::default()
        }));
        let missing = Registry::standard(deps);
        let cfg = PipelineConfig {
            reranker: StageCfg::named(ONNX_CROSSENCODER),
            ..PipelineConfig::default()
        };
        let err = format!("{:#}", Pipeline::build(&cfg, &missing).err().unwrap());
        assert!(err.contains("download the reranker model"), "{err}");
    }

    fn cross_encoder(params: serde_json::Value) -> CrossEncoder {
        let cfg = StageCfg {
            name: ONNX_CROSSENCODER.into(),
            params: params.as_object().cloned().unwrap_or_default(),
        };
        CrossEncoder {
            params: CrossEncoderParams::from_cfg(&cfg).unwrap(),
            model: Arc::new(margin_embed::fake::FakeReranker),
        }
    }

    #[tokio::test]
    async fn the_cross_encoder_scores_every_purpose_on_one_scale() {
        let stage = cross_encoder(serde_json::json!({}));
        let hits = vec![
            hit("a", "d1", "unrelated words", 0.9),
            hit("b", "d1", "gibbs ringing overshoot", 0.5),
            hit("c", "d1", "gibbs only", 0.3),
        ];
        for purpose in RetrievalPurpose::ALL {
            let out = stage.rerank(&req(purpose), hits.clone()).await.unwrap();
            assert_eq!(ids(&out), ["b", "c", "a"], "{purpose:?}");
            assert!(sorted(&out));
            // The logit (shared words) is kept; the score is on [0, 4) plus the fused.
            assert_eq!(out[0].rerank, Some(3.0));
            assert!(
                out[0].score >= 3.9 && out[0].score < 5.0,
                "{}",
                out[0].score
            );
            assert!(out[2].score < 1.0);
        }
        // apply_to gates it.
        let stage = cross_encoder(serde_json::json!({"apply_to": ["lesson"]}));
        let out = stage
            .rerank(&req(RetrievalPurpose::Agent), hits.clone())
            .await
            .unwrap();
        assert_eq!(out, hits);
        assert_eq!(scale_logits(&[2.0, 2.0]), [0.0, 0.0]);
    }

    #[tokio::test]
    async fn pictures_keep_their_place_under_the_cross_encoder() {
        let mut picture = hit("p", "d1", "", 0.9);
        picture.passage.modality = "image".into();
        let hits = vec![
            picture,
            hit("b", "d1", "gibbs ringing", 0.5),
            hit("c", "d1", "nothing", 0.1),
        ];
        let out = cross_encoder(serde_json::json!({}))
            .rerank(&req(RetrievalPurpose::Agent), hits.clone())
            .await
            .unwrap();
        // The picture had the best fused score: it stays near the top, unscored.
        assert_eq!(out[0].passage.id, "p", "{:?}", ids(&out));
        assert_eq!(out[0].rerank, None);
        assert!(sorted(&out));
        // "score" lets the model score it like any passage (its OCR text is empty).
        let out = cross_encoder(serde_json::json!({"pictures": "score"}))
            .rerank(&req(RetrievalPurpose::Agent), hits)
            .await
            .unwrap();
        assert_eq!(out[0].passage.id, "b");
    }

    #[tokio::test]
    async fn a_cross_encoder_that_fails_is_an_error() {
        struct Broken;
        impl margin_embed::Reranker for Broken {
            fn name(&self) -> &str {
                "broken"
            }
            fn score(
                &self,
                _q: &str,
                passages: &[String],
            ) -> Result<Vec<f32>, margin_embed::EmbedError> {
                Ok(vec![1.0; passages.len().saturating_sub(1)])
            }
        }
        let mut stage = cross_encoder(serde_json::json!({}));
        stage.model = Arc::new(Broken);
        assert!(
            stage
                .rerank(&req(RetrievalPurpose::Agent), fused())
                .await
                .is_err()
        );
    }
}
