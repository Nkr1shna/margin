//! The service's seams to `margin-ai`, `margin-video` and `margin-embed`. Jobs call these
//! traits; the production implementations forward to the crates, and tests plug in fakes.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use std::time::{Duration, Instant};

use margin_ai::{
    AiError, Backoff, CallOptions, ClaudeClient, CoursePlan, Generated, InputPart, JsonFormat,
    LessonContext, ModelWindow, PlanRequest, Provider, ProviderConfig, ProviderKind, Ranked,
    RerankCandidate, ResponsesClient, Retrying, RewriteInput, SceneRequest, TextModel, TextOutput,
};
use margin_embed::{Chunk, EmbedError, Embedder, Extracted, ModelFiles, Transcript};
use margin_shared::Waiting;
use margin_shared::context::TraceRow;
use margin_shared::{SourceKind, ToolStatus};
use margin_video::{RenderConfig, RenderError, RenderJob, RenderOutput, RenderProgress};

use crate::pipeline::{Chunker, ChunkerCfg};
use margin_shared::sync::Locked;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Longer side of a picture sent to the lesson writer: enough for the Responses API's
/// high detail (which scales to 2048 px, then 768 px on the shorter side) without
/// sending more than it reads.
pub const WRITER_IMAGE_SIDE: usize = 1536;

/// The provider, model and window a call on one route uses (`AppState::model_for`,
/// `docs/agent-providers-brief.md` D10). `config.models.get(route)` is `model`. Its
/// `Debug` prints no credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    pub config: ProviderConfig,
    pub model: String,
    /// The model's context window and output cap (D13).
    pub window: ModelWindow,
}

/// The model work jobs need. Every call takes the `ModelChoice` of its route and its
/// `CallOptions` (the route, the prompt cache key, an output cap, the constant prefix's
/// length) and returns the answer with its usage, which the caller records as a
/// `model_call` trace row. Course outlines are the agent's `propose_course` arguments
/// (through `agent_model`); `plan_course` turns one into the planned outline with
/// per-lesson questions when the course has sources (§2.5).
pub trait Generator: Send + Sync + 'static {
    fn write_lesson<'a>(
        &'a self,
        m: &'a ModelChoice,
        ctx: &'a LessonContext,
        opts: &'a CallOptions,
        on_delta: Box<dyn FnMut(&str) + Send + 'a>,
    ) -> BoxFuture<'a, Result<Generated<String>, AiError>>;

    fn write_scene<'a>(
        &'a self,
        m: &'a ModelChoice,
        req: &'a SceneRequest,
        opts: &'a CallOptions,
    ) -> BoxFuture<'a, Result<Generated<String>, AiError>>;

    /// The outline planner: one strict-JSON call, checked against `req`.
    fn plan_course<'a>(
        &'a self,
        m: &'a ModelChoice,
        req: &'a PlanRequest,
        opts: &'a CallOptions,
    ) -> BoxFuture<'a, Result<Generated<CoursePlan>, AiError>>;

    /// The provider client the agent loop calls (one step per call; the model is the
    /// config's slot for the request's route).
    fn agent_model(&self, config: &ProviderConfig) -> Arc<dyn Provider>;

    /// The provider's plain text model, unretried: callers wrap it in
    /// `margin_ai::Retrying` (titles, condense summaries) or [`job_model`] (lessons,
    /// scenes, the planner).
    fn text_model(&self, config: &ProviderConfig) -> Arc<dyn TextModel>;

    /// The listwise rerank (R2, `"listwise-small"`): one strict-JSON call on the
    /// `Small` route grading `candidates` for `query`, every candidate once, best first,
    /// retried as the job running on this task says ([`job_model`]). Tests script it
    /// (`FakeGenerator.rerank_reply`).
    fn rerank<'a>(
        &'a self,
        m: &'a ModelChoice,
        query: &'a str,
        candidates: &'a [RerankCandidate],
        opts: &'a CallOptions,
    ) -> BoxFuture<'a, Result<Generated<Vec<Ranked>>, AiError>> {
        Box::pin(async move {
            let model = job_model(self.text_model(&m.config));
            margin_ai::rerank(&model, query, candidates, opts).await
        })
    }

    /// Query rewriting (R2, `"small-expand-v1"`): one strict-JSON call on the `Small`
    /// route for alternative phrasings of `input.query`. Tests script it
    /// (`FakeGenerator.rewrite_reply`).
    fn rewrite_queries<'a>(
        &'a self,
        m: &'a ModelChoice,
        input: &'a RewriteInput,
        opts: &'a CallOptions,
    ) -> BoxFuture<'a, Result<Generated<Vec<String>>, AiError>> {
        Box::pin(async move {
            let model = job_model(self.text_model(&m.config));
            margin_ai::rewrite_queries(&model, input, opts).await
        })
    }
}

pub trait Renderer: Send + Sync + 'static {
    fn detect_tools(&self) -> BoxFuture<'_, Vec<ToolStatus>>;

    fn render<'a>(
        &'a self,
        job: &'a RenderJob,
        config: &'a RenderConfig,
        on_progress: Box<dyn FnMut(RenderProgress) + Send + 'a>,
    ) -> BoxFuture<'a, Result<RenderOutput, RenderError>>;
}

