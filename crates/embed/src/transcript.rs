//! Speech to text with FluidAudio's command line tool (Parakeet, on the Neural Engine):
//! `fluidaudiocli transcribe <wav> --word-timestamps --output-json <json>` writes the text
//! and each word's start and end time (FluidAudio's
//! `Sources/FluidAudioCLI/Commands/ASR/Parakeet/SlidingWindow/TranscribeCommand.swift`,
//! `TranscriptionJSONOutput`). The sound is handed over as a 16 kHz mono 16-bit WAV.
//!
//! Transcripts are cut into passages of at most 30 seconds (ending at a sentence once
//! one is 20 seconds long) and at most the chunk token limit, so a citation points at a
//! short stretch of the recording.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::{Chunk, EmbedError, SAMPLE_RATE};

/// The tool's name, found on PATH or in `~/.local/bin` (where Margin's install
/// instructions put it).
pub const FLUIDAUDIOCLI: &str = "fluidaudiocli";

/// A passage ends at the first sentence end after this long…
const SOFT_PASSAGE_MS: u64 = 20_000;
/// …and at a word boundary after this long, whatever the words.
const HARD_PASSAGE_MS: u64 = 30_000;

/// What was said, word by word.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Transcript {
    pub words: Vec<Word>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    /// The word with its punctuation, e.g. "energy,".
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

impl Transcript {
    /// The words starting in [`start_ms`, `end_ms`), joined with spaces.
    pub fn span(&self, start_ms: u64, end_ms: u64) -> String {
        self.words
            .iter()
            .filter(|w| w.start_ms >= start_ms && w.start_ms < end_ms)
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The transcript cut into passages, each with the times of its first and last word.
    pub fn chunks(&self, count_tokens: &dyn Fn(&str) -> usize, max_tokens: usize) -> Vec<Chunk> {
        let mut chunks = Vec::new();
        let mut current: Vec<&Word> = Vec::new();
        let mut tokens = 0usize;
        let flush = |current: &mut Vec<&Word>, chunks: &mut Vec<Chunk>| {
            if let (Some(first), Some(last)) = (current.first(), current.last()) {
                chunks.push(Chunk {
                    text: current
                        .iter()
                        .map(|w| w.text.as_str())
                        .collect::<Vec<_>>()
                        .join(" "),
                    page: None,
                    start_ms: Some(first.start_ms),
                    end_ms: Some(last.end_ms.max(first.start_ms)),
                });
            }
            current.clear();
        };
        for word in &self.words {
            // Leading space: words are joined with one.
            let word_tokens = count_tokens(&format!(" {}", word.text)).max(1);
            if let Some(first) = current.first()
                && (tokens + word_tokens > max_tokens
                    || word.end_ms.saturating_sub(first.start_ms) > HARD_PASSAGE_MS)
            {
                flush(&mut current, &mut chunks);
                tokens = 0;
            }
            current.push(word);
            tokens += word_tokens;
            let first = current[0];
            if word.end_ms.saturating_sub(first.start_ms) >= SOFT_PASSAGE_MS
                && ends_sentence(&word.text)
            {
                flush(&mut current, &mut chunks);
                tokens = 0;
            }
        }
        flush(&mut current, &mut chunks);
        chunks
    }
}

fn ends_sentence(word: &str) -> bool {
    word.trim_end_matches(['"', '\'', ')', '”', '’'])
        .ends_with(['.', '!', '?', '。', '！', '？'])
}

/// `fluidaudiocli` on PATH or in `~/.local/bin`.
pub fn find_fluidaudiocli() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/bin"));
    }
    dirs.into_iter()
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| d.join(FLUIDAUDIOCLI))
        .find(|p| is_executable(p))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Transcribes 16 kHz mono `samples` with `cli`, using `work_dir` for the temporary WAV
/// and JSON (removed afterwards). Blocking; the first run downloads the speech model.
pub fn transcribe(cli: &Path, samples: &[f32], work_dir: &Path) -> Result<Transcript, EmbedError> {
    if samples.is_empty() {
        return Ok(Transcript::default());
    }
    std::fs::create_dir_all(work_dir)?;
    let wav = work_dir.join("transcribe.wav");
    let json = work_dir.join("transcribe.json");
    let _ = std::fs::remove_file(&json);
    std::fs::write(&wav, wav_bytes(samples))?;
    let result = Command::new(cli)
        .arg("transcribe")
        .arg(&wav)
        .arg("--word-timestamps")
        .arg("--output-json")
        .arg(&json)
        .current_dir(work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    let _ = std::fs::remove_file(&wav);
    let out =
        result.map_err(|e| EmbedError::Transcribe(format!("couldn't run {FLUIDAUDIOCLI}: {e}")))?;
    let written = std::fs::read(&json);
    let _ = std::fs::remove_file(&json);
    match written {
        Ok(bytes) if out.status.success() => parse(&bytes),
        _ => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let tail: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
            let tail = tail[tail.len().saturating_sub(3)..].join(" / ");
            Err(EmbedError::Transcribe(format!(
                "{FLUIDAUDIOCLI} exited with {} without a transcript{}",
                out.status,
                if tail.is_empty() {
                    String::new()
                } else {
                    format!(": {tail}")
                }
            )))
        }
    }
}

