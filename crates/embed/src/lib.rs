//! Local embeddings for sources: EmbeddingGemma 2 run in-process for text, images and
//! audio, on Core ML (macOS) or ONNX Runtime (Linux, and macOS with the `onnx` feature;
//! `Backend`); reading sources (text and code, PDFKit + Vision OCR, ImageIO,
//! AVFoundation, FluidAudio speech recognition); chunking; and ranking. See PLAN.md
//! "Sources and embeddings".
//!
//! This file is the contract the service codes against. Implementations may add items but
//! must keep these signatures.

use std::path::{Path, PathBuf};

/// The Linux readers' programs and library, for Settings → Tools and the service's
/// warnings: `find_tesseract` (OCR; without it pictures ingest without text),
/// `find_ffmpeg` (audio and video), `find_pdfium` (PDFs; `set_tools_dir` adds the
/// service's tools folder to where the library is looked for).
#[cfg(any(target_os = "linux", feature = "linux-readers"))]
pub use av_ffmpeg::{FFMPEG, FFPROBE, Ffmpeg, find_ffmpeg};
pub use backend::{Backend, OnnxDtype, UnknownBackend, embedder_version};
/// `SourceKind::for_name` decides which files are sources.
pub use margin_shared::SourceKind;
#[cfg(any(target_os = "linux", feature = "linux-readers"))]
pub use ocr_tesseract::{TESSERACT, find_tesseract};
#[cfg(any(target_os = "linux", feature = "linux-readers"))]
pub use pdf_pdfium::{find_pdfium, pdfium_library_name, set_tools_dir};
pub use transcript::{FLUIDAUDIOCLI, Transcript, Word, find_fluidaudiocli, transcribe};

#[cfg(target_os = "macos")]
mod audio;
#[cfg(target_os = "macos")]
mod av;
#[cfg(any(target_os = "linux", feature = "linux-readers"))]
mod av_ffmpeg;
mod backend;
mod chunk;
#[cfg(target_os = "macos")]
mod coreml;
mod download;
mod extract;
pub mod fake;
#[cfg(target_os = "macos")]
mod image;
#[cfg(any(target_os = "linux", feature = "linux-readers"))]
mod image_linux;
#[cfg(all(test, target_os = "macos"))]
mod media_tests;
#[cfg(any(target_os = "linux", feature = "linux-readers"))]
mod ocr_tesseract;
#[cfg(any(target_os = "linux", feature = "onnx"))]
pub mod onnx;
#[cfg(target_os = "macos")]
mod pdf;
#[cfg(any(target_os = "linux", feature = "linux-readers"))]
mod pdf_pdfium;
mod rank;
mod transcript;
#[cfg(any(target_os = "linux", feature = "speech-onnx"))]
pub mod transcript_parakeet;
#[cfg(target_os = "macos")]
mod vision;

/// Hugging Face repo the Core ML model is downloaded from.
pub const REPO: &str = "FluidInference/embeddinggemma-2-coreml";
/// Hugging Face repo the ONNX export is downloaded from.
pub const ONNX_REPO: &str = "onnx-community/embeddinggemma-2-ONNX";
/// Default Hugging Face base URL; tests point downloads at a local server instead.
pub const HF_BASE: &str = "https://huggingface.co";
/// Embedding size.
pub const DIM: usize = 768;
/// Most tokens one `embed` input may hold (the largest Core ML `embed_S` function),
/// including the task prefix and `<bos>`/`<eos>`.
pub const MAX_TOKENS: usize = 512;
/// Chunk size the service cuts sources into, leaving room for the document prefix: the
/// hard cap every chunker keeps.
pub const CHUNK_TOKENS: usize = 480;
/// What the `"sections-v2"` chunker aims a chunk at (`docs/agent-rag-architecture.md`
/// §2.1: 256–400 tokens); a chunk only goes past it, up to `CHUNK_TOKENS`, to keep a
/// section's short tail with the rest or an atomic table or code block whole.
pub const CHUNK_TARGET_TOKENS: usize = 384;
/// The chunker names the service's pipeline registry knows them by.
pub const CHUNKER_PAGES: &str = "pages-v1";
pub const CHUNKER_SECTIONS: &str = "sections-v2";
/// How many vectors `Embedder::embed_pipelined` hands back at a time by default: the
/// most rows the service writes in one statement.
pub const EMBED_BATCH: usize = 64;
/// The model's document prompt (`config.json` `prompts.document`); a title replaces
/// `none`.
pub const DOCUMENT_PROMPT: &str = "title: none | text: ";
/// The model's query prompt (`config.json` `prompts.search_query`).
pub const QUERY_PROMPT: &str = "task: search result | query: ";
/// The Core ML model and the way its inputs are prefixed
/// (`Backend::CoreMl.embedder_version()`; each backend has its own,
/// `Backend::embedder_version`). Stored on every passage as `embedder`; rows with another
/// version are embedded again, and recordings are only served for their own version.
pub const EMBEDDER_VERSION: &str = "embeddinggemma-2-coreml/title-prefix-v1";
/// Sample rate of the audio the audio encoder and the speech recognizer take (mono).
pub const SAMPLE_RATE: u32 = 16_000;
/// One audio embedding covers this many samples: 10 seconds.
pub const WINDOW_SAMPLES: usize = 160_000;
/// Video frames are sampled every 5 seconds…
pub const FRAME_INTERVAL_MS: u64 = 5_000;
/// …but at most this many per video (longer videos are sampled more sparsely: an hour
/// gets one frame every 30 seconds).
pub const MAX_FRAMES: u64 = 120;

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("The embedding model isn't downloaded.")]
    NotDownloaded,
    #[error("Download failed: {0}")]
    Download(String),
    #[error("Couldn't load the embedding model: {0}")]
    Load(String),
    #[error("Embedding failed: {0}")]
    Embed(String),
    #[error("Couldn't read the file: {0}")]
    Extract(String),
    #[error("Couldn't transcribe the audio: {0}")]
    Transcribe(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// What an input is, which picks the model's task prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Purpose {
    /// A chunk of a document: `title: {title} | text: {chunk}`, with `title: none` when
    /// there is no title (the source name or section path, `docs/agent-rag-architecture.md`
    /// §2.1).
    Document { title: Option<String> },
    /// What we're looking for: `task: search result | query: {text}`.
    Query,
}