/// `margin_ai` against the configured providers.
pub struct AiGenerator;

impl Generator for AiGenerator {
    fn write_lesson<'a>(
        &'a self,
        m: &'a ModelChoice,
        ctx: &'a LessonContext,
        opts: &'a CallOptions,
        on_delta: Box<dyn FnMut(&str) + Send + 'a>,
    ) -> BoxFuture<'a, Result<Generated<String>, AiError>> {
        Box::pin(async move {
            let model = job_model(self.text_model(&m.config));
            margin_ai::write_lesson_with(&model, ctx, opts, on_delta).await
        })
    }

    fn write_scene<'a>(
        &'a self,
        m: &'a ModelChoice,
        req: &'a SceneRequest,
        opts: &'a CallOptions,
    ) -> BoxFuture<'a, Result<Generated<String>, AiError>> {
        Box::pin(async move {
            let model = job_model(self.text_model(&m.config));
            margin_ai::write_scene_with(&model, req, opts).await
        })
    }

    fn plan_course<'a>(
        &'a self,
        m: &'a ModelChoice,
        req: &'a PlanRequest,
        opts: &'a CallOptions,
    ) -> BoxFuture<'a, Result<Generated<CoursePlan>, AiError>> {
        Box::pin(async move {
            let model = job_model(self.text_model(&m.config));
            margin_ai::plan_course(&model, req, opts).await
        })
    }

    fn agent_model(&self, config: &ProviderConfig) -> Arc<dyn Provider> {
        match config.provider {
            ProviderKind::OpenAi | ProviderKind::ChatGpt => {
                Arc::new(ResponsesClient::from_config(config))
            }
            ProviderKind::Claude => Arc::new(claude_client(config)),
        }
    }

    fn text_model(&self, config: &ProviderConfig) -> Arc<dyn TextModel> {
        match config.provider {
            ProviderKind::OpenAi | ProviderKind::ChatGpt => {
                Arc::new(ResponsesClient::from_config(config))
            }
            ProviderKind::Claude => Arc::new(claude_client(config)),
        }
    }
}

fn claude_client(config: &ProviderConfig) -> ClaudeClient {
    ClaudeClient::new(
        config.base_url.clone(),
        config.credential.secret(),
        config.models.clone(),
    )
}

// ---------------------------------------------------------------------------
// Job calls' retries (`docs/phase5-brief.md` D4)
// ---------------------------------------------------------------------------

/// Where a job shows a wait before a retry (`Some`) and its end (`None`).
pub type JobWait = Arc<dyn Fn(Option<Waiting>) + Send + Sync>;

/// Records a failed attempt that is being retried: its error code and how long it
/// took (a `model_call` row of its own; the call's last attempt is the caller's row).
pub type FailedAttempt = Arc<dyn Fn(&'static str, Duration) + Send + Sync>;

/// How the model calls of the job running on this task are retried and where their
/// waits and failed attempts go. Jobs run their `Generator` call inside
/// [`JobCalls::run`]; `AiGenerator` wraps its text model with [`job_model`], which
/// reads it. Outside a job, calls are retried with the default policy, silently.
#[derive(Clone)]
pub struct JobCalls {
    pub backoff: Backoff,
    pub on_wait: JobWait,
    pub on_failed: FailedAttempt,
    /// The caller's row's clock (`SpanGuard::clock`): restarted when a retried attempt
    /// begins, and given the provider's time on the last attempt (`provider_ms`).
    pub clock: Option<Arc<crate::trace::AttemptClock>>,
}

tokio::task_local! {
    static JOB_CALLS: JobCalls;
}

impl JobCalls {
    /// Retries as `backoff` says, showing nothing and tracing nothing.
    pub fn silent(backoff: Backoff) -> Self {
        Self {
            backoff,
            on_wait: Arc::new(|_| {}),
            on_failed: Arc::new(|_, _| {}),
            clock: None,
        }
    }

    /// Runs `job` with these settings for the model calls it makes.
    pub async fn run<F: Future>(self, job: F) -> F::Output {
        JOB_CALLS.scope(self, job).await
    }

    /// The settings of the job running on this task, else silent defaults.
    fn current() -> Self {
        JOB_CALLS
            .try_with(Clone::clone)
            .unwrap_or_else(|_| Self::silent(Backoff::default()))
    }
}

/// `model` with its calls retried as the job running on this task says
/// ([`JobCalls`]): a call failing before any delta (429 other than the limits waiting
/// doesn't cure, 5xx incl. 529, network) is retried after the pause the API asked for
/// (uncapped) or the computed backoff, each wait shown where the job is seen and each
/// failed attempt traced.
pub fn job_model(model: Arc<dyn TextModel>) -> Retrying<Attempts> {
    let calls = JobCalls::current();
    let last = Arc::new(std::sync::Mutex::new(None));
    let inner = Attempts {
        inner: model,
        last: last.clone(),
        clock: calls.clock,
        attempts: std::sync::atomic::AtomicU32::new(0),
    };
    let (on_wait, on_failed) = (calls.on_wait, calls.on_failed);
    Retrying::new(
        inner,
        calls.backoff,
        Box::new(move |wait| {
            // A wait starts: the attempt before it failed and is retried.
            if wait.is_some()
                && let Some((code, took)) = last.locked().take()
            {
                on_failed(code, took);
            }
            on_wait(wait);
        }),
    )
}

/// A text model that remembers its last failed call (error code, duration) for
/// [`job_model`]'s trace row of a retried attempt.
pub struct Attempts {
    inner: Arc<dyn TextModel>,
    last: Arc<std::sync::Mutex<Option<(&'static str, Duration)>>>,
    clock: Option<Arc<crate::trace::AttemptClock>>,
    attempts: std::sync::atomic::AtomicU32,
}

impl Attempts {
    /// An attempt begins: a retried one restarts the caller's row's timer.
    fn begin(&self) -> Instant {
        let n = self
            .attempts
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n > 0
            && let Some(clock) = &self.clock
        {
            clock.retry_started();
        }
        Instant::now()
    }

    fn note<T>(&self, started: Instant, result: &Result<T, AiError>) {
        if let Some(clock) = &self.clock {
            clock.provider_took(started.elapsed());
        }
        if let Err(e) = result {
            *self.last.locked() = Some((crate::trace::ai_error_code(e), started.elapsed()));
        }
    }
}

impl TextModel for Attempts {
    fn stream_parts<'a>(
        &'a self,
        instructions: &'a str,
        input: &'a [InputPart],
        opts: &'a CallOptions,
        on_delta: Box<dyn FnMut(&str) + Send + 'a>,
    ) -> margin_ai::model::BoxFuture<'a, Result<TextOutput, AiError>> {
        Box::pin(async move {
            let started = self.begin();
            let result = self
                .inner
                .stream_parts(instructions, input, opts, on_delta)
                .await;
            self.note(started, &result);
            result
        })
    }

    fn stream_json<'a>(
        &'a self,
        instructions: &'a str,
        input: &'a [InputPart],
        format: &'a JsonFormat,
        opts: &'a CallOptions,
    ) -> margin_ai::model::BoxFuture<'a, Result<TextOutput, AiError>> {
        Box::pin(async move {
            let started = self.begin();
            let result = self
                .inner
                .stream_json(instructions, input, format, opts)
                .await;
            self.note(started, &result);
            result
        })
    }
}

