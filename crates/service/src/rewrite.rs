//! Query rewriting (R2, `docs/r2-brief.md` D3): `"small-expand-v1"` adds up to
//! `routes::REWRITE_MAX_ADDED` phrasings to a thin plan on the `Small` route, with a
//! `RewriteCache` so a repeated agent search pays once. The prompt sees the query and
//! document names only. A failure or timeout is an error the caller answers by keeping
//! the plan as it was. Not the default (`"none"`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use margin_ai::prompts::{REWRITE_CONSTANT, REWRITE_VERSION};
use margin_ai::{CallOptions, RewriteInput};
use margin_shared::pipeline::{RetrievalPurpose, Route, StageCfg};
use margin_shared::routes::{REWRITE_MAX_ADDED, REWRITE_MAX_OUTPUT_TOKENS, REWRITE_MIN_WORDS};

use crate::engines::BoxFuture;
use crate::pipeline::{
    Registry, RewriteRequest, Rewriter, SmallCall, SmallModels, StageCtor, StageDeps, retrieval_ctx,
};
use crate::rerank::{purposes_param, secs_param, usize_param};
use crate::retrieval::QueryPlan;

/// The stage name.
pub const SMALL_EXPAND_V1: &str = "small-expand-v1";
/// Entries the rewrite cache keeps.
pub const REWRITE_CACHE_ENTRIES: usize = 1_024;
/// Most document names the prompt is given.
pub const REWRITE_SCOPE_NAMES: usize = 12;
/// A lesson's or the planner's plan with fewer strings than this is rewritten.
pub const THIN_PLAN: usize = 3;

/// `"small-expand-v1"`'s params (`StageCfg.params`).
#[derive(Debug, Clone, PartialEq)]
pub struct ExpandParams {
    /// `Agent`/`Context` queries with fewer words are rewritten
    /// (`routes::REWRITE_MIN_WORDS`).
    pub min_words: usize,
    /// `routes::REWRITE_MAX_ADDED`.
    pub max_added: usize,
    /// Default every purpose; `Lesson`/`Plan` only when the plan has fewer than 3
    /// strings.
    pub apply_to: Vec<RetrievalPurpose>,
    /// Default 10 s.
    pub timeout: Duration,
}

impl ExpandParams {
    pub fn from_cfg(cfg: &StageCfg) -> anyhow::Result<Self> {
        let p = &cfg.params;
        let max_added = usize_param(p, "max_added", REWRITE_MAX_ADDED)?;
        anyhow::ensure!(max_added > 0, "params.max_added must be at least 1");
        Ok(Self {
            min_words: usize_param(p, "min_words", REWRITE_MIN_WORDS)?,
            max_added,
            apply_to: purposes_param(p, "apply_to", &RetrievalPurpose::ALL)?,
            timeout: secs_param(p, "timeout_secs", Duration::from_secs(10))?,
        })
    }

    /// Whether `plan` is one this rewriter expands for `purpose`.
    pub fn wants(&self, purpose: RetrievalPurpose, plan: &QueryPlan) -> bool {
        let Some(first) = plan.queries.first() else {
            return false;
        };
        if !self.apply_to.contains(&purpose) {
            return false;
        }
        match purpose {
            RetrievalPurpose::Agent | RetrievalPurpose::Context => {
                first.split_whitespace().count() < self.min_words
            }
            RetrievalPurpose::Lesson | RetrievalPurpose::Plan => plan.queries.len() < THIN_PLAN,
        }
    }
}

/// LRU of rewrites keyed by `hash(rewriter cfg, prompt version, purpose, provider,
/// query, scope_names)`: the strings a rewrite added.
pub struct RewriteCache {
    cap: usize,
    inner: Mutex<CacheInner>,
}

#[derive(Default)]
struct CacheInner {
    tick: u64,
    entries: HashMap<String, (u64, Vec<String>)>,
}

impl Default for RewriteCache {
    fn default() -> Self {
        Self::new(REWRITE_CACHE_ENTRIES)
    }
}

