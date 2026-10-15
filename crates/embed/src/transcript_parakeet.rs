//! Speech to text in-process with NVIDIA Parakeet TDT 0.6B v3 on ONNX Runtime
//! (`parakeet-rs`, the same `ort` the ONNX embedder runs on): the Linux sibling of
//! `transcript.rs`, which runs the same model family through `fluidaudiocli` on the Mac
//! (`docs/plan-linux-service.md` §2.6). It gives the same `Transcript` (words with their
//! start and end times), so `Transcript::chunks` and `lines` work unchanged.
//!
//! The model is `istupakov/parakeet-tdt-0.6b-v3-onnx` (CC-BY-4.0, NVIDIA's weights
//! exported to ONNX), pinned to one revision, downloaded to
//! `data_dir/models/parakeet-tdt-0.6b-v3-{int8,fp32}/` like the embedder. int8 is the
//! default: ~670 MB on disk against ~2.5 GB for fp32.
//!
//! Long recordings are cut into windows of at most `WINDOW_SAMPLES` (60 s), each ending
//! at the quietest 100 ms of its last `CUT_SEARCH_SAMPLES` so a cut rarely splits a word;
//! the model attends over a whole window at once, so its memory grows with the window
//! (spike on an M4 Pro, int8, 4 threads: 60 s → 2.0 GB RSS, 120 s → 3.1 GB, 300 s →
//! 5.9 GB; `parakeet-rs` documents a hard failure past ~8–10 minutes).
//!
//! The model loads on first use and is dropped after `SPEECH_IDLE` unused
//! (`Parakeet::sweep`), so a service that isn't transcribing doesn't hold it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::onnx::IdleSlot;
use crate::{EmbedError, SAMPLE_RATE, Transcript, Word};

/// Hugging Face repo of the ONNX export `parakeet-rs` reads.
pub const PARAKEET_REPO: &str = "istupakov/parakeet-tdt-0.6b-v3-onnx";
/// The repo revision downloads are pinned to (2025's export; the file list below is
/// checked against it).
pub const PARAKEET_REVISION: &str = "8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce";
/// The speech model is dropped after this long unused.
pub const SPEECH_IDLE: Duration = Duration::from_secs(10 * 60);
/// The longest stretch of audio the model hears at once: 60 s.
pub const WINDOW_SAMPLES: usize = 60 * SAMPLE_RATE as usize;
/// A window ends at the quietest frame of its last 8 s…
const CUT_SEARCH_SAMPLES: usize = 8 * SAMPLE_RATE as usize;
/// …measured over 100 ms frames.
const CUT_FRAME_SAMPLES: usize = SAMPLE_RATE as usize / 10;
/// Windows shorter than this (a stray tail) are not sent to the model: the encoder
/// subsamples 8× and needs a few frames to produce anything.
const MIN_WINDOW_SAMPLES: usize = SAMPLE_RATE as usize / 10;
/// Written last by a download: the files and their sizes. A folder without it is an
/// unfinished download.
const FILES_JSON: &str = "files.json";

/// Which export of the weights.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SpeechDtype {
    /// 8-bit weights (`*.int8.onnx`, ~670 MB): the default.
    #[default]
    Int8,
    /// Full precision (`encoder-model.onnx` + its 2.4 GB external data).
    Fp32,
}

impl SpeechDtype {
    /// "int8" or "fp32".
    pub fn as_str(self) -> &'static str {
        match self {
            SpeechDtype::Int8 => "int8",
            SpeechDtype::Fp32 => "fp32",
        }
    }

    /// The folder under `data_dir/models/` this export is kept in.
    pub fn dir_name(self) -> String {
        format!("parakeet-tdt-0.6b-v3-{}", self.as_str())
    }

    /// The repo files this export needs: the vocabulary, the config, the encoder (with
    /// its external data for fp32) and the decoder/joint graph. Only one export per
    /// folder: `parakeet-rs` picks `encoder-model.onnx` over the int8 file when both
    /// are present.
    pub fn files(self) -> &'static [&'static str] {
        match self {
            SpeechDtype::Int8 => &[
                "config.json",
                "vocab.txt",
                "decoder_joint-model.int8.onnx",
                "encoder-model.int8.onnx",
            ],
            SpeechDtype::Fp32 => &[
                "config.json",
                "vocab.txt",
                "decoder_joint-model.onnx",
                "encoder-model.onnx",
                "encoder-model.onnx.data",
            ],
        }
    }
}

impl std::fmt::Display for SpeechDtype {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SpeechDtype {
    type Err = EmbedError;
    fn from_str(s: &str) -> Result<Self, EmbedError> {
        match s {
            "int8" => Ok(SpeechDtype::Int8),
            "fp32" => Ok(SpeechDtype::Fp32),
            other => Err(EmbedError::Load(format!(
                "unknown speech model format {other:?} (int8 or fp32)"
            ))),
        }
    }
}

/// A file of the download, with its size from the repo listing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeechFile {
    pub path: String,
    pub size: u64,
}