/// Records each retried attempt as a `model_call` row like `template` (the call's own
/// row: route, model, provider, prompt version, job or chat), failed with its code.
pub fn failed_attempts(state: &crate::state::AppState, template: TraceRow) -> FailedAttempt {
    let state = state.clone();
    Arc::new(move |code, took| {
        let mut row = template.clone();
        row.id = crate::trace::new_row_id();
        row.at = chrono::Utc::now()
            - chrono::TimeDelta::from_std(took).unwrap_or(chrono::TimeDelta::zero());
        row.duration_ms = took.as_secs_f64() * 1000.0;
        row.ok = false;
        row.error_code = Some(code.to_string());
        row.usage = None;
        row.stop_reason = None;
        state.tracer.record(row);
    })
}

/// A [`JobWait`] that hands each report, in order, to `show` on a task of its own (so
/// it can take the write lock); the task ends with the job's last report.
pub fn ordered_waits<F, Fut>(show: F) -> JobWait
where
    F: Fn(Option<Waiting>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(wait) = rx.recv().await {
            show(wait).await;
        }
    });
    Arc::new(move |wait| {
        let _ = tx.send(wait);
    })
}

/// A wait in words for a job's progress text: "Waiting for the model until 14:03 UTC.
/// The model API asked Margin to slow down."
pub fn waiting_line(wait: &Waiting) -> String {
    format!(
        "Waiting for the model until {} UTC. {}",
        wait.until.format("%H:%M"),
        wait.reason
    )
}

/// `margin_video`'s ManimGX + `margin-tts` pipeline.
pub struct VideoRenderer;

impl Renderer for VideoRenderer {
    /// The source readers' rows (tesseract, ffmpeg, pdfium) say where `margin_embed`
    /// finds them, which also looks in the service's tools folder.
    fn detect_tools(&self) -> BoxFuture<'_, Vec<ToolStatus>> {
        Box::pin(margin_video::detect_tools_with(reader_tools()))
    }

    fn render<'a>(
        &'a self,
        job: &'a RenderJob,
        config: &'a RenderConfig,
        on_progress: Box<dyn FnMut(RenderProgress) + Send + 'a>,
    ) -> BoxFuture<'a, Result<RenderOutput, RenderError>> {
        Box::pin(margin_video::render(job, config, on_progress))
    }
}

/// Opens the ONNX cross-encoder in `files` (`SourceTools::load_reranker`'s default).
pub fn load_onnx_reranker(
    files: &margin_embed::RerankerFiles,
) -> Result<Arc<dyn margin_embed::Reranker>, EmbedError> {
    let m = margin_embed::onnx::OnnxReranker::load(files, &margin_embed::OnnxOptions::default())?;
    Ok(Arc::new(m))
}

/// Where `margin_embed`'s readers find their programs and library.
pub fn reader_tools() -> margin_video::ReaderTools {
    margin_video::ReaderTools {
        tesseract: margin_embed::find_tesseract(),
        ffmpeg: margin_embed::find_ffmpeg().map(|f| (f.ffmpeg, f.ffprobe)),
        pdfium: margin_embed::find_pdfium(),
    }
}

