//! The service's seams to `margin-ai`, `margin-video` and `margin-embed`. Jobs call these
//! traits; the production implementations forward to the crates, and tests plug in fakes.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use margin_agent::ModelClient;
use margin_ai::{
    AiError, CallOptions, CoursePlan, Generated, LessonContext, PlanRequest, ResponsesClient,
    SceneRequest,
};
use margin_embed::{Chunk, EmbedError, Embedder, Extracted, ModelFiles, Transcript};
use margin_shared::{SourceKind, ToolStatus};
use margin_video::{RenderConfig, RenderError, RenderJob, RenderOutput, RenderProgress};

use crate::pipeline::{Chunker, ChunkerCfg};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Longer side of a picture sent to the lesson writer: enough for the Responses API's
/// high detail (which scales to 2048 px, then 768 px on the shorter side) without
/// sending more than it reads.
pub const WRITER_IMAGE_SIDE: usize = 1536;

/// Where and how to call the Responses API. Built per job from Settings and the secret store.
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

/// The model work jobs need. Every call takes its `CallOptions` (the route it runs on,
/// the prompt cache key, an output cap) and returns the answer with its usage, which the
/// caller records as a `model_call` trace row. Course outlines are the agent's
/// `propose_course` arguments (through `agent_model`); `plan_course` turns one into the
/// planned outline with per-lesson questions when the course has sources (§2.5).
pub trait Generator: Send + Sync + 'static {
    fn write_lesson<'a>(
        &'a self,
        ai: &'a AiConfig,
        ctx: &'a LessonContext,
        opts: &'a CallOptions,
        on_delta: Box<dyn FnMut(&str) + Send + 'a>,
    ) -> BoxFuture<'a, Result<Generated<String>, AiError>>;

    fn write_scene<'a>(
        &'a self,
        ai: &'a AiConfig,
        req: &'a SceneRequest,
        opts: &'a CallOptions,
    ) -> BoxFuture<'a, Result<Generated<String>, AiError>>;

    /// The outline planner: one strict-JSON call, checked against `req`.
    fn plan_course<'a>(
        &'a self,
        ai: &'a AiConfig,
        req: &'a PlanRequest,
        opts: &'a CallOptions,
    ) -> BoxFuture<'a, Result<Generated<CoursePlan>, AiError>>;

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
        opts: &'a CallOptions,
        on_delta: Box<dyn FnMut(&str) + Send + 'a>,
    ) -> BoxFuture<'a, Result<Generated<String>, AiError>> {
        Box::pin(
            async move { margin_ai::write_lesson_with(&client(ai), ctx, opts, on_delta).await },
        )
    }

    fn write_scene<'a>(
        &'a self,
        ai: &'a AiConfig,
        req: &'a SceneRequest,
        opts: &'a CallOptions,
    ) -> BoxFuture<'a, Result<Generated<String>, AiError>> {
        Box::pin(async move { margin_ai::write_scene_with(&client(ai), req, opts).await })
    }

    fn plan_course<'a>(
        &'a self,
        ai: &'a AiConfig,
        req: &'a PlanRequest,
        opts: &'a CallOptions,
    ) -> BoxFuture<'a, Result<Generated<CoursePlan>, AiError>> {
        Box::pin(async move { margin_ai::plan_course(&client(ai), req, opts).await })
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

/// `margin_embed`: EmbeddingGemma 2 on the configured backend (Core ML on macOS, ONNX
/// Runtime on Linux) and the platform's readers.
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

    /// The backend `files.backend` names (`service.toml`'s `embedder`): Core ML on
    /// macOS, ONNX Runtime on Linux; an error for one this build can't run.
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

    use anyhow::{Context, bail};
    use margin_embed::{Backend, ModelFiles};

    use super::SourceTools;
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
    /// folder under `data_dir/models`). A backend this build can't run is refused.
    pub fn files_for(config: &Config, embedder: Option<Backend>) -> anyhow::Result<ModelFiles> {
        let backend = match embedder {
            Some(b) => b,
            None => config.embedder_backend()?,
        };
        if !backend.is_built() {
            bail!("this build of margin-service can't run the {backend} embedder");
        }
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
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(60))
            .user_agent(concat!("margin-service/", env!("CARGO_PKG_VERSION")))
            .build()?;
        report(&format!(
            "Downloading {name} from {} into {}…",
            files.backend.repo(),
            files.dir.display()
        ));
        let last = Mutex::new(None::<Instant>);
        let progress = |received: u64, total: u64| {
            let mut last = last.lock().unwrap();
            let due = last.is_none_or(|t| t.elapsed() >= PRINT_INTERVAL);
            if due || (total > 0 && received >= total) {
                *last = Some(Instant::now());
                if total > 0 {
                    report(&format!(
                        "  {} of {} ({}%)",
                        mb(received),
                        mb(total),
                        received * 100 / total
                    ));
                } else {
                    report(&format!("  {}", mb(received)));
                }
            }
        };
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

        #[test]
        fn arguments_name_a_backend() {
            let args =
                |a: &[&str]| parse_args(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
            assert_eq!(args(&[]), Ok(None));
            assert_eq!(
                args(&["--embedder", "onnx-q8"]),
                Ok(Some(Backend::Onnx(OnnxDtype::Q8)))
            );
            assert_eq!(args(&["--embedder=coreml"]), Ok(Some(Backend::CoreMl)));
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
                let r = files_for(&c, Some(b));
                assert_eq!(r.is_ok(), b.is_built(), "{b}");
                if let Ok(f) = r {
                    assert_eq!(f.dir, Path::new("/data/models").join(b.dir_name()));
                }
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