/// The speech model's download in one folder (`data_dir/models/{SpeechDtype::dir_name}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechModelFiles {
    pub dir: PathBuf,
    pub dtype: SpeechDtype,
}

#[derive(Deserialize)]
struct TreeEntry {
    #[serde(rename = "type")]
    kind: String,
    path: String,
    #[serde(default)]
    size: u64,
}

impl SpeechModelFiles {
    pub fn new(dir: impl Into<PathBuf>, dtype: SpeechDtype) -> Self {
        Self {
            dir: dir.into(),
            dtype,
        }
    }

    /// The default export in `models_dir/{dir_name}`.
    pub fn in_models_dir(models_dir: &Path, dtype: SpeechDtype) -> Self {
        Self::new(models_dir.join(dtype.dir_name()), dtype)
    }

    /// Whether `files.json` lists every file this export needs and each is present at
    /// its size.
    pub fn is_complete(&self) -> bool {
        let Ok(bytes) = std::fs::read(self.dir.join(FILES_JSON)) else {
            return false;
        };
        let Ok(files) = serde_json::from_slice::<Vec<SpeechFile>>(&bytes) else {
            return false;
        };
        self.dtype
            .files()
            .iter()
            .all(|name| files.iter().any(|f| f.path == *name))
            && files.iter().all(|f| {
                std::fs::metadata(self.dir.join(&f.path))
                    .is_ok_and(|m| m.is_file() && m.len() == f.size)
            })
    }

    /// Bytes on disk under the folder.
    pub fn size_on_disk(&self) -> u64 {
        fn walk(dir: &Path) -> u64 {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return 0;
            };
            entries
                .flatten()
                .map(|e| match e.file_type() {
                    Ok(t) if t.is_dir() => walk(&e.path()),
                    Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
                    _ => 0,
                })
                .sum()
        }
        walk(&self.dir)
    }

    /// The files this export downloads, with sizes, from
    /// `{hf_base}/api/models/{PARAKEET_REPO}/tree/{PARAKEET_REVISION}`. An error if the
    /// repo lacks one.
    pub async fn remote_files(
        &self,
        client: &reqwest::Client,
        hf_base: &str,
    ) -> Result<Vec<SpeechFile>, EmbedError> {
        let url = format!(
            "{}/api/models/{PARAKEET_REPO}/tree/{PARAKEET_REVISION}",
            hf_base.trim_end_matches('/')
        );
        let tree: Vec<TreeEntry> = client
            .get(&url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| EmbedError::Download(e.to_string()))?
            .json()
            .await
            .map_err(|e| EmbedError::Download(format!("unexpected file list: {e}")))?;
        self.dtype
            .files()
            .iter()
            .map(|name| {
                tree.iter()
                    .find(|e| e.kind == "file" && e.path == *name)
                    .map(|e| SpeechFile {
                        path: e.path.clone(),
                        size: e.size,
                    })
                    .ok_or_else(|| EmbedError::Download(format!("{PARAKEET_REPO} has no {name}")))
            })
            .collect()
    }

    /// Total bytes the download fetches, to show before it starts.
    pub async fn remote_bytes(
        &self,
        client: &reqwest::Client,
        hf_base: &str,
    ) -> Result<u64, EmbedError> {
        Ok(self
            .remote_files(client, hf_base)
            .await?
            .iter()
            .map(|f| f.size)
            .sum())
    }

    /// Downloads the export's files into the folder, each into `{name}.part` first and
    /// renamed once its size checks out; files already there at the right size (an
    /// interrupted download) are kept. `files.json` is written last.
    /// `progress(received, total)` is called as bytes arrive.
    pub async fn download(
        &self,
        client: &reqwest::Client,
        hf_base: &str,
        progress: &(dyn Fn(u64, u64) + Send + Sync),
    ) -> Result<(), EmbedError> {
        let files = self.remote_files(client, hf_base).await?;
        let total: u64 = files.iter().map(|f| f.size).sum();
        tokio::fs::create_dir_all(&self.dir).await?;
        let listing = self.dir.join(FILES_JSON);
        if tokio::fs::metadata(&listing).await.is_ok() {
            tokio::fs::remove_file(&listing).await?;
        }
        let mut received = 0u64;
        progress(received, total);
        for file in &files {
            let target = self.dir.join(&file.path);
            if tokio::fs::metadata(&target)
                .await
                .is_ok_and(|m| m.is_file() && m.len() == file.size)
            {
                received += file.size;
                progress(received, total);
                continue;
            }
            let url = format!(
                "{}/{PARAKEET_REPO}/resolve/{PARAKEET_REVISION}/{}",
                hf_base.trim_end_matches('/'),
                file.path
            );
            let partial = self.dir.join(format!("{}.part", file.path));
            let start = received;
            let written = match fetch(client, &url, &partial, |n| {
                received += n;
                progress(received, total);
            })
            .await
            {
                Ok(n) => n,
                Err(e) => {
                    let _ = tokio::fs::remove_file(&partial).await;
                    return Err(e);
                }
            };
            if written != file.size {
                let _ = tokio::fs::remove_file(&partial).await;
                return Err(EmbedError::Download(format!(
                    "{} is {written} bytes, expected {}",
                    file.path, file.size
                )));
            }
            received = start + written;
            tokio::fs::rename(&partial, &target).await?;
        }
        let json =
            serde_json::to_vec_pretty(&files).map_err(|e| EmbedError::Download(e.to_string()))?;
        let partial = self.dir.join(format!("{FILES_JSON}.part"));
        tokio::fs::write(&partial, json).await?;
        tokio::fs::rename(&partial, &listing).await?;
        Ok(())
    }

    /// Deletes the folder.
    pub fn remove(&self) -> Result<(), EmbedError> {
        match std::fs::remove_dir_all(&self.dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

async fn fetch(
    client: &reqwest::Client,
    url: &str,
    to: &Path,
    mut on_bytes: impl FnMut(u64),
) -> Result<u64, EmbedError> {
    let response = client
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| EmbedError::Download(e.to_string()))?;
    let mut out = tokio::fs::File::create(to).await?;
    let mut stream = response.bytes_stream();
    let mut written = 0u64;
    while let Some(piece) = stream.next().await {
        let piece = piece.map_err(|e| EmbedError::Download(e.to_string()))?;
        out.write_all(&piece).await?;
        written += piece.len() as u64;
        on_bytes(piece.len() as u64);
    }
    out.flush().await?;
    out.sync_all().await?;
    Ok(written)
}

/// A word (or a punctuation mark on its own) as the recognizer gives it, with times in
/// seconds from the start of the audio it was given.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimedWord {
    pub text: String,
    pub start: f32,
    pub end: f32,
}