impl Purpose {
    /// A document chunk without a title (`title: none`).
    pub const DOCUMENT: Purpose = Purpose::Document { title: None };

    /// A document chunk titled `title` (`None` or blank: `title: none`).
    pub fn document(title: Option<&str>) -> Purpose {
        Purpose::Document {
            title: title
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty()),
        }
    }
}

/// The task prefix the model reads before `text`, from the model's two prompts: the
/// document prompt with `none` replaced by the title (newlines and `|` in it become
/// spaces, so a title can't fake the prompt's shape), or the query prompt.
pub(crate) fn render_prompt(
    document_prompt: &str,
    query_prompt: &str,
    purpose: &Purpose,
) -> String {
    match purpose {
        Purpose::Document { title: None } => document_prompt.to_string(),
        Purpose::Document { title: Some(title) } => {
            let title: String = title
                .chars()
                .map(|c| if c == '|' || c.is_control() { ' ' } else { c })
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if title.is_empty() {
                document_prompt.to_string()
            } else {
                document_prompt.replacen("none", &title, 1)
            }
        }
        Purpose::Query => query_prompt.to_string(),
    }
}

/// The prefix the model reads before a text with this purpose
/// (`title: {title} | text: ` or `task: search result | query: `).
pub fn prompt(purpose: &Purpose) -> String {
    render_prompt(DOCUMENT_PROMPT, QUERY_PROMPT, purpose)
}

/// The title a chunk is embedded with: its section path, or the document's name when it
/// has none (`docs/agent-rag-architecture.md` §2.1).
pub fn chunk_title(document_name: &str, chunk: &Chunk) -> String {
    if chunk.section_path.trim().is_empty() {
        document_name.trim().to_string()
    } else {
        chunk.section_path.clone()
    }
}

/// How many soft tokens the image encoder spends on a picture (its `vision_70`,
/// `vision_140` and `vision_280` functions). More keeps more detail and takes longer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageTokens {
    /// 70 tokens.
    Low,
    /// 140 tokens: the model's default for video frames.
    Medium,
    /// 280 tokens: the model's default for images.
    High,
}

impl ImageTokens {
    pub fn count(self) -> usize {
        match self {
            ImageTokens::Low => 70,
            ImageTokens::Medium => 140,
            ImageTokens::High => 280,
        }
    }
}

/// Turns texts, images and audio into L2-normalized `DIM`-dimensional vectors in one
/// space, so a text query finds pictures and recordings too. Blocking: the service calls
/// it from `spawn_blocking`.
pub trait Embedder: Send + Sync {
    /// One vector per text. Texts longer than `MAX_TOKENS` (after the prefix) are cut.
    fn embed(&self, texts: &[String], purpose: Purpose) -> Result<Vec<Vec<f32>>, EmbedError>;
    /// Tokens `text` takes with the model's tokenizer (no prefix, no `<bos>`/`<eos>`).
    fn count_tokens(&self, text: &str) -> usize;
    /// One vector per image file (any format ImageIO reads; its orientation applied).
    fn embed_images(
        &self,
        images: &[PathBuf],
        tokens: ImageTokens,
    ) -> Result<Vec<Vec<f32>>, EmbedError>;
    /// One vector per window of 16 kHz mono samples (at most `WINDOW_SAMPLES` each;
    /// more are cut).
    fn embed_audio(&self, windows: &[Vec<f32>]) -> Result<Vec<Vec<f32>>, EmbedError>;

    /// Embeds `inputs` (each text with its own purpose, so every chunk can carry its own
    /// title) and hands the vectors back `batch` at a time, in order:
    /// `on_batch(index of the batch's first input, vectors)`, on the calling thread, so
    /// the caller writes one batch while the next is embedded. An error from `on_batch`
    /// (a cancelled ingest) stops the work and is returned. The Core ML embedder prepares
    /// inputs (tokenizing, table lookups) on one thread while the model predicts on
    /// another; this default embeds batch by batch.
    fn embed_pipelined(
        &self,
        inputs: &[(String, Purpose)],
        batch: usize,
        on_batch: &mut dyn FnMut(usize, Vec<Vec<f32>>) -> Result<(), EmbedError>,
    ) -> Result<(), EmbedError> {
        let batch = batch.max(1);
        for (n, group) in inputs.chunks(batch).enumerate() {
            let mut vectors = Vec::with_capacity(group.len());
            // One `embed` call per run of inputs with the same purpose.
            let mut start = 0;
            while start < group.len() {
                let purpose = &group[start].1;
                let end = start
                    + group[start..]
                        .iter()
                        .take_while(|(_, p)| p == purpose)
                        .count();
                let texts: Vec<String> = group[start..end].iter().map(|(t, _)| t.clone()).collect();
                vectors.extend(self.embed(&texts, purpose.clone())?);
                start = end;
            }
            on_batch(n * batch, vectors)?;
        }
        Ok(())
    }

    /// The version stamped on what it embeds (`Backend::embedder_version`): rows of
    /// another version are embedded again, and the dense leg compares only vectors of
    /// the active one. Fakes keep the Core ML version unless they say otherwise.
    fn version(&self) -> &str {
        EMBEDDER_VERSION
    }
}