impl RewriteCache {
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            inner: Mutex::default(),
        }
    }

    pub fn get(&self, key: &str) -> Option<Vec<String>> {
        let mut inner = self.inner.lock().unwrap();
        inner.tick += 1;
        let tick = inner.tick;
        let (used, added) = inner.entries.get_mut(key)?;
        *used = tick;
        Some(added.clone())
    }

    pub fn insert(&self, key: String, added: Vec<String>) {
        let mut inner = self.inner.lock().unwrap();
        inner.tick += 1;
        let tick = inner.tick;
        if !inner.entries.contains_key(&key)
            && inner.entries.len() >= self.cap
            && let Some(oldest) = inner
                .entries
                .iter()
                .min_by_key(|(_, (used, _))| *used)
                .map(|(k, _)| k.clone())
        {
            inner.entries.remove(&oldest);
        }
        inner.entries.insert(key, (tick, added));
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The rewriter on a provider's small model.
pub struct SmallExpand {
    pub params: ExpandParams,
    pub models: Arc<dyn SmallModels>,
    pub cache: Arc<RewriteCache>,
    pub deps: StageDeps,
    /// The stage's config as canonical JSON: part of every cache key, so other params
    /// never share an entry.
    pub cfg_key: String,
}

impl SmallExpand {
    /// The cache key of one rewrite.
    pub fn cache_key(&self, req: &RewriteRequest<'_>, query: &str, names: &[String]) -> String {
        let key = serde_json::json!({
            "cfg": self.cfg_key,
            "version": REWRITE_VERSION,
            "purpose": req.purpose,
            "provider": req.provider,
            "query": query,
            "scope": names,
        });
        crate::state::sha256_hex(&key.to_string())
    }

    async fn call(
        &self,
        req: &RewriteRequest<'_>,
        input: &RewriteInput,
    ) -> anyhow::Result<Vec<String>> {
        let small = self.models.small(req.provider).await?;
        let opts = CallOptions::new(Route::Small)
            .with_cache_key(format!("rewrite:{REWRITE_VERSION}"))
            .with_cache_prefix_len(REWRITE_CONSTANT.len())
            .with_prompt_version(REWRITE_VERSION)
            .with_max_output_tokens(REWRITE_MAX_OUTPUT_TOKENS);
        let ctx = retrieval_ctx();
        let call = SmallCall {
            trace_id: req.trace_id,
            job_id: req.job_id,
            parent_id: ctx.as_ref().map(|c| c.row_id.as_str()),
        };
        self.models
            .rewrite_queries(&small, input, &opts, call)
            .await
    }
}

impl Rewriter for SmallExpand {
    fn name(&self) -> &'static str {
        SMALL_EXPAND_V1
    }
    fn rewrite<'a>(
        &'a self,
        req: &'a RewriteRequest<'a>,
        plan: QueryPlan,
    ) -> BoxFuture<'a, anyhow::Result<QueryPlan>> {
        Box::pin(async move {
            if !self.params.wants(req.purpose, &plan) {
                return Ok(plan);
            }
            let query = plan.queries[0].clone();
            let names: Vec<String> = req
                .scope_names
                .iter()
                .take(REWRITE_SCOPE_NAMES)
                .cloned()
                .collect();
            let key = self.cache_key(req, &query, &names);
            let added = match self.cache.get(&key) {
                Some(added) => added,
                None => {
                    let input = RewriteInput {
                        query,
                        purpose: req.purpose,
                        scope_names: names,
                        max_added: self.params.max_added,
                    };
                    let mut added =
                        tokio::time::timeout(self.params.timeout, self.call(req, &input))
                            .await
                            .map_err(|_| {
                                anyhow::anyhow!(
                                    "the query rewrite took longer than {:?}",
                                    self.params.timeout
                                )
                            })??;
                    added.truncate(self.params.max_added);
                    self.cache.insert(key, added.clone());
                    added
                }
            };
            Ok(QueryPlan::from_strings(
                plan.queries.into_iter().chain(added),
            ))
        })
    }
}