/// `TranscriptionJSONOutput`'s fields Margin reads.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Output {
    #[serde(default)]
    word_timings: Vec<WordTiming>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WordTiming {
    word: String,
    start_time: f64,
    end_time: f64,
}

/// Parses `--output-json`'s file.
pub(crate) fn parse(bytes: &[u8]) -> Result<Transcript, EmbedError> {
    let output: Output = serde_json::from_slice(bytes)
        .map_err(|e| EmbedError::Transcribe(format!("unexpected transcript: {e}")))?;
    let ms = |s: f64| (s.max(0.0) * 1000.0).round() as u64;
    let words = output
        .word_timings
        .into_iter()
        .filter(|w| !w.word.trim().is_empty())
        .map(|w| Word {
            text: w.word.trim().to_string(),
            start_ms: ms(w.start_time),
            end_ms: ms(w.end_time).max(ms(w.start_time)),
        })
        .collect();
    Ok(Transcript { words })
}

/// A 16-bit PCM WAV of 16 kHz mono `samples`.
pub(crate) fn wav_bytes(samples: &[f32]) -> Vec<u8> {
    let data = samples.len() * 2;
    let mut out = Vec::with_capacity(44 + data);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + data) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data as u32).to_le_bytes());
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(text: &str, start: u64, end: u64) -> Word {
        Word {
            text: text.into(),
            start_ms: start,
            end_ms: end,
        }
    }

    #[test]
    fn parses_fluidaudio_json() {
        let json = r#"{
          "audioFile" : "speech.wav", "confidence" : 0.97, "durationSeconds" : 6.36,
          "mode" : "batch", "modelVersion" : "v3", "text" : "Photosynthesis turns",
          "wordTimings" : [
            {"confidence": 0.98, "endTime": 0.9600000000000001, "startTime": 0, "word": "Photosynthesis"},
            {"confidence": 1, "endTime": 1.28, "startTime": 0.96, "word": "turns"},
            {"confidence": 1, "endTime": 1.5, "startTime": 1.4, "word": " "}
          ]
        }"#;
        let t = parse(json.as_bytes()).unwrap();
        assert_eq!(
            t.words,
            vec![word("Photosynthesis", 0, 960), word("turns", 960, 1280)]
        );
        assert!(parse(b"not json").is_err());
        assert_eq!(parse(b"{}").unwrap(), Transcript::default());
    }

    #[test]
    fn spans_take_the_words_starting_inside() {
        let t = Transcript {
            words: vec![
                word("a", 0, 900),
                word("b", 9_500, 10_400),
                word("c", 10_000, 11_000),
            ],
        };
        assert_eq!(t.span(0, 10_000), "a b");
        assert_eq!(t.span(10_000, 20_000), "c");
        assert_eq!(t.span(20_000, 30_000), "");
    }

    #[test]
    fn chunks_end_at_sentences_and_time_limits() {
        // One word a second: "w0 … w24." then more words without a full stop.
        let mut words: Vec<Word> = (0..25)
            .map(|i| word(&format!("w{i}"), i * 1000, i * 1000 + 800))
            .collect();
        words[3].text = "early.".into();
        words[22].text = "end.".into();
        words.extend((25..90).map(|i| word(&format!("x{i}"), i * 1000, i * 1000 + 800)));
        let t = Transcript { words };
        let chunks = t.chunks(&|s| s.split_whitespace().count(), 1000);
        // The first sentence end after 20 s closes the first passage.
        assert_eq!(chunks[0].start_ms, Some(0));
        assert_eq!(chunks[0].end_ms, Some(22_800));
        assert!(chunks[0].text.starts_with("w0 w1 w2 early. w4"));
        assert!(chunks[0].text.ends_with("w21 end."));
        // Without sentence ends, passages stop at 30 s.
        assert_eq!(chunks[1].start_ms, Some(23_000));
        assert!(chunks[1].end_ms.unwrap() - chunks[1].start_ms.unwrap() <= HARD_PASSAGE_MS);
        assert!(chunks.iter().all(|c| c.page.is_none()));
        let all: Vec<&str> = chunks.iter().flat_map(|c| c.text.split(' ')).collect();
        assert_eq!(all.len(), 90);

        // The token limit cuts too.
        let short = t.chunks(&|s| s.split_whitespace().count(), 10);
        assert!(short.iter().all(|c| c.text.split(' ').count() <= 10));
        assert!(Transcript::default().chunks(&|_| 1, 10).is_empty());
    }

    #[test]
    fn wav_header_is_16k_mono_pcm() {
        let wav = wav_bytes(&[0.0, 1.0, -1.0, 0.5]);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(wav.len(), 44 + 8);
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
        assert_eq!(i16::from_le_bytes([wav[46], wav[47]]), 32767);
        assert_eq!(i16::from_le_bytes([wav[48], wav[49]]), -32767);
    }
}