/// Runs `prep` over `inputs` on a second thread while `run` turns each prepared input
/// into a vector on this one, handing vectors to `on_batch` `batch` at a time (with the
/// index of the batch's first input). The preparing thread stays at most `batch` inputs
/// ahead. Errors from any of the three stop both threads.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn pipelined<I: Sync, P: Send>(
    inputs: &[I],
    batch: usize,
    prep: &(dyn Fn(&I) -> Result<P, EmbedError> + Sync),
    run: &mut dyn FnMut(P) -> Result<Vec<f32>, EmbedError>,
    on_batch: &mut dyn FnMut(usize, Vec<Vec<f32>>) -> Result<(), EmbedError>,
) -> Result<(), EmbedError> {
    let batch = batch.max(1);
    std::thread::scope(|scope| {
        let (send, receive) = std::sync::mpsc::sync_channel::<Result<P, EmbedError>>(batch);
        scope.spawn(move || {
            for input in inputs {
                let prepared = prep(input);
                let failed = prepared.is_err();
                // The receiver is gone when the other side stopped early.
                if send.send(prepared).is_err() || failed {
                    break;
                }
            }
        });
        let mut vectors = Vec::with_capacity(batch.min(inputs.len()));
        let mut first = 0;
        for prepared in receive.iter() {
            vectors.push(run(prepared?)?);
            if vectors.len() == batch {
                on_batch(first, std::mem::take(&mut vectors))?;
                first += batch;
            }
        }
        if !vectors.is_empty() {
            on_batch(first, vectors)?;
        }
        Ok(())
    })
}

/// The downloaded model files of one backend in one folder
/// (`data_dir/models/{Backend::dir_name}`).
#[derive(Debug, Clone)]
pub struct ModelFiles {
    pub dir: PathBuf,
    pub backend: Backend,
}

impl ModelFiles {
    /// The Core ML download in `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self::with_backend(dir, Backend::CoreMl)
    }

    /// `backend`'s download in `dir`.
    pub fn with_backend(dir: impl Into<PathBuf>, backend: Backend) -> Self {
        Self {
            dir: dir.into(),
            backend,
        }
    }

    /// Whether every file of every part (text, vision, audio) is present: a finished
    /// download.
    pub fn is_complete(&self) -> bool {
        download::is_complete(&self.dir, download::Manifest::Model(self.backend))
    }

    /// Path of an ONNX graph (`"model"`, `"vision_encoder"`, `"audio_encoder"`) of this
    /// download; `None` for a Core ML download.
    pub fn graph_path(&self, graph: &str) -> Option<PathBuf> {
        match self.backend {
            Backend::Onnx(dtype) => Some(self.dir.join(download::onnx_graph(graph, dtype))),
            Backend::CoreMl => None,
        }
    }

    /// Bytes on disk for the downloaded files.
    pub fn size_on_disk(&self) -> u64 {
        download::size_on_disk(&self.dir)
    }

    /// Total bytes the Core ML download fetches (from the Hugging Face API), for showing
    /// before the user starts it. `remote_bytes` asks for this download's backend.
    pub async fn remote_size(client: &reqwest::Client, hf_base: &str) -> Result<u64, EmbedError> {
        ModelFiles::new(PathBuf::new())
            .remote_bytes(client, hf_base)
            .await
    }

    /// Total bytes this backend's download fetches (from the Hugging Face API).
    pub async fn remote_bytes(
        &self,
        client: &reqwest::Client,
        hf_base: &str,
    ) -> Result<u64, EmbedError> {
        let files =
            download::remote_files(client, hf_base, download::Manifest::Model(self.backend))
                .await?;
        Ok(files.iter().map(|f| f.size).sum())
    }

    /// Downloads every part of the model (the text model, the vision and audio encoders
    /// and their tables) from `hf_base`/the backend's repo (resolve/main) into `dir`, into temporary
    /// names first so an interrupted download is never mistaken for a complete one; files
    /// already there from an earlier attempt are kept. `progress(received, total)` is
    /// called as bytes arrive, with one total for everything.
    pub async fn download(
        &self,
        client: &reqwest::Client,
        hf_base: &str,
        progress: &(dyn Fn(u64, u64) + Send + Sync),
    ) -> Result<(), EmbedError> {
        download::download(
            &self.dir,
            download::Manifest::Model(self.backend),
            client,
            hf_base,
            progress,
        )
        .await
    }

    /// Deletes the downloaded files (and the compiled model cache).
    pub fn remove(&self) -> Result<(), EmbedError> {
        download::remove(&self.dir)
    }
}

/// EmbeddingGemma 2 on Core ML (Neural Engine where possible; the encoders on the GPU).
/// macOS only.
#[cfg(target_os = "macos")]
pub struct CoreMlEmbedder(coreml::Model);

#[cfg(target_os = "macos")]
impl CoreMlEmbedder {
    /// Loads the tokenizer and token table and the Core ML text model, compiling the
    /// `.mlpackage` on first use (cached next to it). The vision and audio encoders load
    /// (and compile) the first time they're needed. Blocking.
    pub fn load(files: &ModelFiles) -> Result<Self, EmbedError> {
        if files.backend != Backend::CoreMl {
            return Err(EmbedError::Load(format!(
                "{} holds the {} model, not Core ML's",
                files.dir.display(),
                files.backend
            )));
        }
        coreml::Model::load(files).map(CoreMlEmbedder)
    }
}

#[cfg(target_os = "macos")]
impl Embedder for CoreMlEmbedder {
    fn embed(&self, texts: &[String], purpose: Purpose) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.0.embed(texts, purpose)
    }
    fn count_tokens(&self, text: &str) -> usize {
        self.0.count_tokens(text)
    }
    fn embed_images(
        &self,
        images: &[PathBuf],
        tokens: ImageTokens,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.0.embed_images(images, tokens)
    }
    fn embed_audio(&self, windows: &[Vec<f32>]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.0.embed_audio(windows)
    }
    fn embed_pipelined(
        &self,
        inputs: &[(String, Purpose)],
        batch: usize,
        on_batch: &mut dyn FnMut(usize, Vec<Vec<f32>>) -> Result<(), EmbedError>,
    ) -> Result<(), EmbedError> {
        self.0.embed_pipelined(inputs, batch, on_batch)
    }
    fn version(&self) -> &str {
        Backend::CoreMl.embedder_version()
    }
}