/// Registers `"small-expand-v1"`; its constructor fails without `deps.models`. One
/// cache serves every pipeline built from this registry (keys carry the config).
pub fn register(reg: &mut Registry) {
    let cache = Arc::new(RewriteCache::default());
    reg.register(
        SMALL_EXPAND_V1,
        StageCtor::Rewriter(Arc::new(move |cfg, deps| {
            let params = ExpandParams::from_cfg(cfg)?;
            let models = deps.models.clone().with_context(|| {
                format!(
                    "the rewriter \"{SMALL_EXPAND_V1}\" needs a small-tasks model; none is configured here"
                )
            })?;
            Ok(Arc::new(SmallExpand {
                params,
                models,
                cache: cache.clone(),
                deps: deps.clone(),
                cfg_key: serde_json::to_string(cfg).unwrap_or_default(),
            }))
        })),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Store;
    use crate::pipeline::{Pipeline, RetrievalCtx, with_retrieval};
    use crate::rerank::fakes::FakeModels;
    use margin_shared::Provider;
    use margin_shared::pipeline::PipelineConfig;

    async fn stage(models: Arc<FakeModels>, params: serde_json::Value) -> SmallExpand {
        let cfg = StageCfg {
            name: SMALL_EXPAND_V1.into(),
            params: params.as_object().cloned().unwrap_or_default(),
        };
        SmallExpand {
            params: ExpandParams::from_cfg(&cfg).unwrap(),
            models,
            cache: Arc::new(RewriteCache::default()),
            deps: StageDeps::new(Store::memory().await.unwrap()),
            cfg_key: serde_json::to_string(&cfg).unwrap(),
        }
    }

    fn req<'a>(purpose: RetrievalPurpose, names: &'a [String]) -> RewriteRequest<'a> {
        RewriteRequest {
            purpose,
            provider: Some(Provider::Claude),
            trace_id: "turn-1",
            job_id: None,
            scope_names: names,
        }
    }

    fn scripted(added: &[&str]) -> Arc<FakeModels> {
        let models = Arc::new(FakeModels::default());
        *models.rewrite_reply.lock().unwrap() = Some(added.iter().map(|s| s.to_string()).collect());
        models
    }

    #[tokio::test]
    async fn a_short_agent_query_gets_phrasings_added() {
        let models = scripted(&["gibbs phenomenon ringing", "overshoot near a jump", "third"]);
        let s = stage(models.clone(), serde_json::json!({})).await;
        let names: Vec<String> = (0..20).map(|i| format!("doc-{i:02}.pdf")).collect();
        let ctx = RetrievalCtx {
            row_id: "row-9".into(),
            passages_min: 0,
        };
        let plan = with_retrieval(
            ctx,
            s.rewrite(
                &req(RetrievalPurpose::Agent, &names),
                QueryPlan::single("gibbs"),
            ),
        )
        .await
        .unwrap();
        // Capped at max_added, after the query.
        assert_eq!(
            plan.queries,
            ["gibbs", "gibbs phenomenon ringing", "overshoot near a jump"]
        );
        let calls = models.calls();
        assert_eq!(calls.len(), 1);
        let input = calls[0].rewrite.clone().unwrap();
        assert_eq!(input.query, "gibbs");
        assert_eq!(input.purpose, RetrievalPurpose::Agent);
        assert_eq!(input.max_added, REWRITE_MAX_ADDED);
        assert_eq!(input.scope_names.len(), REWRITE_SCOPE_NAMES);
        assert_eq!(calls[0].provider, Provider::Claude);
        assert_eq!(calls[0].parent_id.as_deref(), Some("row-9"));
        let opts = &calls[0].opts;
        assert_eq!(opts.route, Route::Small);
        assert_eq!(opts.prompt_version.as_deref(), Some(REWRITE_VERSION));
        assert_eq!(opts.cache_key.as_deref(), Some("rewrite:rewrite-v1"));
        assert_eq!(opts.cache_prefix_len, Some(REWRITE_CONSTANT.len()));
        assert_eq!(opts.max_output_tokens, Some(REWRITE_MAX_OUTPUT_TOKENS));
    }

    #[tokio::test]
    async fn only_thin_plans_are_rewritten() {
        let models = scripted(&["more"]);
        let s = stage(models.clone(), serde_json::json!({})).await;
        // Five words or more: no call.
        let five = QueryPlan::single("what is the gibbs phenomenon");
        let out = s
            .rewrite(&req(RetrievalPurpose::Context, &[]), five.clone())
            .await
            .unwrap();
        assert_eq!(out, five);
        // A lesson with a full plan (three strings or more): no call.
        let full = QueryPlan::from_strings(["a".into(), "b".into(), "c".into()]);
        let out = s
            .rewrite(&req(RetrievalPurpose::Lesson, &[]), full.clone())
            .await
            .unwrap();
        assert_eq!(out, full);
        assert!(models.calls().is_empty());
        // A thin lesson plan (a `write_lesson` lesson with no proposal): one call, even
        // though its strings are long.
        let thin = QueryPlan::lesson(
            "Fourier series",
            "Sums of sines and cosines that build periodic signals",
            &[],
            None,
        );
        let out = s
            .rewrite(&req(RetrievalPurpose::Lesson, &[]), thin.clone())
            .await
            .unwrap();
        assert_eq!(out.queries.len(), thin.queries.len() + 1);
        assert_eq!(models.calls().len(), 1);
        // apply_to gates every purpose.
        let s = stage(models.clone(), serde_json::json!({"apply_to": ["lesson"]})).await;
        let one = QueryPlan::single("gibbs");
        let out = s
            .rewrite(&req(RetrievalPurpose::Agent, &[]), one.clone())
            .await
            .unwrap();
        assert_eq!(out, one);
        assert_eq!(models.calls().len(), 1);
        // An empty plan is left alone.
        let out = s
            .rewrite(&req(RetrievalPurpose::Lesson, &[]), QueryPlan::default())
            .await
            .unwrap();
        assert!(out.queries.is_empty());
    }

    #[tokio::test]
    async fn a_repeated_query_is_answered_from_the_cache() {
        let models = scripted(&["gibbs phenomenon"]);
        let s = stage(models.clone(), serde_json::json!({})).await;
        let names = vec!["dsp.pdf".to_string()];
        for _ in 0..3 {
            let out = s
                .rewrite(
                    &req(RetrievalPurpose::Agent, &names),
                    QueryPlan::single("gibbs"),
                )
                .await
                .unwrap();
            assert_eq!(out.queries, ["gibbs", "gibbs phenomenon"]);
        }
        assert_eq!(models.calls().len(), 1, "cache hits skip the call");
        // Another purpose, scope or provider is another entry.
        s.rewrite(
            &req(RetrievalPurpose::Context, &names),
            QueryPlan::single("gibbs"),
        )
        .await
        .unwrap();
        s.rewrite(
            &req(RetrievalPurpose::Agent, &[]),
            QueryPlan::single("gibbs"),
        )
        .await
        .unwrap();
        let mut other = req(RetrievalPurpose::Agent, &names);
        other.provider = Some(Provider::OpenAi);
        s.rewrite(&other, QueryPlan::single("gibbs")).await.unwrap();
        assert_eq!(models.calls().len(), 4);
        // A failure isn't cached.
        *models.rewrite_reply.lock().unwrap() = None;
        let r = req(RetrievalPurpose::Agent, &names);
        assert!(s.rewrite(&r, QueryPlan::single("fourier")).await.is_err());
        *models.rewrite_reply.lock().unwrap() = Some(vec!["fourier series".into()]);
        let out = s.rewrite(&r, QueryPlan::single("fourier")).await.unwrap();
        assert_eq!(out.queries, ["fourier", "fourier series"]);
    }

    #[tokio::test]
    async fn every_rewrite_failure_is_an_error_the_caller_answers_with_the_plan() {
        let one = QueryPlan::single("gibbs");
        // No small-tasks model.
        let models = Arc::new(FakeModels {
            no_model: true,
            ..Default::default()
        });
        let s = stage(models, serde_json::json!({})).await;
        assert!(
            s.rewrite(&req(RetrievalPurpose::Agent, &[]), one.clone())
                .await
                .is_err()
        );
        // The call fails.
        let s = stage(Arc::new(FakeModels::default()), serde_json::json!({})).await;
        assert!(
            s.rewrite(&req(RetrievalPurpose::Agent, &[]), one.clone())
                .await
                .is_err()
        );
        // A timeout.
        let models = Arc::new(FakeModels {
            delay: Some(Duration::from_millis(300)),
            ..Default::default()
        });
        *models.rewrite_reply.lock().unwrap() = Some(vec!["x".into()]);
        let s = stage(models, serde_json::json!({"timeout_secs": 0.05})).await;
        let err = s
            .rewrite(&req(RetrievalPurpose::Agent, &[]), one)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("longer than"), "{err}");
    }

    #[test]
    fn the_cache_is_an_lru() {
        let cache = RewriteCache::new(2);
        cache.insert("a".into(), vec!["1".into()]);
        cache.insert("b".into(), vec!["2".into()]);
        assert!(cache.get("a").is_some());
        cache.insert("c".into(), vec!["3".into()]);
        assert_eq!(cache.len(), 2);
        assert!(cache.get("b").is_none(), "the least recently used goes");
        assert_eq!(cache.get("a"), Some(vec!["1".to_string()]));
        assert_eq!(cache.get("c"), Some(vec!["3".to_string()]));
        assert_eq!(RewriteCache::default().cap, REWRITE_CACHE_ENTRIES);
    }

    #[tokio::test]
    async fn the_rewriter_needs_models_and_valid_params() {
        let store = Store::memory().await.unwrap();
        let cfg = PipelineConfig {
            rewriter: StageCfg::named(SMALL_EXPAND_V1),
            ..PipelineConfig::default()
        };
        let without = Registry::standard(StageDeps::new(store.clone()));
        let err = format!("{:#}", Pipeline::build(&cfg, &without).err().unwrap());
        assert!(err.contains("needs a small-tasks model"), "{err}");
        let mut deps = StageDeps::new(store);
        deps.models = Some(Arc::new(FakeModels::default()));
        let with = Registry::standard(deps);
        assert_eq!(
            Pipeline::build(&cfg, &with).unwrap().rewriter.name(),
            SMALL_EXPAND_V1
        );
        let mut bad = cfg.clone();
        bad.rewriter.params = serde_json::json!({"max_added": 0})
            .as_object()
            .cloned()
            .unwrap();
        assert!(Pipeline::build(&bad, &with).is_err());
        bad.rewriter.params = serde_json::json!({"apply_to": "agent"})
            .as_object()
            .cloned()
            .unwrap();
        assert!(Pipeline::build(&bad, &with).is_err());
    }
}
