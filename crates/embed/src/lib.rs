//! Local embeddings for sources: EmbeddingGemma 2 (Core ML) run in-process for text,
//! images and audio; reading sources (text and code, PDFKit + Vision OCR, ImageIO,
//! AVFoundation, FluidAudio speech recognition); chunking; and ranking. See PLAN.md
//! "Sources and embeddings".
//!
//! This file is the contract the service codes against. Implementations may add items but
//! must keep these signatures.

use std::path::{Path, PathBuf};

/// `SourceKind::for_name` decides which files are sources.
pub use margin_shared::SourceKind;
pub use transcript::{FLUIDAUDIOCLI, Transcript, Word, find_fluidaudiocli, transcribe};

#[cfg(target_os = "macos")]
mod audio;
#[cfg(target_os = "macos")]
mod av;
mod chunk;
#[cfg(target_os = "macos")]
mod coreml;
mod download;
mod extract;
pub mod fake;
#[cfg(target_os = "macos")]
mod image;
#[cfg(all(test, target_os = "macos"))]
mod media_tests;
#[cfg(target_os = "macos")]
mod pdf;
mod rank;
mod transcript;
#[cfg(target_os = "macos")]
mod vision;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// A chunk of a source: `title: none | text: {chunk}`.
    Document,
    /// What we're looking for: `task: search result | query: {text}`.
    Query,
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

    /// Whether every file of every part (text, vision, audio) is present: a finished
    /// download.
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

    /// Downloads every part of the model (the text model, the vision and audio encoders
    /// and their tables) from `hf_base`/`REPO` (resolve/main) into `dir`, into temporary
    /// names first so an interrupted download is never mistaken for a complete one; files
    /// already there from an earlier attempt are kept. `progress(received, total)` is
    /// called as bytes arrive, with one total for everything.
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

/// Reads a source. Text and code are decoded as UTF-8 (lossy); PDFs with PDFKit (Vision
/// OCR for pages without a text layer), each page also rendered to `page_file(n)` in
/// `media_dir`; images are checked to decode; audio is decoded to 16 kHz mono; videos
/// give `frame_file(ms)` pictures in `media_dir` (see `frame_times`) and their
/// soundtrack. Blocking.
pub fn extract(path: &Path, kind: SourceKind, media_dir: &Path) -> Result<Extracted, EmbedError> {
    extract::extract(path, kind, media_dir)
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

#[cfg(not(target_os = "macos"))]
pub fn jpeg_for_model(_path: &Path, _max_side: usize) -> Result<Vec<u8>, EmbedError> {
    Err(EmbedError::Extract("reading images needs macOS".into()))
}

/// A piece of a source small enough to embed.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub text: String,
    /// PDF page it came from.
    pub page: Option<u32>,
    /// Where in an audio or video source the words are (transcript passages).
    pub start_ms: Option<u64>,
    pub end_ms: Option<u64>,
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
    fn media_file_names() {
        assert_eq!(page_file(3), "page-3.png");
        assert_eq!(frame_file(12_500), "frame-12500.jpg");
        assert_eq!(ImageTokens::High.count(), 280);
    }
}