/// The service's seam to `margin-embed`: the embedding model's files, loading it, and
/// reading, cutting and ranking sources. `EmbedTools` forwards to the crate; tests plug
/// in fakes (with a fake Hugging Face server).
pub trait SourceTools: Send + Sync + 'static {
    /// Whether a finished download is in `files`.
    fn is_complete(&self, files: &ModelFiles) -> bool;
    /// Bytes the downloaded model takes on disk.
    fn size_on_disk(&self, files: &ModelFiles) -> u64;
    /// Bytes a download of `files` (its backend's files) would fetch.
    fn remote_size<'a>(
        &'a self,
        files: &'a ModelFiles,
        http: &'a reqwest::Client,
        hf_base: &'a str,
    ) -> BoxFuture<'a, Result<u64, EmbedError>>;
    /// Downloads the model into `files`, calling `progress(received, total)`.
    fn download<'a>(
        &'a self,
        files: &'a ModelFiles,
        http: &'a reqwest::Client,
        hf_base: &'a str,
        progress: &'a (dyn Fn(u64, u64) + Send + Sync),
    ) -> BoxFuture<'a, Result<(), EmbedError>>;
    /// Deletes the downloaded model.
    fn remove(&self, files: &ModelFiles) -> Result<(), EmbedError>;
    /// Loads the downloaded model. Blocking.
    fn load(&self, files: &ModelFiles) -> Result<Arc<dyn Embedder>, EmbedError>;
    /// Reads a source file: its text, its pictures (rendered pages and video frames are
    /// written into `media_dir` as `page_file`/`frame_file`) and its sound. Blocking.
    fn extract(
        &self,
        path: &Path,
        kind: SourceKind,
        media_dir: &Path,
    ) -> Result<Extracted, EmbedError>;
    /// `extract`, except that a PDF's pages aren't rendered (its `pictures` is empty):
    /// ingest renders each page through `pdf_page_picture`, which keeps the render in
    /// `media_dir` for retrieval and citations. Blocking.
    fn extract_lazy(
        &self,
        path: &Path,
        kind: SourceKind,
        media_dir: &Path,
    ) -> Result<Extracted, EmbedError> {
        self.extract(path, kind, media_dir)
    }
    /// What is said in 16 kHz mono `samples`, word by word, using `work_dir` for
    /// temporary files. `Ok(None)` when no speech recognizer is installed (or its model
    /// isn't downloaded); `no_transcriber` says what to do about it. Blocking.
    fn transcribe(
        &self,
        samples: &[f32],
        work_dir: &Path,
    ) -> Result<Option<Transcript>, EmbedError>;
    /// Why `transcribe` gives `Ok(None)`: what is missing and where to get it.
    fn no_transcriber(&self) -> String {
        "the speech model isn't downloaded (Settings → Sources downloads it)".to_string()
    }
    /// The speech model this service downloads and transcribes with (Parakeet on ONNX
    /// Runtime); `None` for a test fake without one.
    fn speech_model(&self) -> Option<&dyn SpeechModel> {
        None
    }
    /// Why pictures (images, scanned PDF pages) are read without their text on this
    /// service, when they are: no OCR program installed. `None` when OCR works.
    fn ocr_missing(&self) -> Option<String> {
        None
    }
    /// Unloads what has been unused for a while (the speech model). The service calls it
    /// every minute. Blocking.
    fn sweep(&self) {}
    /// Opens a downloaded reranker model (R2, the `"onnx-crossencoder"` reranker; the
    /// caller has checked `files.is_complete()`). The ONNX cross-encoder where this
    /// build has the runtime; tests stand in `margin_embed::fake::FakeReranker`.
    /// Blocking.
    fn load_reranker(
        &self,
        files: &margin_embed::RerankerFiles,
    ) -> Result<Arc<dyn margin_embed::Reranker>, EmbedError> {
        load_onnx_reranker(files)
    }
    /// An image file as the lesson writer is shown it: (media type, bytes), at most
    /// `WRITER_IMAGE_SIDE` px. Blocking.
    fn image_for_model(&self, path: &Path) -> Result<(String, Vec<u8>), EmbedError>;
    /// Cuts extracted text into chunks of at most `cfg.max_tokens` with the pipeline's
    /// chunker, picked by name in Settings (`chunker.name()`: "sections-v2",
    /// "pages-v1", …).
    fn chunk(
        &self,
        chunker: &dyn Chunker,
        doc: &Extracted,
        kind: SourceKind,
        count_tokens: &dyn Fn(&str) -> usize,
        cfg: &ChunkerCfg,
    ) -> Vec<Chunk>;
    /// Page `page` (from 1) of the PDF at `pdf` as a picture, rendered into `media_dir`
    /// as `margin_embed::page_file(page)` the first time it is asked for and served from
    /// there after: pages are rendered when retrieval or a citation first needs them,
    /// not at ingest. Blocking.
    fn pdf_page_picture(
        &self,
        pdf: &Path,
        page: u32,
        media_dir: &Path,
    ) -> Result<PathBuf, EmbedError>;
    /// How well a chunk matches a query; higher is better.
    fn score(
        &self,
        query_vec: &[f32],
        query_text: &str,
        chunk_vec: &[f32],
        chunk_text: &str,
    ) -> f32;
}