/// How the ONNX backend runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnnxOptions {
    /// ONNX Runtime intra-op threads; 0 = one less than the CPUs (at least one).
    pub threads: usize,
    /// Most padded tokens (rows × the longest row) in one run of the text graph; clamped
    /// to `MAX_TOKENS..=8192`.
    pub batch_tokens: usize,
}

impl Default for OnnxOptions {
    fn default() -> Self {
        Self {
            threads: 0,
            batch_tokens: 4096,
        }
    }
}

impl OnnxOptions {
    /// The thread count to use.
    pub fn threads(&self) -> usize {
        if self.threads > 0 {
            return self.threads;
        }
        std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .saturating_sub(1)
            .max(1)
    }

    /// `batch_tokens`, clamped.
    pub fn batch_tokens(&self) -> usize {
        self.batch_tokens.clamp(MAX_TOKENS, 8192)
    }
}

/// EmbeddingGemma 2 on ONNX Runtime, CPU execution provider (`docs/plan-linux-service.md`
/// §2.1). Text batches for real; Linux always, macOS with the `onnx` feature.
#[cfg(any(target_os = "linux", feature = "onnx"))]
pub struct OnnxEmbedder(onnx::Model);

#[cfg(not(any(target_os = "linux", feature = "onnx")))]
pub struct OnnxEmbedder(std::convert::Infallible);

impl OnnxEmbedder {
    /// Loads the tokenizer and the text graph of an ONNX download (`files.backend` names
    /// the format). The encoders load the first time they're needed. Blocking.
    #[cfg(any(target_os = "linux", feature = "onnx"))]
    pub fn load(files: &ModelFiles, opts: &OnnxOptions) -> Result<Self, EmbedError> {
        onnx::Model::load(files, opts).map(OnnxEmbedder)
    }

    #[cfg(not(any(target_os = "linux", feature = "onnx")))]
    pub fn load(_files: &ModelFiles, _opts: &OnnxOptions) -> Result<Self, EmbedError> {
        Err(EmbedError::Load(
            "this build has no ONNX runtime (build margin-embed with the onnx feature)".into(),
        ))
    }
}

#[cfg(any(target_os = "linux", feature = "onnx"))]
impl Embedder for OnnxEmbedder {
    fn embed(&self, texts: &[String], purpose: Purpose) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.0.embed(texts, purpose)
    }
    fn count_tokens(&self, text: &str) -> usize {
        self.0.count_tokens(text)
    }
    fn embed_images(
        &self,
        images: &[PathBuf],
        tokens: ImageTokens,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.0.embed_images(images, tokens)
    }
    fn embed_audio(&self, windows: &[Vec<f32>]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.0.embed_audio(windows)
    }
    fn embed_pipelined(
        &self,
        inputs: &[(String, Purpose)],
        batch: usize,
        on_batch: &mut dyn FnMut(usize, Vec<Vec<f32>>) -> Result<(), EmbedError>,
    ) -> Result<(), EmbedError> {
        self.0.embed_pipelined(inputs, batch, on_batch)
    }
    fn version(&self) -> &str {
        self.0.version()
    }
}

/// Loads the embedder `files.backend` names, with default ONNX options: what the
/// service's `SourceTools::load` calls. An error for a backend this binary wasn't built
/// with (`Backend::is_built`). Blocking.
pub fn load_embedder(files: &ModelFiles) -> Result<std::sync::Arc<dyn Embedder>, EmbedError> {
    load_embedder_with(files, &OnnxOptions::default())
}

/// `load_embedder` with these ONNX options.
pub fn load_embedder_with(
    files: &ModelFiles,
    opts: &OnnxOptions,
) -> Result<std::sync::Arc<dyn Embedder>, EmbedError> {
    if !files.backend.is_built() {
        return Err(EmbedError::Load(format!(
            "this build can't run the {} embedder",
            files.backend
        )));
    }
    match files.backend {
        #[cfg(target_os = "macos")]
        Backend::CoreMl => Ok(std::sync::Arc::new(CoreMlEmbedder::load(files)?)),
        #[cfg(any(target_os = "linux", feature = "onnx"))]
        Backend::Onnx(_) => Ok(std::sync::Arc::new(OnnxEmbedder::load(files, opts)?)),
        #[allow(unreachable_patterns)]
        _ => {
            let _ = opts;
            Err(EmbedError::Load(format!(
                "this build can't run the {} embedder",
                files.backend
            )))
        }
    }
}

/// How a text document marks its structure, which decides how the `"sections-v2"`
/// chunker finds headings, tables and code blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextFormat {
    /// No markup the chunker reads: paragraphs and sentences only (PDF text, OCR text,
    /// plain notes, notebooks).
    #[default]
    Plain,
    /// `#` and underlined headings, pipe tables, fenced code and `$$` math.
    Markdown,
    /// reStructuredText: underlined (and overlined) titles, directives, literal blocks,
    /// grid and simple tables.
    Rst,
    /// HTML, already turned into text by `extract`: `h1`–`h6` as `#` headings and table
    /// rows as `| cell | cell |` lines, so it is read like Markdown.
    Html,
    /// LaTeX: `\part` … `\subparagraph` headings, environments and display math as atomic
    /// blocks.
    Latex,
}

impl TextFormat {
    /// The format a file name says (`.md`, `.rst`, `.tex`, `.html`…), or, for a name
    /// that doesn't (`.txt`, none), the one `text` looks like.
    pub fn detect(name: &str, text: &str) -> TextFormat {
        let ext = Path::new(name)
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "md" | "markdown" | "mdown" | "mkd" | "mdx" => TextFormat::Markdown,
            "rst" | "rest" => TextFormat::Rst,
            "tex" | "latex" | "ltx" => TextFormat::Latex,
            "html" | "htm" | "xhtml" => TextFormat::Html,
            _ => chunk::sniff_format(text),
        }
    }
}

