//! The service's seams to `margin-ai`, `margin-video` and `margin-embed`. Jobs call these
//! traits; the production implementations forward to the crates, and tests plug in fakes.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use margin_agent::ModelClient;
use margin_ai::{AiError, LessonContext, ResponsesClient, SceneRequest};
use margin_embed::{Chunk, EmbedError, Embedder, Extracted, ModelFiles, Transcript};
use margin_shared::{SourceKind, ToolStatus};
use margin_video::{RenderConfig, RenderError, RenderJob, RenderOutput, RenderProgress};

use crate::pipeline::{Chunker, ChunkerCfg};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Longer side of a picture sent to the lesson writer: enough for the Responses API's
/// high detail (which scales to 2048 px, then 768 px on the shorter side) without
/// sending more than it reads.
pub const WRITER_IMAGE_SIDE: usize = 1536;

/// Where and how to call the Responses API. Built per job from Settings + Keychain.
#[derive(Clone)]
pub struct AiConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

impl std::fmt::Debug for AiConfig {
    // Never print the key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiConfig")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .finish()
    }
}

/// The model work jobs need. Course outlines aren't here: the agent writes them as
/// `propose_course` arguments in a chat turn (through `agent_model`).
pub trait Generator: Send + Sync + 'static {
    fn write_lesson<'a>(
        &'a self,
        ai: &'a AiConfig,
        ctx: &'a LessonContext,
        on_delta: Box<dyn FnMut(&str) + Send + 'a>,
    ) -> BoxFuture<'a, Result<String, AiError>>;

    fn write_scene<'a>(
        &'a self,
        ai: &'a AiConfig,
        req: &'a SceneRequest,
    ) -> BoxFuture<'a, Result<String, AiError>>;

    /// The model the agent loop calls for a chat turn. The turn picks the agent model
    /// (Settings' `agent_model`, else `ai.model`) per request.
    fn agent_model(&self, ai: &AiConfig) -> Arc<dyn ModelClient>;
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

/// `margin_ai` against the configured Responses API.
pub struct AiGenerator;

fn client(ai: &AiConfig) -> ResponsesClient {
    ResponsesClient::new(ai.base_url.clone(), ai.api_key.clone(), ai.model.clone())
}

impl Generator for AiGenerator {
    fn write_lesson<'a>(
        &'a self,
        ai: &'a AiConfig,
        ctx: &'a LessonContext,
        on_delta: Box<dyn FnMut(&str) + Send + 'a>,
    ) -> BoxFuture<'a, Result<String, AiError>> {
        Box::pin(async move { margin_ai::write_lesson(&client(ai), ctx, on_delta).await })
    }

    fn write_scene<'a>(
        &'a self,
        ai: &'a AiConfig,
        req: &'a SceneRequest,
    ) -> BoxFuture<'a, Result<String, AiError>> {
        Box::pin(async move { margin_ai::write_scene(&client(ai), req).await })
    }

    fn agent_model(&self, ai: &AiConfig) -> Arc<dyn ModelClient> {
        Arc::new(client(ai))
    }
}

/// `margin_video`'s ManimGX + FluidAudio pipeline.
pub struct VideoRenderer;

impl Renderer for VideoRenderer {
    fn detect_tools(&self) -> BoxFuture<'_, Vec<ToolStatus>> {
        Box::pin(margin_video::detect_tools())
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

/// The service's seam to `margin-embed`: the embedding model's files, loading it, and
/// reading, cutting and ranking sources. `EmbedTools` forwards to the crate; tests plug
/// in fakes (with a fake Hugging Face server).
pub trait SourceTools: Send + Sync + 'static {
    /// Whether a finished download is in `files`.
    fn is_complete(&self, files: &ModelFiles) -> bool;
    /// Bytes the downloaded model takes on disk.
    fn size_on_disk(&self, files: &ModelFiles) -> u64;
    /// Bytes a download would fetch.
    fn remote_size<'a>(
        &'a self,
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
    /// What is said in 16 kHz mono `samples`, word by word, using `work_dir` for
    /// temporary files. `Ok(None)` when no speech recognizer is installed. Blocking.
    fn transcribe(
        &self,
        samples: &[f32],
        work_dir: &Path,
    ) -> Result<Option<Transcript>, EmbedError>;
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

/// `margin_embed`: EmbeddingGemma 2 on Core ML, PDFKit + Vision OCR.
pub struct EmbedTools;

impl SourceTools for EmbedTools {
    fn is_complete(&self, files: &ModelFiles) -> bool {
        files.is_complete()
    }

    fn size_on_disk(&self, files: &ModelFiles) -> u64 {
        files.size_on_disk()
    }

    fn remote_size<'a>(
        &'a self,
        http: &'a reqwest::Client,
        hf_base: &'a str,
    ) -> BoxFuture<'a, Result<u64, EmbedError>> {
        Box::pin(ModelFiles::remote_size(http, hf_base))
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

    #[cfg(target_os = "macos")]
    fn load(&self, files: &ModelFiles) -> Result<Arc<dyn Embedder>, EmbedError> {
        Ok(Arc::new(margin_embed::CoreMlEmbedder::load(files)?))
    }

    #[cfg(not(target_os = "macos"))]
    fn load(&self, _files: &ModelFiles) -> Result<Arc<dyn Embedder>, EmbedError> {
        Err(EmbedError::Load(
            "the embedding model runs on Core ML, which needs macOS".into(),
        ))
    }

    fn extract(
        &self,
        path: &Path,
        kind: SourceKind,
        media_dir: &Path,
    ) -> Result<Extracted, EmbedError> {
        margin_embed::extract(path, kind, media_dir)
    }

    /// With `fluidaudiocli` (Parakeet), found like the video pipeline finds it.
    fn transcribe(
        &self,
        samples: &[f32],
        work_dir: &Path,
    ) -> Result<Option<Transcript>, EmbedError> {
        let Some(cli) = margin_embed::find_fluidaudiocli() else {
            return Ok(None);
        };
        margin_embed::transcribe(&cli, samples, work_dir).map(Some)
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

    /// Until `margin_embed` renders a single page on its own (R1-embed), the page comes
    /// from reading the PDF into a scratch folder next to `media_dir`.
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
        let scratch = media_dir.with_extension(format!("render-{}", uuid::Uuid::new_v4()));
        let result = (|| {
            let doc = margin_embed::extract(pdf, SourceKind::Pdf, &scratch)?;
            let picture = doc
                .pictures
                .into_iter()
                .find(|p| p.page == Some(page))
                .ok_or_else(|| EmbedError::Extract(format!("the PDF has no page {page}")))?;
            std::fs::create_dir_all(media_dir)?;
            std::fs::rename(&picture.path, &target)?;
            Ok(target.clone())
        })();
        let _ = std::fs::remove_dir_all(&scratch);
        result
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