/// Turns one window of 16 kHz mono samples into timed words. The real one is Parakeet;
/// tests use a fake.
pub trait Recognizer: Send {
    fn recognize(&mut self, samples: &[f32]) -> Result<Vec<TimedWord>, EmbedError>;
}

/// Parakeet TDT through `parakeet-rs`, on the CPU.
pub struct ParakeetRecognizer(parakeet_rs::ParakeetTDT);

impl ParakeetRecognizer {
    /// Loads the export in `files` with `threads` intra-op threads. Blocking.
    pub fn load(files: &SpeechModelFiles, threads: usize) -> Result<Self, EmbedError> {
        if !files.is_complete() {
            return Err(EmbedError::NotDownloaded);
        }
        let env = crate::onnx::session::environment()?.clone();
        let config = parakeet_rs::ExecutionConfig::new()
            .with_environment(env)
            .with_execution_provider(parakeet_rs::ExecutionProvider::Cpu)
            .with_intra_threads(threads.max(1));
        parakeet_rs::ParakeetTDT::from_pretrained(&files.dir, Some(config))
            .map(Self)
            .map_err(|e| EmbedError::Load(format!("the speech model: {e}")))
    }
}

impl Recognizer for ParakeetRecognizer {
    fn recognize(&mut self, samples: &[f32]) -> Result<Vec<TimedWord>, EmbedError> {
        use parakeet_rs::Transcriber;
        let result = self
            .0
            .transcribe_samples(
                samples.to_vec(),
                SAMPLE_RATE,
                1,
                Some(parakeet_rs::TimestampMode::Words),
            )
            .map_err(|e| EmbedError::Transcribe(e.to_string()))?;
        Ok(result
            .tokens
            .into_iter()
            .map(|t| TimedWord {
                text: t.text,
                start: t.start,
                end: t.end,
            })
            .collect())
    }
}

/// How `Parakeet` loads its recognizer.
type Loader =
    dyn Fn(&SpeechModelFiles, usize) -> Result<Box<dyn Recognizer>, EmbedError> + Send + Sync;

/// The speech recognizer the service keeps: loads Parakeet on first use and drops it
/// after `SPEECH_IDLE` unused.
pub struct Parakeet {
    files: SpeechModelFiles,
    threads: usize,
    slot: IdleSlot<Mutex<Box<dyn Recognizer>>>,
    load: Box<Loader>,
}

impl Parakeet {
    /// A recognizer for the download in `files`, run on `threads` threads. Nothing is
    /// loaded until the first `transcribe`.
    pub fn new(files: SpeechModelFiles, threads: usize) -> Self {
        Self::with_loader(files, threads, |files, threads| {
            Ok(Box::new(ParakeetRecognizer::load(files, threads)?) as Box<dyn Recognizer>)
        })
    }