/// Everything read from a source file.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Extracted {
    /// Its text: the whole of a text or code file, or a PDF page by page. Empty for
    /// images, audio and video (their speech comes from `transcribe`).
    pub pages: Vec<Page>,
    /// Pictures to embed with the image encoder: an image file itself, each page of a PDF
    /// (rendered), or frames sampled from a video.
    pub pictures: Vec<Picture>,
    /// The sound of an audio file or a video's soundtrack, 16 kHz mono.
    pub sound: Option<Vec<f32>>,
    /// Length of an audio or video source.
    pub duration_ms: Option<u64>,
    /// How the text marks its structure (`Plain` for everything but text documents).
    pub format: TextFormat,
}

/// A run of text: one page of a PDF (`number` from 1), or the whole of a text/code file
/// (`number` = None).
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub number: Option<u32>,
    pub text: String,
}

/// A picture of a source, as an image file on disk.
#[derive(Debug, Clone, PartialEq)]
pub struct Picture {
    /// The image: the source file itself, or `page_file`/`frame_file` in the media folder.
    pub path: PathBuf,
    /// The PDF page it shows.
    pub page: Option<u32>,
    /// The video time it was taken at.
    pub at_ms: Option<u64>,
    /// How much detail to embed it with.
    pub tokens: ImageTokens,
}

/// The rendered image of PDF page `n` (from 1) in the media folder.
pub fn page_file(n: u32) -> String {
    format!("page-{n}.png")
}

/// The frame of a video taken at `ms` in the media folder.
pub fn frame_file(ms: u64) -> String {
    format!("frame-{ms}.jpg")
}

/// When frames are taken from a video `duration_ms` long: every `FRAME_INTERVAL_MS` (or
/// `duration / MAX_FRAMES` when that's longer), each in the middle of its stretch, so the
/// first isn't the usual black opening frame. A clip shorter than the interval gets one
/// frame, in its middle. Returns `(interval_ms, times)`.
pub fn frame_times(duration_ms: u64) -> (u64, Vec<u64>) {
    if duration_ms == 0 {
        return (FRAME_INTERVAL_MS, Vec::new());
    }
    let interval = FRAME_INTERVAL_MS.max(duration_ms.div_ceil(MAX_FRAMES));
    let count = duration_ms.div_ceil(interval);
    let times = (0..count)
        .map(|k| (k * interval + ((k + 1) * interval).min(duration_ms)) / 2)
        .collect();
    (interval, times)
}

/// Reads a source. Text and code are decoded as UTF-8 (lossy), with `format` set from
/// the name or the contents; PDFs with PDFKit (Vision OCR for pages without a text
/// layer), each page also rendered to `page_file(n)` in `media_dir`; images are checked
/// to decode and their text read with Vision OCR (one page, when there is any); audio is
/// decoded to 16 kHz mono; videos give `frame_file(ms)` pictures in `media_dir` (see
/// `frame_times`) and their soundtrack. Blocking.
pub fn extract(path: &Path, kind: SourceKind, media_dir: &Path) -> Result<Extracted, EmbedError> {
    extract::extract(path, kind, media_dir, true)
}

/// `extract`, except that PDF pages aren't rendered: a PDF's `pictures` is empty, and
/// `pdf_page_picture` draws a page when it is first needed (to embed it, cite it or show
/// it). Everything else is as `extract`. Blocking.
pub fn extract_lazy(
    path: &Path,
    kind: SourceKind,
    media_dir: &Path,
) -> Result<Extracted, EmbedError> {
    extract::extract(path, kind, media_dir, false)
}

/// Renders page `page` (from 1) of the PDF at `path` as the picture `extract` would have
/// made of it (color, 144 dpi, longer side at most 2048 px) and writes it to `out` as
/// PNG. An error for a page past the end. Blocking.
#[cfg(target_os = "macos")]
pub fn pdf_page_picture(path: &Path, page: u32, out: &Path) -> Result<(), EmbedError> {
    pdf::page_picture(path, page, out)
}

