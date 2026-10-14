//! Local embeddings for sources: EmbeddingGemma 2 (Core ML) run in-process, PDF and text
//! extraction (PDFKit + Vision OCR), chunking, and ranking. See PLAN.md "Sources and
//! embeddings".
//!
//! This file is the contract the service codes against. Implementations may add items but
//! must keep these signatures.

use std::path::{Path, PathBuf};

/// `SourceKind::for_name` decides which files are sources.
pub use margin_shared::SourceKind;

mod chunk;
#[cfg(target_os = "macos")]
mod coreml;
mod download;
mod extract;
pub mod fake;
#[cfg(target_os = "macos")]
mod pdf;
mod rank;

/// Hugging Face repo the model is downloaded from.
pub const REPO: &str = "FluidInference/embeddinggemma-2-coreml";
/// Default Hugging Face base URL; tests point downloads at a local server instead.
pub const HF_BASE: &str = "https://huggingface.co";
/// Embedding size.
pub const DIM: usize = 768;
/// Most tokens one `embed` input may hold (the largest Core ML `embed_S` function),
/// including the task prefix and `<bos>`/`<eos>`.
pub const MAX_TOKENS: usize = 512;
/// Chunk size the service cuts sources into, leaving room for the document prefix.
pub const CHUNK_TOKENS: usize = 480;

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
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// What an input is, which picks the model's task prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// A chunk of a source: `title: none | text: {chunk}`.
    Document,
    /// What we're looking for: `task: search result | query: {text}`.
    Query,
}

/// Turns texts into L2-normalized `DIM`-dimensional vectors. Blocking: the service calls
/// it from `spawn_blocking`.
pub trait Embedder: Send + Sync {
    /// One vector per text. Texts longer than `MAX_TOKENS` (after the prefix) are cut.
    fn embed(&self, texts: &[String], purpose: Purpose) -> Result<Vec<Vec<f32>>, EmbedError>;
    /// Tokens `text` takes with the model's tokenizer (no prefix, no `<bos>`/`<eos>`).
    fn count_tokens(&self, text: &str) -> usize;
}

/// The downloaded model files in one folder (`data_dir/models/embeddinggemma-2`).
#[derive(Debug, Clone)]
pub struct ModelFiles {
    pub dir: PathBuf,
}

impl ModelFiles {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Whether every file the text model needs is present (a finished download).
    pub fn is_complete(&self) -> bool {
        download::is_complete(&self.dir)
    }

    /// Bytes on disk for the downloaded files.
    pub fn size_on_disk(&self) -> u64 {
        download::size_on_disk(&self.dir)
    }

    /// Total bytes a download fetches (from the Hugging Face API), for showing before
    /// the user starts it.
    pub async fn remote_size(client: &reqwest::Client, hf_base: &str) -> Result<u64, EmbedError> {
        let files = download::remote_files(client, hf_base).await?;
        Ok(files.iter().map(|f| f.size).sum())
    }

    /// Downloads the text model's files from `hf_base`/`REPO` (resolve/main) into `dir`,
    /// into temporary names first so an interrupted download is never mistaken for a
    /// complete one. `progress(received, total)` is called as bytes arrive.
    pub async fn download(
        &self,
        client: &reqwest::Client,
        hf_base: &str,
        progress: &(dyn Fn(u64, u64) + Send + Sync),
    ) -> Result<(), EmbedError> {
        download::download(&self.dir, client, hf_base, progress).await
    }

    /// Deletes the downloaded files (and the compiled model cache).
    pub fn remove(&self) -> Result<(), EmbedError> {
        download::remove(&self.dir)
    }
}

/// EmbeddingGemma 2 on Core ML (Neural Engine where possible). macOS only.
#[cfg(target_os = "macos")]
pub struct CoreMlEmbedder(coreml::Model);

#[cfg(target_os = "macos")]
impl CoreMlEmbedder {
    /// Loads the tokenizer and token table and the Core ML text model, compiling the
    /// `.mlpackage` on first use (cached next to it). Blocking.
    pub fn load(files: &ModelFiles) -> Result<Self, EmbedError> {
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
}

/// Text pulled from a source file.
#[derive(Debug, Clone, PartialEq)]
pub struct Extracted {
    pub pages: Vec<Page>,
}

/// A run of text: one page of a PDF (`number` from 1), or the whole of a text/code file
/// (`number` = None).
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub number: Option<u32>,
    pub text: String,
}

/// Extracts text: UTF-8 (lossy) for text and code; PDFKit for PDFs, with Vision OCR for
/// pages that have no text layer. Blocking.
pub fn extract(path: &Path, kind: SourceKind) -> Result<Extracted, EmbedError> {
    extract::extract(path, kind)
}

/// A piece of a source small enough to embed.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub text: String,
    /// PDF page it came from.
    pub page: Option<u32>,
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