    /// `new` with another way to load the recognizer (tests).
    pub fn with_loader(
        files: SpeechModelFiles,
        threads: usize,
        load: impl Fn(&SpeechModelFiles, usize) -> Result<Box<dyn Recognizer>, EmbedError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            files,
            threads,
            slot: IdleSlot::new(SPEECH_IDLE),
            load: Box::new(load),
        }
    }

    pub fn files(&self) -> &SpeechModelFiles {
        &self.files
    }

    /// Transcribes 16 kHz mono `samples`, loading the model if it isn't loaded.
    /// Blocking; one recording at a time (a second caller waits).
    pub fn transcribe(&self, samples: &[f32]) -> Result<Transcript, EmbedError> {
        if samples.is_empty() {
            return Ok(Transcript::default());
        }
        let recognizer: Arc<Mutex<Box<dyn Recognizer>>> = self.slot.get(Instant::now(), || {
            (self.load)(&self.files, self.threads).map(Mutex::new)
        })?;
        let mut recognizer = recognizer
            .lock()
            .map_err(|_| EmbedError::Transcribe("the speech model failed earlier".into()))?;
        transcribe_with(recognizer.as_mut(), samples)
    }

    /// Drops the model if unused for `SPEECH_IDLE`. Returns whether it did.
    pub fn sweep(&self) -> bool {
        self.slot.sweep(Instant::now())
    }

    pub fn is_loaded(&self) -> bool {
        self.slot.is_loaded()
    }

    /// Drops the model now (before its files are deleted). Returns whether it was loaded.
    pub fn unload(&self) -> bool {
        self.slot.clear()
    }
}

/// Transcribes `samples` window by window with `recognizer` (see `windows`) and joins
/// the words, with times from the start of the recording.
pub fn transcribe_with(
    recognizer: &mut dyn Recognizer,
    samples: &[f32],
) -> Result<Transcript, EmbedError> {
    let mut words: Vec<Word> = Vec::new();
    for (start, window) in windows(samples) {
        if window.len() < MIN_WINDOW_SAMPLES {
            continue;
        }
        let offset_ms = crate::sample_ms(start);
        let floor = words.last().map_or(0, |w| w.end_ms);
        let found = recognizer.recognize(window)?;
        append_words(&mut words, &found, offset_ms, floor);
    }
    Ok(Transcript { words })
}

/// Splits 16 kHz mono samples into windows of at most `WINDOW_SAMPLES`, each (but the
/// last) ending in the middle of the quietest `CUT_FRAME_SAMPLES` frame of its last
/// `CUT_SEARCH_SAMPLES`. Returns each window's first sample with the slice; together they
/// cover every sample once.
pub fn windows(samples: &[f32]) -> Vec<(usize, &[f32])> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < samples.len() {
        let rest = samples.len() - start;
        if rest <= WINDOW_SAMPLES {
            out.push((start, &samples[start..]));
            break;
        }
        let end = start + quietest_cut(&samples[start..start + WINDOW_SAMPLES]);
        out.push((start, &samples[start..end]));
        start = end;
    }
    out
}

/// Where to end a full window: the middle of its quietest frame among the last
/// `CUT_SEARCH_SAMPLES` (the first such frame on a tie, so silence cuts early).
fn quietest_cut(window: &[f32]) -> usize {
    let from = window.len().saturating_sub(CUT_SEARCH_SAMPLES);
    let mut best = (f32::INFINITY, window.len());
    let mut at = from;
    while at + CUT_FRAME_SAMPLES <= window.len() {
        let energy: f32 = window[at..at + CUT_FRAME_SAMPLES]
            .iter()
            .map(|s| s * s)
            .sum();
        if energy < best.0 {
            best = (energy, at + CUT_FRAME_SAMPLES / 2);
        }
        at += CUT_FRAME_SAMPLES;
    }
    best.1
}

/// Whether `text` is punctuation only (Parakeet's word mode gives a comma or a full stop
/// as a token of its own).
fn is_punctuation(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_punctuation() || "。，！？、…“”‘’«»".contains(c))
}