/// A speech model the service downloads (`SourceTools::speech_model`), shown in Settings
/// → Sources beside the embedding model.
pub trait SpeechModel: Send + Sync {
    /// Its Hugging Face repo.
    fn repo(&self) -> String;
    /// Whether a finished download is on disk. Blocking.
    fn is_complete(&self) -> bool;
    /// Bytes the download takes on disk. Blocking.
    fn size_on_disk(&self) -> u64;
    /// Bytes a download would fetch.
    fn remote_size<'a>(
        &'a self,
        http: &'a reqwest::Client,
        hf_base: &'a str,
    ) -> BoxFuture<'a, Result<u64, EmbedError>>;
    /// Downloads it, calling `progress(received, total)`.
    fn download<'a>(
        &'a self,
        http: &'a reqwest::Client,
        hf_base: &'a str,
        progress: &'a (dyn Fn(u64, u64) + Send + Sync),
    ) -> BoxFuture<'a, Result<(), EmbedError>>;
    /// Unloads it and deletes its files. Blocking.
    fn remove(&self) -> Result<(), EmbedError>;
}

/// `margin_embed`: EmbeddingGemma 2 on the configured ONNX backend and the readers.
/// Speech is recognized in-process by Parakeet (`margin_embed::transcript_parakeet`),
/// whose model is downloaded into `data_dir/models` like the embedder's.
pub struct EmbedTools {
    speech: ParakeetModel,
}

impl EmbedTools {
    /// The tools for a service keeping its models in `models_dir` (`data_dir/models`).
    pub fn new(models_dir: &Path) -> Self {
        Self {
            speech: ParakeetModel::new(models_dir),
        }
    }
}

/// Parakeet TDT 0.6B v3 (int8) on ONNX Runtime: loaded on first use, dropped after
/// `SPEECH_IDLE` unused.
struct ParakeetModel {
    parakeet: margin_embed::transcript_parakeet::Parakeet,
}

impl ParakeetModel {
    /// Threads the recognizer runs on: half the cores (at least 1, at most 4), so
    /// transcribing leaves room for the embedder and the service.
    fn threads() -> usize {
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        (cores / 2).clamp(1, 4)
    }

    fn new(models_dir: &Path) -> Self {
        use margin_embed::transcript_parakeet::{Parakeet, SpeechDtype, SpeechModelFiles};
        let files = SpeechModelFiles::in_models_dir(models_dir, SpeechDtype::Int8);
        Self {
            parakeet: Parakeet::new(files, Self::threads()),
        }
    }

    fn files(&self) -> &margin_embed::transcript_parakeet::SpeechModelFiles {
        self.parakeet.files()
    }
}

impl SpeechModel for ParakeetModel {
    fn repo(&self) -> String {
        margin_embed::transcript_parakeet::PARAKEET_REPO.to_string()
    }

    fn is_complete(&self) -> bool {
        self.files().is_complete()
    }

    fn size_on_disk(&self) -> u64 {
        self.files().size_on_disk()
    }

    fn remote_size<'a>(
        &'a self,
        http: &'a reqwest::Client,
        hf_base: &'a str,
    ) -> BoxFuture<'a, Result<u64, EmbedError>> {
        Box::pin(self.files().remote_bytes(http, hf_base))
    }

    fn download<'a>(
        &'a self,
        http: &'a reqwest::Client,
        hf_base: &'a str,
        progress: &'a (dyn Fn(u64, u64) + Send + Sync),
    ) -> BoxFuture<'a, Result<(), EmbedError>> {
        Box::pin(self.files().download(http, hf_base, progress))
    }

    fn remove(&self) -> Result<(), EmbedError> {
        // Unloaded first: `sweep` drops it only when idle, so it is forced here.
        self.parakeet.unload();
        self.files().remove()
    }
}

impl SourceTools for EmbedTools {
    fn is_complete(&self, files: &ModelFiles) -> bool {
        files.is_complete()
    }

    fn size_on_disk(&self, files: &ModelFiles) -> u64 {
        files.size_on_disk()
    }

