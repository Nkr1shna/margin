//! What was said in a recording, word by word with each word's start and end time (made
//! by Parakeet, `transcript_parakeet.rs`).
//!
//! Transcripts are cut into passages of at most 30 seconds (ending at a sentence once
//! one is 20 seconds long) and at most the chunk token limit, so a citation points at a
//! short stretch of the recording.

use margin_shared::TranscriptSegment;
use serde::{Deserialize, Serialize};

use crate::Chunk;

/// A passage ends at the first sentence end after this long…
const SOFT_PASSAGE_MS: u64 = 20_000;
/// …and at a word boundary after this long, whatever the words.
const HARD_PASSAGE_MS: u64 = 30_000;
/// A line of the transcript the app shows ends at a sentence end, at a pause this long…
const LINE_PAUSE_MS: u64 = 1_500;
/// …or at a word boundary after this long.
const MAX_LINE_MS: u64 = 12_000;

/// What was said, word by word. Kept with the source (as JSON) so the app can show the
/// whole transcript.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    pub words: Vec<Word>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

    /// The transcript as lines to read along with the recording: each ends at a sentence
    /// end, before a pause of `LINE_PAUSE_MS`, or at a word boundary once it is
    /// `MAX_LINE_MS` long.
    pub fn lines(&self) -> Vec<TranscriptSegment> {
        let mut lines = Vec::new();
        let mut current: Vec<&Word> = Vec::new();
        let flush = |current: &mut Vec<&Word>, lines: &mut Vec<TranscriptSegment>| {
            if let (Some(first), Some(last)) = (current.first(), current.last()) {
                lines.push(TranscriptSegment {
                    start_ms: first.start_ms,
                    end_ms: last.end_ms.max(first.start_ms),
                    text: current
                        .iter()
                        .map(|w| w.text.as_str())
                        .collect::<Vec<_>>()
                        .join(" "),
                });
            }
            current.clear();
        };
        for word in &self.words {
            if let (Some(first), Some(last)) = (current.first(), current.last())
                && (word.start_ms.saturating_sub(last.end_ms) >= LINE_PAUSE_MS
                    || word.end_ms.saturating_sub(first.start_ms) > MAX_LINE_MS)
            {
                flush(&mut current, &mut lines);
            }
            current.push(word);
            if ends_sentence(&word.text) {
                flush(&mut current, &mut lines);
            }
        }
        flush(&mut current, &mut lines);
        lines
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
                    ..Default::default()
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
    fn lines_end_at_sentences_pauses_and_length() {
        let mut words = vec![
            word("Hello", 0, 400),
            word("there.", 450, 900),
            word("After", 1_000, 1_300),
            word("this", 1_350, 1_600),
            // A long pause starts a new line.
            word("pause", 4_000, 4_400),
            word("goes", 4_500, 4_800),
            word("on", 4_900, 5_100),
        ];
        // Then one word a second without a full stop: cut once longer than 12 s.
        words.extend((0..20).map(|i| word(&format!("w{i}"), 7_000 + i * 1000, 7_800 + i * 1000)));
        let t = Transcript { words };
        let lines = t.lines();
        let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            &texts[..3],
            &["Hello there.", "After this", "pause goes on"]
        );
        assert_eq!((lines[0].start_ms, lines[0].end_ms), (0, 900));
        assert_eq!((lines[2].start_ms, lines[2].end_ms), (4_000, 5_100));
        assert!(lines[3..].iter().all(|l| l.end_ms - l.start_ms <= 12_000));
        let words: usize = lines.iter().map(|l| l.text.split(' ').count()).sum();
        assert_eq!(words, 27);
        assert!(Transcript::default().lines().is_empty());
        // Kept on disk as JSON.
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<Transcript>(&json).unwrap(), t);
    }
}