/// Adds a window's words to `words`: times shifted by `offset_ms` and never before
/// `floor_ms` (the previous window's last word), punctuation joined to the word before
/// it, blanks dropped.
fn append_words(words: &mut Vec<Word>, found: &[TimedWord], offset_ms: u64, floor_ms: u64) {
    let ms = |s: f32| offset_ms + (f64::from(s.max(0.0)) * 1000.0).round() as u64;
    for w in found {
        let text = w.text.trim();
        if text.is_empty() {
            continue;
        }
        if is_punctuation(text) {
            if let Some(last) = words.last_mut() {
                last.text.push_str(text);
            }
            continue;
        }
        let start_ms = ms(w.start).max(floor_ms);
        let start_ms = words.last().map_or(start_ms, |l| start_ms.max(l.start_ms));
        words.push(Word {
            text: text.to_string(),
            start_ms,
            end_ms: ms(w.end).max(start_ms),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// What Parakeet (int8, this revision) gave in word mode for 12.3 s of macOS `say`
    /// reading "Photosynthesis turns light energy into chemical energy. Plants take in
    /// carbon dioxide and water, and release oxygen. The Fourier series decomposes a
    /// periodic signal into a sum of sines and cosines." (the spike, 2026-10-14; first
    /// fifteen tokens, as recorded).
    const RECORDED: &str = r#"[
      {"text": "Photosynthesis", "start": 0.0, "end": 1.04},
      {"text": "turns", "start": 1.04, "end": 1.28},
      {"text": "light", "start": 1.28, "end": 1.60},
      {"text": "energy", "start": 1.60, "end": 2.00},
      {"text": "into", "start": 2.00, "end": 2.24},
      {"text": "chemical", "start": 2.24, "end": 2.72},
      {"text": "energy", "start": 2.72, "end": 3.12},
      {"text": ",", "start": 3.12, "end": 3.36},
      {"text": "plants", "start": 3.36, "end": 3.76},
      {"text": "take", "start": 3.76, "end": 4.00},
      {"text": "in", "start": 4.00, "end": 4.16},
      {"text": "carbon", "start": 4.16, "end": 4.56},
      {"text": "dioxide", "start": 4.56, "end": 5.28},
      {"text": "and", "start": 5.28, "end": 5.44},
      {"text": "water", "start": 5.44, "end": 5.76}
    ]"#;

    /// Gives the same recorded words for every window and remembers each window's length.
    struct Fake {
        words: Vec<TimedWord>,
        lengths: Vec<usize>,
    }

    impl Recognizer for Fake {
        fn recognize(&mut self, samples: &[f32]) -> Result<Vec<TimedWord>, EmbedError> {
            self.lengths.push(samples.len());
            Ok(self.words.clone())
        }
    }

    fn recorded() -> Vec<TimedWord> {
        serde_json::from_str(RECORDED).unwrap()
    }

    #[test]
    fn recorded_words_become_a_transcript() {
        let mut fake = Fake {
            words: recorded(),
            lengths: Vec::new(),
        };
        let samples = vec![0.01f32; 12 * SAMPLE_RATE as usize];
        let t = transcribe_with(&mut fake, &samples).unwrap();
        assert_eq!(fake.lengths, vec![samples.len()]);
        let texts: Vec<&str> = t.words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(
            texts.join(" "),
            "Photosynthesis turns light energy into chemical energy, plants take in carbon dioxide and water"
        );
        assert_eq!((t.words[0].start_ms, t.words[0].end_ms), (0, 1040));
        // The comma joins "energy" and keeps its times.
        assert_eq!(
            t.words[6],
            Word {
                text: "energy,".into(),
                start_ms: 2720,
                end_ms: 3120
            }
        );
        // Ready for the passages and lines the service keeps.
        let chunks = t.chunks(&|s| s.split_whitespace().count(), 1000);
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            (chunks[0].start_ms, chunks[0].end_ms),
            (Some(0), Some(5760))
        );
        assert!(!t.lines().is_empty());
    }

    #[test]
    fn long_audio_is_cut_at_quiet_points_and_times_carry_on() {
        // 150 s of tone with a silent 100 ms frame at 55 s, and one 57 s into the second
        // window (on its frame grid).
        let mut samples = vec![0.5f32; 150 * SAMPLE_RATE as usize];
        let rate = SAMPLE_RATE as usize;
        let quiet1 = 55 * rate;
        let cut1 = quiet1 + CUT_FRAME_SAMPLES / 2;
        let quiet2 = cut1 + 57 * rate;
        let cut2 = quiet2 + CUT_FRAME_SAMPLES / 2;
        samples[quiet1..quiet1 + CUT_FRAME_SAMPLES].fill(0.0);
        samples[quiet2..quiet2 + CUT_FRAME_SAMPLES].fill(0.0);
        let windows = windows(&samples);
        let starts: Vec<usize> = windows.iter().map(|(s, _)| *s).collect();
        assert_eq!(starts, vec![0, cut1, cut2]);
        assert!(windows.iter().all(|(_, w)| w.len() <= WINDOW_SAMPLES));
        assert_eq!(
            windows.iter().map(|(_, w)| w.len()).sum::<usize>(),
            samples.len()
        );

        let mut fake = Fake {
            words: recorded(),
            lengths: Vec::new(),
        };
        let t = transcribe_with(&mut fake, &samples).unwrap();
        assert_eq!(fake.lengths.len(), 3);
        // The second window's first word starts at its offset.
        let per_window = 14; // 15 tokens, the comma joined
        assert_eq!(t.words.len(), 3 * per_window);
        assert_eq!(t.words[per_window].start_ms, crate::sample_ms(cut1));
        assert_eq!(t.words[2 * per_window].start_ms, crate::sample_ms(cut2));
        assert!(t.words.windows(2).all(|p| p[0].start_ms <= p[1].start_ms));
    }

    #[test]
    fn windows_without_quiet_cut_at_full_length_and_tails_are_kept() {
        // Constant noise: every frame ties, so the first frame of the search area wins.
        let samples = vec![0.3f32; WINDOW_SAMPLES * 2 + 1000];
        let windows = windows(&samples);
        let first = WINDOW_SAMPLES - CUT_SEARCH_SAMPLES + CUT_FRAME_SAMPLES / 2;
        assert_eq!(windows[0].1.len(), first);
        assert_eq!(
            windows.iter().map(|(_, w)| w.len()).sum::<usize>(),
            samples.len()
        );
        assert!(super::windows(&[]).is_empty());
        let short = vec![0.0f32; 100];
        assert_eq!(super::windows(&short), vec![(0, &short[..])]);
    }

    #[test]
    fn tiny_windows_and_silence_give_no_words() {
        let mut fake = Fake {
            words: recorded(),
            lengths: Vec::new(),
        };
        // Shorter than the model can hear: not sent.
        let t = transcribe_with(&mut fake, &[0.0; 800]).unwrap();
        assert!(t.words.is_empty());
        assert!(fake.lengths.is_empty());
    }

    #[test]
    fn punctuation_blanks_and_overlaps_are_cleaned() {
        let mut words = Vec::new();
        let found = vec![
            TimedWord {
                text: ".".into(),
                start: 0.0,
                end: 0.1,
            }, // nothing to join: dropped
            TimedWord {
                text: " Hello".into(),
                start: 0.2,
                end: 0.5,
            },
            TimedWord {
                text: "?".into(),
                start: 0.5,
                end: 0.6,
            },
            TimedWord {
                text: "  ".into(),
                start: 0.6,
                end: 0.7,
            },
            TimedWord {
                text: "again".into(),
                start: 0.1,
                end: 0.05,
            }, // goes back in time
        ];
        append_words(&mut words, &found, 1000, 1100);
        assert_eq!(
            words,
            vec![
                Word {
                    text: "Hello?".into(),
                    start_ms: 1200,
                    end_ms: 1500
                },
                Word {
                    text: "again".into(),
                    start_ms: 1200,
                    end_ms: 1200
                },
            ]
        );
        assert!(is_punctuation("…"));
        assert!(!is_punctuation("e.g."));
    }

    #[test]
    fn the_model_loads_once_and_unloads_when_idle() {
        let loads = Arc::new(AtomicUsize::new(0));
        let counted = loads.clone();
        let parakeet = Parakeet::with_loader(
            SpeechModelFiles::new("/nonexistent", SpeechDtype::Int8),
            2,
            move |_, threads| {
                assert_eq!(threads, 2);
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(Fake {
                    words: recorded(),
                    lengths: Vec::new(),
                }) as Box<dyn Recognizer>)
            },
        );
        assert!(!parakeet.is_loaded());
        assert_eq!(parakeet.transcribe(&[]).unwrap(), Transcript::default());
        assert_eq!(loads.load(Ordering::SeqCst), 0);
        let samples = vec![0.1f32; SAMPLE_RATE as usize];
        parakeet.transcribe(&samples).unwrap();
        parakeet.transcribe(&samples).unwrap();
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert!(parakeet.is_loaded());
        // Just used: kept.
        assert!(!parakeet.sweep());
        assert!(parakeet.slot.sweep(Instant::now() + SPEECH_IDLE));
        assert!(!parakeet.is_loaded());
        parakeet.transcribe(&samples).unwrap();
        assert_eq!(loads.load(Ordering::SeqCst), 2);
        // Unloaded on demand (its files are about to be deleted), used or not.
        assert!(parakeet.unload());
        assert!(!parakeet.is_loaded());
        assert!(!parakeet.unload());
    }

    #[test]
    fn a_missing_download_is_not_downloaded() {
        let files = SpeechModelFiles::new("/nonexistent/parakeet", SpeechDtype::Int8);
        assert!(!files.is_complete());
        assert!(matches!(
            ParakeetRecognizer::load(&files, 1),
            Err(EmbedError::NotDownloaded)
        ));
        let parakeet = Parakeet::new(files, 1);
        assert!(matches!(
            parakeet.transcribe(&[0.0; 16_000]),
            Err(EmbedError::NotDownloaded)
        ));
    }

    #[test]
    fn dtypes_name_their_files_and_folders() {
        assert_eq!(SpeechDtype::default(), SpeechDtype::Int8);
        assert_eq!(SpeechDtype::Int8.dir_name(), "parakeet-tdt-0.6b-v3-int8");
        assert!(
            SpeechDtype::Int8
                .files()
                .iter()
                .all(|f| !f.ends_with("model.onnx"))
        );
        assert!(
            SpeechDtype::Fp32
                .files()
                .contains(&"encoder-model.onnx.data")
        );
        assert_eq!("fp32".parse::<SpeechDtype>().unwrap(), SpeechDtype::Fp32);
        assert!("q8".parse::<SpeechDtype>().is_err());
        let files = SpeechModelFiles::in_models_dir(Path::new("/m"), SpeechDtype::Fp32);
        assert_eq!(files.dir, Path::new("/m/parakeet-tdt-0.6b-v3-fp32"));
    }

    // A fake Hugging Face repo for the download.
    mod download {
        use super::*;
        use axum::Router;
        use axum::extract::{Path as UrlPath, State};
        use axum::http::StatusCode;
        use axum::routing::get;

        type Repo = Arc<Vec<(String, Vec<u8>)>>;

        fn repo() -> Repo {
            Arc::new(vec![
                ("config.json".into(), b"{\"features_size\": 128}".to_vec()),
                ("vocab.txt".into(), b"<unk> 0\n".to_vec()),
                ("decoder_joint-model.int8.onnx".into(), vec![1; 3000]),
                ("encoder-model.int8.onnx".into(), vec![2; 9000]),
                ("encoder-model.onnx".into(), vec![3; 500]),
                ("README.md".into(), b"readme".to_vec()),
            ])
        }

        async fn serve(repo: Repo, broken: Option<&'static str>) -> String {
            async fn tree(
                State((repo, _)): State<(Repo, Option<&'static str>)>,
                UrlPath((_, _, rev)): UrlPath<(String, String, String)>,
            ) -> Result<axum::Json<serde_json::Value>, StatusCode> {
                if rev != PARAKEET_REVISION {
                    return Err(StatusCode::NOT_FOUND);
                }
                Ok(axum::Json(serde_json::Value::Array(
                    repo.iter()
                        .map(|(p, b)| serde_json::json!({"type": "file", "path": p, "size": b.len()}))
                        .collect(),
                )))
            }
            async fn file(
                State((repo, broken)): State<(Repo, Option<&'static str>)>,
                UrlPath((_, _, rev, path)): UrlPath<(String, String, String, String)>,
            ) -> Result<Vec<u8>, StatusCode> {
                if rev != PARAKEET_REVISION {
                    return Err(StatusCode::NOT_FOUND);
                }
                let (_, bytes) = repo
                    .iter()
                    .find(|(p, _)| *p == path)
                    .ok_or(StatusCode::NOT_FOUND)?;
                if broken == Some(path.as_str()) {
                    return Ok(bytes[..bytes.len() / 2].to_vec());
                }
                Ok(bytes.clone())
            }
            let app = Router::new()
                .route("/api/models/{owner}/{name}/tree/{rev}", get(tree))
                .route("/{owner}/{name}/resolve/{rev}/{*path}", get(file))
                .with_state((repo, broken));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            format!("http://{addr}")
        }

        fn temp_dir(name: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "margin-speech-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            dir
        }

        #[tokio::test]
        async fn downloads_the_int8_export_only() {
            let base = serve(repo(), None).await;
            let client = reqwest::Client::new();
            let files = SpeechModelFiles::new(temp_dir("int8"), SpeechDtype::Int8);
            assert_eq!(
                files.remote_bytes(&client, &base).await.unwrap(),
                22 + 8 + 3000 + 9000
            );
            let last = Mutex::new((0, 0));
            files
                .download(&client, &base, &|r, t| *last.lock().unwrap() = (r, t))
                .await
                .unwrap();
            assert_eq!(*last.lock().unwrap(), (12_030, 12_030));
            assert!(files.is_complete());
            assert!(!files.dir.join("encoder-model.onnx").exists());
            assert!(!files.dir.join("README.md").exists());
            // A file changed on disk: no longer complete.
            std::fs::write(files.dir.join("vocab.txt"), b"x").unwrap();
            assert!(!files.is_complete());
            files.remove().unwrap();
            assert!(!files.dir.exists());
            files.remove().unwrap();
        }

        #[tokio::test]
        async fn a_short_file_or_a_missing_one_fails() {
            let base = serve(repo(), Some("encoder-model.int8.onnx")).await;
            let client = reqwest::Client::new();
            let files = SpeechModelFiles::new(temp_dir("short"), SpeechDtype::Int8);
            let err = files
                .download(&client, &base, &|_, _| {})
                .await
                .unwrap_err();
            assert!(err.to_string().contains("expected 9000"), "{err}");
            assert!(!files.is_complete());
            assert!(!files.dir.join("encoder-model.int8.onnx.part").exists());
            // fp32 needs graphs the repo lacks.
            let fp32 = SpeechModelFiles::new(temp_dir("fp32"), SpeechDtype::Fp32);
            let err = fp32.remote_bytes(&client, &base).await.unwrap_err();
            assert!(
                err.to_string().contains("has no decoder_joint-model.onnx"),
                "{err}"
            );
            let _ = files.remove();
        }
    }

    /// Downloads the real int8 export from Hugging Face when `MARGIN_PARAKEET_DOWNLOAD_TO`
    /// names a folder.
    #[tokio::test]
    async fn real_download_when_asked() {
        let Some(dir) = std::env::var_os("MARGIN_PARAKEET_DOWNLOAD_TO") else {
            eprintln!("skipping: MARGIN_PARAKEET_DOWNLOAD_TO is not set");
            return;
        };
        let client = reqwest::Client::new();
        let files = SpeechModelFiles::new(PathBuf::from(dir), SpeechDtype::Int8);
        let total = files.remote_bytes(&client, crate::HF_BASE).await.unwrap();
        eprintln!("downloading {total} bytes to {}", files.dir.display());
        files
            .download(&client, crate::HF_BASE, &|_, _| {})
            .await
            .unwrap();
        assert!(files.is_complete());
        assert!(files.size_on_disk() >= total);
    }

    /// The real model on real speech: needs `MARGIN_REAL_MODEL_TESTS=1`, a finished
    /// download in `MARGIN_PARAKEET_DIR` (int8), and a speech synthesizer to make the
    /// audio (`say` on macOS, `espeak-ng` on Linux).
    #[test]
    fn real_model_transcribes_speech() {
        if !crate::real_model_tests() {
            return;
        }
        let Some(dir) = std::env::var_os("MARGIN_PARAKEET_DIR") else {
            eprintln!("skipping: MARGIN_PARAKEET_DIR is not set");
            return;
        };
        let work = std::env::temp_dir().join(format!("margin-speech-real-{}", std::process::id()));
        std::fs::create_dir_all(&work).unwrap();
        let wav = work.join("speech.wav");
        let text = "Photosynthesis turns light energy into chemical energy. \
                    The Fourier series decomposes a periodic signal into sines and cosines.";
        let made = if cfg!(target_os = "macos") {
            std::process::Command::new("say")
                .args(["--file-format=WAVE", "--data-format=LEI16@16000", "-o"])
                .arg(&wav)
                .arg(text)
                .status()
        } else {
            std::process::Command::new("espeak-ng")
                .arg("-w")
                .arg(&wav)
                .arg(text)
                .status()
        };
        if !made.is_ok_and(|s| s.success()) {
            eprintln!("skipping: no speech synthesizer");
            return;
        }
        let (rate, speech) = read_wav(&std::fs::read(&wav).unwrap());
        let speech = resample(&speech, rate);
        let parakeet = Parakeet::new(SpeechModelFiles::new(dir, SpeechDtype::Int8), 4);
        let t = parakeet.transcribe(&speech).unwrap();
        let said = t.span(0, u64::MAX).to_lowercase();
        eprintln!("{said}");
        for word in ["photosynthesis", "light", "energy", "fourier", "signal"] {
            assert!(said.contains(word), "{word} missing from {said:?}");
        }
        assert!(t.words.windows(2).all(|p| p[0].start_ms <= p[1].start_ms));

        // Seventy seconds: the clip, silence, the clip again… cut into two windows, with
        // the later words at their place in the recording.
        let mut long = Vec::new();
        while long.len() < 70 * SAMPLE_RATE as usize {
            long.extend_from_slice(&speech);
            long.extend(std::iter::repeat_n(0.0, SAMPLE_RATE as usize));
        }
        let started = Instant::now();
        let t = parakeet.transcribe(&long).unwrap();
        eprintln!(
            "{:.1} s of audio in {:?}",
            long.len() as f32 / 16_000.0,
            started.elapsed()
        );
        let last = t.words.last().unwrap();
        assert!(last.end_ms > 60_000, "{last:?}");
        let times = t
            .words
            .iter()
            .filter(|w| w.text.to_lowercase().starts_with("photosynthesis"))
            .count();
        assert!(times >= 4, "{times}");
        let _ = std::fs::remove_dir_all(&work);
    }

    /// The samples (as f32) and rate of a 16-bit PCM mono WAV, walking its chunks.
    fn read_wav(bytes: &[u8]) -> (u32, Vec<f32>) {
        let mut at = 12;
        let mut rate = 0;
        while at + 8 <= bytes.len() {
            let id = &bytes[at..at + 4];
            let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
            let body = &bytes[at + 8..(at + 8 + len).min(bytes.len())];
            if id == b"fmt " {
                rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            } else if id == b"data" {
                let samples = body
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| f32::from(i16::from_le_bytes(*b)) / 32768.0)
                    .collect();
                return (rate, samples);
            }
            at += 8 + len + (len & 1);
        }
        panic!("no data chunk");
    }

    /// Linear resampling to 16 kHz (espeak-ng writes 22.05 kHz).
    fn resample(samples: &[f32], rate: u32) -> Vec<f32> {
        if rate == SAMPLE_RATE {
            return samples.to_vec();
        }
        let n = (samples.len() as u64 * u64::from(SAMPLE_RATE) / u64::from(rate)) as usize;
        (0..n)
            .map(|i| {
                let x = i as f64 * f64::from(rate) / f64::from(SAMPLE_RATE);
                let j = x as usize;
                let f = (x - j as f64) as f32;
                let a = samples[j.min(samples.len() - 1)];
                let b = samples[(j + 1).min(samples.len() - 1)];
                a + (b - a) * f
            })
            .collect()
    }
}