/// On Linux with pdfium (`pdf_pdfium.rs`), at the same 144 dpi and 2048 px cap.
#[cfg(target_os = "linux")]
pub fn pdf_page_picture(path: &Path, page: u32, out: &Path) -> Result<(), EmbedError> {
    pdf_pdfium::page_picture(path, page, out)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn pdf_page_picture(_path: &Path, _page: u32, _out: &Path) -> Result<(), EmbedError> {
    Err(EmbedError::Extract(
        "reading PDFs needs macOS or Linux".into(),
    ))
}

/// Splits 16 kHz mono samples into the windows the audio encoder embeds: consecutive
/// `WINDOW_SAMPLES` slices, dropping a last one under half a second (it carries almost
/// nothing and may be too short to produce a token), as FluidUse does. Returns each
/// window's first sample with the slice.
pub fn audio_windows(samples: &[f32]) -> Vec<(usize, &[f32])> {
    let mut windows: Vec<(usize, &[f32])> = samples
        .chunks(WINDOW_SAMPLES)
        .enumerate()
        .map(|(i, w)| (i * WINDOW_SAMPLES, w))
        .collect();
    if windows.len() > 1
        && windows
            .last()
            .is_some_and(|(_, w)| w.len() < SAMPLE_RATE as usize / 2)
    {
        windows.pop();
    }
    windows
}

/// Milliseconds from the start of 16 kHz audio at `sample`.
pub fn sample_ms(sample: usize) -> u64 {
    sample as u64 * 1000 / SAMPLE_RATE as u64
}

/// An image file shrunk to at most `max_side` pixels (never enlarged) and upright, as
/// JPEG: what the lesson writer is shown. Blocking.
#[cfg(target_os = "macos")]
pub fn jpeg_for_model(path: &Path, max_side: usize) -> Result<Vec<u8>, EmbedError> {
    image::jpeg(path, max_side, 0.85)
}

/// On Linux with `image` (+ libheif), EXIF orientation applied (`image_linux.rs`).
#[cfg(target_os = "linux")]
pub fn jpeg_for_model(path: &Path, max_side: usize) -> Result<Vec<u8>, EmbedError> {
    image_linux::jpeg(path, max_side, image_linux::MODEL_JPEG_QUALITY)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn jpeg_for_model(_path: &Path, _max_side: usize) -> Result<Vec<u8>, EmbedError> {
    Err(EmbedError::Extract(
        "reading images needs macOS or Linux".into(),
    ))
}

/// A piece of a source small enough to embed.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Chunk {
    pub text: String,
    /// PDF page it came from.
    pub page: Option<u32>,
    /// Where in an audio or video source the words are (transcript passages).
    pub start_ms: Option<u64>,
    pub end_ms: Option<u64>,
    /// The headings above it, "Chapter 2 > Fourier series"; empty when the document has
    /// none (or the chunker doesn't look for them).
    pub section_path: String,
    /// The lines of a text document it spans (1-based, inclusive).
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
    /// A content hash of `text` (hex), so re-ingesting an unchanged chunk keeps its
    /// vector; empty from chunkers that don't set it.
    pub hash: String,
}

impl Chunk {
    /// A chunk of `text` from `page`, every other field empty.
    pub fn new(text: impl Into<String>, page: Option<u32>) -> Self {
        Self {
            text: text.into(),
            page,
            ..Self::default()
        }
    }
}

/// Reorders retrieved passages by relevance to a query (`docs/agent-rag-architecture.md`
/// §2.4): a local cross-encoder (R2, `docs/r2-brief.md` D5; `onnx::OnnxReranker`, or
/// `fake::FakeReranker` in tests), behind the service's `"onnx-crossencoder"` stage.
/// Scores are in the input's order; higher is more relevant. Blocking.
pub trait Reranker: Send + Sync {
    /// The model and version, recorded with eval runs and traces.
    fn name(&self) -> &str;
    fn score(&self, query: &str, passages: &[String]) -> Result<Vec<f32>, EmbedError>;
}

/// The cross-encoders the `"onnx-crossencoder"` reranker can run (`params.model`),
/// downloaded on demand into `data_dir/models/{dir}`, never bundled. The repos and file
/// names are R2-xenc's to confirm (`docs/r2-brief.md` §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RerankerModel {
    /// `ms-marco-minilm-l6-v2`: 22 M params, English, ~23 MB int8. The default.
    MsMarcoMiniLmL6,
    /// `bge-reranker-base`: 278 M params, multilingual, ~280 MB int8.
    BgeRerankerBase,
}

impl RerankerModel {
    pub const ALL: [RerankerModel; 2] = [
        RerankerModel::MsMarcoMiniLmL6,
        RerankerModel::BgeRerankerBase,
    ];
    /// What `params.model` names when it names nothing.
    pub const DEFAULT: RerankerModel = RerankerModel::MsMarcoMiniLmL6;

    /// The `params.model` name.
    pub fn name(self) -> &'static str {
        match self {
            RerankerModel::MsMarcoMiniLmL6 => "ms-marco-minilm-l6-v2",
            RerankerModel::BgeRerankerBase => "bge-reranker-base",
        }
    }

    pub fn from_name(name: &str) -> Option<RerankerModel> {
        Self::ALL.into_iter().find(|m| m.name() == name)
    }

    /// The Hugging Face repo of its ONNX export.
    pub fn repo(self) -> &'static str {
        match self {
            RerankerModel::MsMarcoMiniLmL6 => "Xenova/ms-marco-MiniLM-L-6-v2",
            RerankerModel::BgeRerankerBase => "Xenova/bge-reranker-base",
        }
    }

    /// The folder under `data_dir/models` its files are kept in.
    pub fn dir(self) -> &'static str {
        match self {
            RerankerModel::MsMarcoMiniLmL6 => "reranker-ms-marco-minilm-l6-v2",
            RerankerModel::BgeRerankerBase => "reranker-bge-reranker-base",
        }
    }

    /// The repo files a download fetches; all are required.
    pub fn files(self) -> &'static [&'static str] {
        &["config.json", "tokenizer.json", RERANKER_GRAPH]
    }

    /// The model and version, recorded with eval runs and traces (`Reranker::name`).
    pub fn version(self) -> String {
        format!("{}@int8", self.name())
    }
}

/// The int8 graph of a reranker's ONNX export.
pub const RERANKER_GRAPH: &str = "onnx/model_quantized.onnx";

/// The downloaded files of one reranker model in one folder
/// (`data_dir/models/{RerankerModel::dir}`).
#[derive(Debug, Clone)]
pub struct RerankerFiles {
    pub dir: PathBuf,
    pub model: RerankerModel,
}

impl RerankerFiles {
    /// `model`'s download under `models_dir`.
    pub fn new(models_dir: &Path, model: RerankerModel) -> Self {
        Self {
            dir: models_dir.join(model.dir()),
            model,
        }
    }

    /// Whether every file is present: a finished download.
    pub fn is_complete(&self) -> bool {
        download::is_complete(&self.dir, download::Manifest::Reranker(self.model))
    }

    /// The tokenizer and the graph.
    pub fn tokenizer_path(&self) -> PathBuf {
        self.dir.join("tokenizer.json")
    }
    pub fn graph_path(&self) -> PathBuf {
        self.dir.join(RERANKER_GRAPH)
    }

    /// Total bytes the download fetches (from the Hugging Face API, at the pinned
    /// `RerankerModel::revision`).
    pub async fn remote_size(
        &self,
        client: &reqwest::Client,
        hf_base: &str,
    ) -> Result<u64, EmbedError> {
        let files =
            download::remote_files(client, hf_base, download::Manifest::Reranker(self.model))
                .await?;
        Ok(files.iter().map(|f| f.size).sum())
    }

