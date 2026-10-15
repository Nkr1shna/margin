//! The service's `SmallModels` (R2, `docs/r2-brief.md` D1, D2): a provider's
//! small-tasks model resolved through `AppState::model_for(Route::Small, provider)` and
//! `Generator::text_model`; the rerank and rewrite calls go through `Generator::rerank`
//! and `Generator::rewrite_queries` (scripted by `FakeGenerator` in tests) and are
//! recorded as `model_call` rows (route `small`, their prompt version, the job, the
//! retrieval row as parent, the cost). The registry is built before the `App` it reads
//! from, so the state is bound after (`bind`).
//!
//! It also finds the local cross-encoder (`"onnx-crossencoder"`): the download under
//! `data_dir/models`, opened by `SourceTools::load_reranker` on its first `score`
//! (blocking, on the caller's blocking thread) and kept per model; the ONNX session
//! unloads itself when idle.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use margin_ai::prompts::{RERANK_VERSION, REWRITE_VERSION};
use margin_ai::{CallOptions, Generated, Ranked, RerankCandidate, RewriteInput};
use margin_embed::{EmbedError, RerankerFiles, RerankerModel};
use margin_shared::Provider;
use margin_shared::pipeline::Route;

use crate::engines::{BoxFuture, ModelChoice};
use crate::pipeline::{SmallCall, SmallModel, SmallModels};
use crate::state::{App, AppState};
use crate::trace::{self, SpanGuard};

/// `SmallModels` over the running service's settings and generator.
#[derive(Default)]
pub struct StateModels {
    /// Shared with the cross-encoders handed out, which may be built before the bind
    /// (the pipeline in Settings is built at start) and find the service through it.
    state: Arc<OnceLock<Weak<App>>>,
    /// The cross-encoders handed out, per model (loaded lazily).
    encoders: Mutex<HashMap<RerankerModel, Arc<dyn margin_embed::Reranker>>>,
}

impl StateModels {
    /// Binds the service once its `App` exists. A second bind is ignored.
    pub fn bind(&self, state: Weak<App>) {
        let _ = self.state.set(state);
    }

    /// The bound service, while it runs.
    fn app(&self) -> anyhow::Result<AppState> {
        self.state
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| anyhow::anyhow!("the service's small-tasks models aren't ready yet"))
    }

    async fn choice(&self, provider: Option<Provider>) -> anyhow::Result<(AppState, ModelChoice)> {
        let app = self.app()?;
        let choice = app
            .model_for(Route::Small, provider)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e.message))?;
        Ok((app, choice))
    }
}

/// A `model_call` row for one small call, with the job and the parent retrieval row.
fn small_call_span(
    app: &AppState,
    choice: &ModelChoice,
    version: &str,
    call: &SmallCall<'_>,
) -> SpanGuard {
    let mut span = SpanGuard::new(
        trace::model_call(
            app,
            call.trace_id,
            Route::Small,
            &choice.model,
            choice.config.provider.as_str(),
            version.to_string(),
        ),
        app,
    );
    span.row().job_id = call.job_id.map(str::to_string);
    span.row().parent_id = call.parent_id.map(str::to_string);
    span
}

/// Finishes `span` with the call's usage, or fails it with the error's code.
fn finish<T>(
    mut span: SpanGuard,
    result: Result<Generated<T>, margin_ai::AiError>,
) -> anyhow::Result<T> {
    match result {
        Ok(Generated { value, usage }) => {
            span.row().usage = Some(trace::trace_usage(&usage));
            span.finish(true);
            Ok(value)
        }
        Err(e) => {
            span.fail(trace::ai_error_code(&e));
            Err(e.into())
        }
    }
}

impl SmallModels for StateModels {
    fn small<'a>(
        &'a self,
        provider: Option<Provider>,
    ) -> BoxFuture<'a, anyhow::Result<SmallModel>> {
        Box::pin(async move {
            let (app, choice) = self.choice(provider).await?;
            Ok(SmallModel {
                model: app.generator.text_model(&choice.config),
                provider: choice.config.provider,
                id: choice.model,
                window: choice.window,
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
            let (app, choice) = self.choice(Some(small.provider)).await?;
            let span = small_call_span(&app, &choice, RERANK_VERSION, &call);
            let result = app.generator.rerank(&choice, query, candidates, opts).await;
            finish(span, result)
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
            let (app, choice) = self.choice(Some(small.provider)).await?;
            let span = small_call_span(&app, &choice, REWRITE_VERSION, &call);
            let result = app.generator.rewrite_queries(&choice, input, opts).await;
            finish(span, result)
        })
    }

    /// Before the service is bound (its pipeline is built at start, before the `App`)
    /// the download isn't checked, so a removed model never stops the service from
    /// starting: the first rerank then fails (traced `rerank_failed`, fused order kept).
    /// Once bound, a missing download is an error, so Settings refuses the variant.
    fn cross_encoder(
        &self,
        model: RerankerModel,
    ) -> anyhow::Result<Arc<dyn margin_embed::Reranker>> {
        if let Some(app) = self.state.get().and_then(Weak::upgrade) {
            let files = RerankerFiles::new(&app.config.models_dir(), model);
            anyhow::ensure!(
                files.is_complete(),
                "the reranker model \"{}\" isn't downloaded; download the reranker model in Settings → Sources first",
                model.name()
            );
        }
        let mut encoders = self.encoders.lock().unwrap();
        let encoder = encoders
            .entry(model)
            .or_insert_with(|| Arc::new(LazyEncoder::new(self.state.clone(), model)));
        Ok(encoder.clone())
    }
}

/// A cross-encoder opened on its first `score`, from the download it names, through the
/// service's `SourceTools::load_reranker`. A removed download is noticed then: the
/// model stays loaded until it unloads itself, and the next open fails.
struct LazyEncoder {
    model: RerankerModel,
    state: Arc<OnceLock<Weak<App>>>,
    loaded: Mutex<Option<Arc<dyn margin_embed::Reranker>>>,
}

impl LazyEncoder {
    fn new(state: Arc<OnceLock<Weak<App>>>, model: RerankerModel) -> Self {
        Self {
            model,
            state,
            loaded: Mutex::new(None),
        }
    }

    fn get(&self) -> Result<Arc<dyn margin_embed::Reranker>, EmbedError> {
        let mut loaded = self.loaded.lock().unwrap();
        if let Some(m) = loaded.as_ref() {
            return Ok(m.clone());
        }
        let Some(app) = self.state.get().and_then(Weak::upgrade) else {
            return Err(EmbedError::Load(
                "the service isn't ready to load the reranker model yet".into(),
            ));
        };
        let files = RerankerFiles::new(&app.config.models_dir(), self.model);
        if !files.is_complete() {
            return Err(EmbedError::Load(format!(
                "the reranker model \"{}\" isn't downloaded; download the reranker model in Settings → Sources first",
                self.model.name()
            )));
        }
        let m = app.source_tools.load_reranker(&files)?;
        *loaded = Some(m.clone());
        Ok(m)
    }
}

impl margin_embed::Reranker for LazyEncoder {
    fn name(&self) -> &str {
        self.model.name()
    }

    fn score(&self, query: &str, passages: &[String]) -> Result<Vec<f32>, EmbedError> {
        self.get()?.score(query, passages)
    }
}