    fn remote_size<'a>(
        &'a self,
        files: &'a ModelFiles,
        http: &'a reqwest::Client,
        hf_base: &'a str,
    ) -> BoxFuture<'a, Result<u64, EmbedError>> {
        Box::pin(files.remote_bytes(http, hf_base))
    }

    fn download<'a>(
        &'a self,
        files: &'a ModelFiles,
        http: &'a reqwest::Client,
        hf_base: &'a str,
        progress: &'a (dyn Fn(u64, u64) + Send + Sync),
    ) -> BoxFuture<'a, Result<(), EmbedError>> {
        Box::pin(files.download(http, hf_base, progress))
    }

    fn remove(&self, files: &ModelFiles) -> Result<(), EmbedError> {
        files.remove()
    }

    /// The backend `files.backend` names (`service.toml`'s `embedder`).
    fn load(&self, files: &ModelFiles) -> Result<Arc<dyn Embedder>, EmbedError> {
        margin_embed::load_embedder(files)
    }

    fn extract(
        &self,
        path: &Path,
        kind: SourceKind,
        media_dir: &Path,
    ) -> Result<Extracted, EmbedError> {
        margin_embed::extract(path, kind, media_dir)
    }

    fn extract_lazy(
        &self,
        path: &Path,
        kind: SourceKind,
        media_dir: &Path,
    ) -> Result<Extracted, EmbedError> {
        margin_embed::extract_lazy(path, kind, media_dir)
    }

    /// Parakeet in-process, once its model is downloaded.
    fn transcribe(
        &self,
        samples: &[f32],
        _work_dir: &Path,
    ) -> Result<Option<Transcript>, EmbedError> {
        if !self.speech.is_complete() {
            return Ok(None);
        }
        self.speech.parakeet.transcribe(samples).map(Some)
    }

    fn speech_model(&self) -> Option<&dyn SpeechModel> {
        Some(&self.speech)
    }

    fn ocr_missing(&self) -> Option<String> {
        margin_embed::find_tesseract().is_none().then(|| {
            "tesseract isn't installed on the service (Settings → Tools shows how to install it)"
                .to_string()
        })
    }

    fn sweep(&self) {
        if self.speech.parakeet.sweep() {
            tracing::info!("the speech model was unloaded after being unused");
        }
    }

    fn image_for_model(&self, path: &Path) -> Result<(String, Vec<u8>), EmbedError> {
        let jpeg = margin_embed::jpeg_for_model(path, WRITER_IMAGE_SIDE)?;
        Ok(("image/jpeg".to_string(), jpeg))
    }

    fn chunk(
        &self,
        chunker: &dyn Chunker,
        doc: &Extracted,
        kind: SourceKind,
        count_tokens: &dyn Fn(&str) -> usize,
        cfg: &ChunkerCfg,
    ) -> Vec<Chunk> {
        chunker.chunk(doc, kind, count_tokens, cfg)
    }

    /// Rendered once into `media_dir` and served from there after.
    fn pdf_page_picture(
        &self,
        pdf: &Path,
        page: u32,
        media_dir: &Path,
    ) -> Result<PathBuf, EmbedError> {
        let target = media_dir.join(margin_embed::page_file(page));
        if target.is_file() {
            return Ok(target);
        }
        std::fs::create_dir_all(media_dir)?;
        // Written beside the target, then moved, so a half-written page is never served.
        let partial = media_dir.join(format!(".{}.part", uuid::Uuid::new_v4()));
        let result = margin_embed::pdf_page_picture(pdf, page, &partial)
            .and_then(|()| std::fs::rename(&partial, &target).map_err(EmbedError::from));
        if result.is_err() {
            let _ = std::fs::remove_file(&partial);
        }
        result.map(|()| target)
    }

    fn score(
        &self,
        query_vec: &[f32],
        query_text: &str,
        chunk_vec: &[f32],
        chunk_text: &str,
    ) -> f32 {
        margin_embed::score(query_vec, query_text, chunk_vec, chunk_text)
    }
}

/// `margin-service download-model`: the embedding model downloaded from the command line,
/// for a box without the app (the Linux provisioning script runs it). The same download
/// as Settings → Sources, into the same folder (`Config::model_files`).
pub mod model_download {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use anyhow::Context;
    use margin_embed::{Backend, ModelFiles};
    use margin_shared::sync::Locked;

    use super::{SourceTools, SpeechModel};
    use crate::config::Config;

    /// Progress lines are printed at most this often.
    const PRINT_INTERVAL: Duration = Duration::from_secs(2);