    /// Fetches the files (keeping any already complete from an interrupted download),
    /// reporting `(received, total)`.
    pub async fn download(
        &self,
        client: &reqwest::Client,
        hf_base: &str,
        progress: &(dyn Fn(u64, u64) + Send + Sync),
    ) -> Result<(), EmbedError> {
        download::download(
            &self.dir,
            download::Manifest::Reranker(self.model),
            client,
            hf_base,
            progress,
        )
        .await
    }

    /// Deletes the folder. Nothing there is fine.
    pub fn remove(&self) -> Result<(), EmbedError> {
        download::remove(&self.dir)
    }
}

/// Cuts extracted text into chunks of at most `max_tokens` (as counted by
/// `count_tokens`): prose along paragraphs then sentences, code along lines, PDFs page by
/// page. Adjacent chunks overlap by about 10% so a passage cut in two is still found.
pub fn chunk(
    doc: &Extracted,
    kind: SourceKind,
    count_tokens: &dyn Fn(&str) -> usize,
    max_tokens: usize,
) -> Vec<Chunk> {
    chunk::chunk(doc, kind, count_tokens, max_tokens)
}

/// Settings of the `"sections-v2"` chunker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SectionsCfg {
    /// No chunk counts more tokens than this.
    pub max_tokens: usize,
    /// Chunks are filled to about this many tokens (at most `max_tokens`).
    pub target_tokens: usize,
}

impl Default for SectionsCfg {
    fn default() -> Self {
        Self {
            max_tokens: CHUNK_TOKENS,
            target_tokens: CHUNK_TARGET_TOKENS,
        }
    }
}

/// The `"sections-v2"` chunker (`docs/agent-rag-architecture.md` §2.1). Text documents
/// are cut along their headings (Markdown, reST, HTML, LaTeX; `doc.format`): a chunk
/// never spans two sections, and carries the headings above it as `section_path`
/// ("Chapter 2 > Fourier series") and the lines it spans. Inside a section, prose is cut
/// between paragraphs then sentences, filled to about `target_tokens`; tables, code
/// fences and display math are kept whole when they fit `max_tokens`. Each chunk after
/// the first of a section starts with the previous chunk's last sentence. PDFs (and OCR
/// text) are cut page by page the same way, without headings or lines; code along lines.
/// Every chunk has `hash`, the SHA-256 of its section path and text, so an unchanged
/// chunk under an unchanged heading keeps its vector. Deterministic.
pub fn chunk_sections(
    doc: &Extracted,
    kind: SourceKind,
    count_tokens: &dyn Fn(&str) -> usize,
    cfg: &SectionsCfg,
) -> Vec<Chunk> {
    chunk::chunk_sections(doc, kind, count_tokens, cfg)
}

/// SHA-256 of `bytes`, as lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The hash a chunk carries: SHA-256 of its section path, a zero byte and its text.
pub fn chunk_hash(section_path: &str, text: &str) -> String {
    let mut bytes = Vec::with_capacity(section_path.len() + 1 + text.len());
    bytes.extend_from_slice(section_path.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(text.as_bytes());
    sha256_hex(&bytes)
}

/// SHA-256 (the `sha2` crate).
pub(crate) mod sha256 {
    pub fn digest(bytes: &[u8]) -> [u8; 32] {
        <sha2::Sha256 as sha2::Digest>::digest(bytes).into()
    }
}

/// Cosine similarity of two L2-normalized vectors (their dot product).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Ranking score for a chunk: cosine similarity plus a boost for the query's exact
/// words (and numbers/identifiers) that appear in the chunk, since no embedding holds an
/// invoice number exactly.
pub fn score(query_vec: &[f32], query_text: &str, chunk_vec: &[f32], chunk_text: &str) -> f32 {
    cosine(query_vec, chunk_vec) + rank::term_boost(query_text, chunk_text)
}

/// Whether tests may load real models (the Core ML embedder, the speech recognizer):
/// only when `MARGIN_REAL_MODEL_TESTS=1`. A plain `cargo test` uses fakes and recordings
/// only; the real-model tests skip with a message.
#[cfg(test)]
pub(crate) fn real_model_tests() -> bool {
    let on = std::env::var("MARGIN_REAL_MODEL_TESTS").is_ok_and(|v| v == "1");
    if !on {
        eprintln!("skipping: real-model tests need MARGIN_REAL_MODEL_TESTS=1");
    }
    on
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_every_five_seconds_up_to_a_cap() {
        assert_eq!(frame_times(0).1, Vec::<u64>::new());
        assert_eq!(frame_times(3_000), (5_000, vec![1_500]));
        assert_eq!(frame_times(12_000), (5_000, vec![2_500, 7_500, 11_000]));
        let (interval, times) = frame_times(10 * 60_000);
        assert_eq!(interval, 5_000);
        assert_eq!(times.len(), 120);
        let (interval, times) = frame_times(60 * 60_000);
        assert_eq!(interval, 30_000);
        assert_eq!(times.len(), 120);
        assert_eq!(times[0], 15_000);
        assert!(times.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn audio_is_cut_into_ten_second_windows() {
        let samples = vec![0f32; WINDOW_SAMPLES * 2 + 4000];
        let windows = audio_windows(&samples);
        // The 0.25-s tail is dropped.
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[1].0, WINDOW_SAMPLES);
        assert_eq!(sample_ms(windows[1].0), 10_000);
        let samples = vec![0f32; WINDOW_SAMPLES + 9000];
        assert_eq!(audio_windows(&samples).len(), 2);
        // A short recording is one window, however short.
        assert_eq!(audio_windows(&[0.0; 100]).len(), 1);
        assert!(audio_windows(&[]).is_empty());
    }

    #[test]
    fn sha256_matches_the_standard_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // A million 'a's crosses many blocks.
        assert_eq!(
            sha256_hex(&vec![b'a'; 1_000_000]),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
        assert_ne!(chunk_hash("A", "b"), chunk_hash("", "Ab"));
    }

    #[test]
    fn prompts_carry_the_title() {
        assert_eq!(prompt(&Purpose::DOCUMENT), "title: none | text: ");
        assert_eq!(
            prompt(&Purpose::document(Some(" Fourier > Gibbs "))),
            "title: Fourier > Gibbs | text: "
        );
        assert_eq!(
            prompt(&Purpose::document(Some("  "))),
            "title: none | text: "
        );
        // A title can't add a second "|" field or a line break.
        assert_eq!(
            prompt(&Purpose::Document {
                title: Some("a | text: b\nc".into())
            }),
            "title: a text: b c | text: "
        );
        assert_eq!(prompt(&Purpose::Query), "task: search result | query: ");
        let chunk = Chunk::new("x", None);
        assert_eq!(chunk_title(" notes.md ", &chunk), "notes.md");
        let chunk = Chunk {
            section_path: "A > B".into(),
            ..chunk
        };
        assert_eq!(chunk_title("notes.md", &chunk), "A > B");
        assert_eq!(embedder_version(Backend::CoreMl), EMBEDDER_VERSION);
        assert_eq!(fake::FakeEmbedder.version(), EMBEDDER_VERSION);
    }

    #[test]
    fn the_default_pipelined_embed_batches_in_order_and_stops_on_error() {
        let fake = fake::FakeEmbedder;
        let inputs: Vec<(String, Purpose)> = (0..10)
            .map(|i| {
                (
                    format!("text number {i}"),
                    Purpose::document(Some(&format!("t{}", i / 3))),
                )
            })
            .collect();
        let mut got = Vec::new();
        fake.embed_pipelined(&inputs, 4, &mut |first, vectors| {
            got.push((first, vectors.len()));
            Ok(())
        })
        .unwrap();
        assert_eq!(got, vec![(0, 4), (4, 4), (8, 2)]);
        let mut calls = 0;
        let err = fake
            .embed_pipelined(&inputs, 4, &mut |_, _| {
                calls += 1;
                Err(EmbedError::Embed("cancelled".into()))
            })
            .unwrap_err();
        assert!(matches!(err, EmbedError::Embed(m) if m == "cancelled"));
        assert_eq!(calls, 1);
    }

    #[test]
    fn pipelined_prepares_on_another_thread_keeps_order_and_stops_early() {
        let here = std::thread::current().id();
        let inputs: Vec<u32> = (0..25).collect();
        let prepared_on = std::sync::Mutex::new(Vec::new());
        let prep = |i: &u32| -> Result<u32, EmbedError> {
            prepared_on
                .lock()
                .unwrap()
                .push(std::thread::current().id());
            Ok(i * 2)
        };
        let mut ran = Vec::new();
        let mut batches = Vec::new();
        pipelined(
            &inputs,
            8,
            &prep,
            &mut |p| {
                ran.push(p);
                Ok(vec![p as f32])
            },
            &mut |first, vectors| {
                batches.push((
                    first,
                    vectors.iter().map(|v| v[0] as u32).collect::<Vec<_>>(),
                ));
                Ok(())
            },
        )
        .unwrap();
        assert!(prepared_on.lock().unwrap().iter().all(|id| *id != here));
        assert_eq!(ran, (0..25).map(|i| i * 2).collect::<Vec<_>>());
        assert_eq!(batches.len(), 4);
        assert_eq!(batches[3], (24, vec![48]));
        assert_eq!(batches[1].0, 8);

        // A failing `run` stops the preparing thread too (it never prepares everything).
        let count = std::sync::atomic::AtomicUsize::new(0);
        let many: Vec<u32> = (0..10_000).collect();
        let err = pipelined(
            &many,
            4,
            &|i: &u32| {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(*i)
            },
            &mut |p| {
                if p == 5 {
                    Err(EmbedError::Embed("model failed".into()))
                } else {
                    Ok(vec![0.0])
                }
            },
            &mut |_, _| Ok(()),
        )
        .unwrap_err();
        assert!(matches!(err, EmbedError::Embed(_)));
        assert!(count.load(std::sync::atomic::Ordering::SeqCst) < 100);

        // A failing `prep` is returned as is.
        let err = pipelined(
            &inputs,
            4,
            &|i: &u32| {
                if *i == 3 {
                    Err(EmbedError::Extract("bad input".into()))
                } else {
                    Ok(*i)
                }
            },
            &mut |p| Ok(vec![p as f32]),
            &mut |_, _| Ok(()),
        )
        .unwrap_err();
        assert!(matches!(err, EmbedError::Extract(_)));
    }

    #[test]
    fn formats_come_from_the_name_then_the_contents() {
        assert_eq!(TextFormat::detect("a.md", ""), TextFormat::Markdown);
        assert_eq!(TextFormat::detect("a.RST", ""), TextFormat::Rst);
        assert_eq!(TextFormat::detect("a.tex", ""), TextFormat::Latex);
        assert_eq!(TextFormat::detect("a.htm", ""), TextFormat::Html);
        assert_eq!(
            TextFormat::detect("README", "# Title\n\nText."),
            TextFormat::Markdown
        );
        assert_eq!(
            TextFormat::detect("notes.txt", "Title\n=====\n\nText."),
            TextFormat::Rst
        );
        assert_eq!(
            TextFormat::detect("paper", "\\documentclass{article}\n\\section{Intro}\nText."),
            TextFormat::Latex
        );
        assert_eq!(
            TextFormat::detect("notes.txt", "Just some text.\nMore."),
            TextFormat::Plain
        );
    }

    #[test]
    fn media_file_names() {
        assert_eq!(page_file(3), "page-3.png");
        assert_eq!(frame_file(12_500), "frame-12500.jpg");
        assert_eq!(ImageTokens::High.count(), 280);
    }
}