    /// `[--embedder NAME]`: the backend to download instead of the configured one.
    pub fn parse_args(args: &[String]) -> Result<Option<Backend>, String> {
        let mut embedder = None;
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let value = match a.as_str() {
                "--embedder" => it
                    .next()
                    .cloned()
                    .ok_or_else(|| "--embedder needs a name".to_string())?,
                other => match other.strip_prefix("--embedder=") {
                    Some(v) => v.to_string(),
                    None => return Err(format!("unknown option {other}")),
                },
            };
            embedder = Some(value.parse::<Backend>().map_err(|e| e.to_string())?);
        }
        Ok(embedder)
    }

    /// The files to download: the configured backend's, or `embedder`'s (into its own
    /// folder under `data_dir/models`).
    pub fn files_for(config: &Config, embedder: Option<Backend>) -> anyhow::Result<ModelFiles> {
        let backend = match embedder {
            Some(b) => b,
            None => config.embedder_backend()?,
        };
        Ok(ModelFiles::with_backend(
            config.data_dir.join("models").join(backend.dir_name()),
            backend,
        ))
    }

    /// Downloads `files` from `hf_base` unless they are complete, reporting progress
    /// lines through `report`.
    pub async fn run(
        tools: &dyn SourceTools,
        files: &ModelFiles,
        hf_base: &str,
        report: &(dyn Fn(&str) + Send + Sync),
    ) -> anyhow::Result<()> {
        let name = files.backend.label();
        if tools.is_complete(files) {
            report(&format!(
                "{name} is already downloaded in {} ({}).",
                files.dir.display(),
                mb(tools.size_on_disk(files))
            ));
            return Ok(());
        }
        let http = http_client()?;
        report(&format!(
            "Downloading {name} from {} into {}…",
            files.backend.repo(),
            files.dir.display()
        ));
        let progress = Printer::new(report);
        let progress = |received: u64, total: u64| progress.print(received, total);
        tools
            .download(files, &http, hf_base, &progress)
            .await
            .with_context(|| format!("couldn't download {name}"))?;
        anyhow::ensure!(
            tools.is_complete(files),
            "the download of {name} finished but isn't complete in {}",
            files.dir.display()
        );
        report(&format!(
            "{name} is downloaded ({}). The service loads it on first use.",
            mb(tools.size_on_disk(files))
        ));
        Ok(())
    }

    /// Downloads the speech model (`SourceTools::speech_model`) unless it is complete,
    /// reporting progress lines through `report`.
    pub async fn run_speech(
        model: &dyn SpeechModel,
        hf_base: &str,
        report: &(dyn Fn(&str) + Send + Sync),
    ) -> anyhow::Result<()> {
        let repo = model.repo();
        if model.is_complete() {
            report(&format!(
                "The speech model {repo} is already downloaded ({}).",
                mb(model.size_on_disk())
            ));
            return Ok(());
        }
        let http = http_client()?;
        report(&format!("Downloading the speech model {repo}…"));
        let progress = Printer::new(report);
        let progress = |received: u64, total: u64| progress.print(received, total);
        model
            .download(&http, hf_base, &progress)
            .await
            .with_context(|| format!("couldn't download the speech model {repo}"))?;
        anyhow::ensure!(
            model.is_complete(),
            "the download of the speech model {repo} finished but isn't complete"
        );
        report(&format!(
            "The speech model is downloaded ({}). The service loads it when it first transcribes.",
            mb(model.size_on_disk())
        ));
        Ok(())
    }

    fn http_client() -> anyhow::Result<reqwest::Client> {
        Ok(reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(60))
            .user_agent(concat!("margin-service/", env!("CARGO_PKG_VERSION")))
            .build()?)
    }

    /// Prints download progress at most every `PRINT_INTERVAL`, and at the end.
    struct Printer<'a> {
        report: &'a (dyn Fn(&str) + Send + Sync),
        last: Mutex<Option<Instant>>,
    }

    impl<'a> Printer<'a> {
        fn new(report: &'a (dyn Fn(&str) + Send + Sync)) -> Self {
            Self {
                report,
                last: Mutex::new(None),
            }
        }

        fn print(&self, received: u64, total: u64) {
            let mut last = self.last.locked();
            let due = last.is_none_or(|t| t.elapsed() >= PRINT_INTERVAL);
            if due || (total > 0 && received >= total) {
                *last = Some(Instant::now());
                if total > 0 {
                    (self.report)(&format!(
                        "  {} of {} ({}%)",
                        mb(received),
                        mb(total),
                        received * 100 / total
                    ));
                } else {
                    (self.report)(&format!("  {}", mb(received)));
                }
            }
        }
    }

    fn mb(bytes: u64) -> String {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    }

    #[cfg(test)]
    mod tests {
        use std::path::{Path, PathBuf};
        use std::sync::Arc;

        use margin_embed::{Chunk, EmbedError, Embedder, Extracted, OnnxDtype, Transcript};
        use margin_shared::SourceKind;

        use super::*;
        use crate::engines::BoxFuture;
        use crate::pipeline::{Chunker, ChunkerCfg};

        /// Writes one file of 10 bytes as "the model", in two progress steps.
        struct Fake;

        impl SourceTools for Fake {
            fn is_complete(&self, files: &ModelFiles) -> bool {
                files.dir.join("model").is_file()
            }
            fn size_on_disk(&self, files: &ModelFiles) -> u64 {
                std::fs::metadata(files.dir.join("model")).map_or(0, |m| m.len())
            }
            fn remote_size<'a>(
                &'a self,
                _: &'a ModelFiles,
                _: &'a reqwest::Client,
                _: &'a str,
            ) -> BoxFuture<'a, Result<u64, EmbedError>> {
                Box::pin(async { Ok(10) })
            }
            fn download<'a>(
                &'a self,
                files: &'a ModelFiles,
                _: &'a reqwest::Client,
                hf_base: &'a str,
                progress: &'a (dyn Fn(u64, u64) + Send + Sync),
            ) -> BoxFuture<'a, Result<(), EmbedError>> {
                Box::pin(async move {
                    if hf_base == "fail" {
                        return Err(EmbedError::Download("offline".into()));
                    }
                    progress(5, 10);
                    std::fs::create_dir_all(&files.dir)?;
                    std::fs::write(files.dir.join("model"), b"0123456789")?;
                    progress(10, 10);
                    Ok(())
                })
            }
            fn remove(&self, _: &ModelFiles) -> Result<(), EmbedError> {
                unreachable!()
            }
            fn load(&self, _: &ModelFiles) -> Result<Arc<dyn Embedder>, EmbedError> {
                unreachable!()
            }
            fn extract(&self, _: &Path, _: SourceKind, _: &Path) -> Result<Extracted, EmbedError> {
                unreachable!()
            }
            fn transcribe(&self, _: &[f32], _: &Path) -> Result<Option<Transcript>, EmbedError> {
                unreachable!()
            }
            fn image_for_model(&self, _: &Path) -> Result<(String, Vec<u8>), EmbedError> {
                unreachable!()
            }
            fn chunk(
                &self,
                _: &dyn Chunker,
                _: &Extracted,
                _: SourceKind,
                _: &dyn Fn(&str) -> usize,
                _: &ChunkerCfg,
            ) -> Vec<Chunk> {
                unreachable!()
            }
            fn pdf_page_picture(&self, _: &Path, _: u32, _: &Path) -> Result<PathBuf, EmbedError> {
                unreachable!()
            }
            fn score(&self, _: &[f32], _: &str, _: &[f32], _: &str) -> f32 {
                unreachable!()
            }
        }

        /// A speech model of 20 bytes, kept in memory.
        #[derive(Default)]
        struct FakeSpeech(std::sync::atomic::AtomicBool);

        impl SpeechModel for FakeSpeech {
            fn repo(&self) -> String {
                "speech/repo".into()
            }
            fn is_complete(&self) -> bool {
                self.0.load(std::sync::atomic::Ordering::SeqCst)
            }
            fn size_on_disk(&self) -> u64 {
                if self.is_complete() { 20 } else { 0 }
            }
            fn remote_size<'a>(
                &'a self,
                _: &'a reqwest::Client,
                _: &'a str,
            ) -> BoxFuture<'a, Result<u64, EmbedError>> {
                Box::pin(async { Ok(20) })
            }
            fn download<'a>(
                &'a self,
                _: &'a reqwest::Client,
                hf_base: &'a str,
                progress: &'a (dyn Fn(u64, u64) + Send + Sync),
            ) -> BoxFuture<'a, Result<(), EmbedError>> {
                Box::pin(async move {
                    if hf_base == "fail" {
                        return Err(EmbedError::Download("offline".into()));
                    }
                    progress(20, 20);
                    self.0.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
            }
            fn remove(&self) -> Result<(), EmbedError> {
                unreachable!()
            }
        }

        #[tokio::test]
        async fn the_speech_model_downloads_once_and_reports() {
            let speech = FakeSpeech::default();
            let lines = Mutex::new(Vec::<String>::new());
            let report = |l: &str| lines.lock().unwrap().push(l.to_string());
            let err = run_speech(&speech, "fail", &report).await.unwrap_err();
            assert!(format!("{err:#}").contains("offline"), "{err:#}");
            run_speech(&speech, "http://hf", &report).await.unwrap();
            let out = lines.lock().unwrap().join("\n");
            assert!(
                out.contains("Downloading the speech model speech/repo"),
                "{out}"
            );
            assert!(out.contains("100%"), "{out}");
            assert!(out.contains("is downloaded (0.0 MB)"), "{out}");
            run_speech(&speech, "fail", &report).await.unwrap();
            assert!(
                lines
                    .lock()
                    .unwrap()
                    .last()
                    .unwrap()
                    .contains("already downloaded")
            );
        }

        #[test]
        fn arguments_name_a_backend() {
            let args =
                |a: &[&str]| parse_args(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
            assert_eq!(args(&[]), Ok(None));
            assert_eq!(
                args(&["--embedder", "onnx-q8"]),
                Ok(Some(Backend::Onnx(OnnxDtype::Q8)))
            );
            assert_eq!(
                args(&["--embedder=onnx-fp32"]),
                Ok(Some(Backend::Onnx(OnnxDtype::Fp32)))
            );
            assert!(
                args(&["--embedder=coreml"])
                    .unwrap_err()
                    .contains("onnx-q8")
            );
            assert!(args(&["--embedder"]).unwrap_err().contains("needs a name"));
            assert!(args(&["--embedder", "tf"]).unwrap_err().contains("onnx-q8"));
            assert!(args(&["--force"]).unwrap_err().contains("unknown option"));
        }

        #[test]
        fn files_are_the_configured_backends_or_the_named_one() {
            let c = Config {
                data_dir: PathBuf::from("/data"),
                ..Config::default()
            };
            let f = files_for(&c, None).unwrap();
            assert_eq!(f.backend, Backend::default());
            assert_eq!(f.dir, c.model_files().dir);
            for b in Backend::ALL {
                let f = files_for(&c, Some(b)).unwrap();
                assert_eq!(f.dir, Path::new("/data/models").join(b.dir_name()));
            }
        }

        #[tokio::test]
        async fn it_downloads_once_and_reports() {
            let dir = std::env::temp_dir().join(format!("margin-dl-{}", uuid::Uuid::new_v4()));
            let files = ModelFiles::with_backend(dir.join("m"), Backend::default());
            let lines = Mutex::new(Vec::<String>::new());
            let report = |l: &str| lines.lock().unwrap().push(l.to_string());
            assert!(run(&Fake, &files, "fail", &report).await.is_err());
            run(&Fake, &files, "http://hf", &report).await.unwrap();
            let out = lines.lock().unwrap().join("\n");
            assert!(out.contains("Downloading"), "{out}");
            assert!(out.contains("100%"), "{out}");
            assert!(out.contains("is downloaded (0.0 MB)"), "{out}");
            run(&Fake, &files, "fail", &report).await.unwrap();
            assert!(
                lines
                    .lock()
                    .unwrap()
                    .last()
                    .unwrap()
                    .contains("already downloaded"),
                "a complete download isn't fetched again"
            );
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}
