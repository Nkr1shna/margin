//! Cutting extracted text into chunks that fit the model.
//!
//! Each page is split into units: sentences (and lines) of prose, lines of code. A unit
//! that alone is over the limit is halved at whitespace until it fits. Units are packed
//! greedily; when a chunk is full it ends at the last paragraph break if that keeps it at
//! least half full, so prose and code tend to break between paragraphs and blocks. The
//! next chunk starts with the previous one's last units, up to a tenth of the limit.
//! Unit counts are summed while packing, and every finished chunk is counted once more
//! and shortened if the whole comes to more than the parts.

use crate::{Chunk, Extracted, SourceKind};

/// What joins a unit to the one before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sep {
    /// A blank line: a new paragraph or block.
    Para,
    Line,
    Space,
    /// Pieces of one word cut apart.
    None,
}

impl Sep {
    fn as_str(self) -> &'static str {
        match self {
            Sep::Para => "\n\n",
            Sep::Line => "\n",
            Sep::Space => " ",
            Sep::None => "",
        }
    }
}

#[derive(Debug)]
struct Unit {
    text: String,
    tokens: usize,
    sep: Sep,
}

pub(crate) fn chunk(
    doc: &Extracted,
    kind: SourceKind,
    count_tokens: &dyn Fn(&str) -> usize,
    max_tokens: usize,
) -> Vec<Chunk> {
    let max_tokens = max_tokens.max(1);
    let mut chunks = Vec::new();
    for page in &doc.pages {
        let units = units(&page.text, kind, count_tokens, max_tokens);
        for text in pack(&units, count_tokens, max_tokens) {
            chunks.push(Chunk {
                text,
                page: page.number,
            });
        }
    }
    chunks
}

/// Paragraphs (runs of lines between blank lines), each split into units.
fn units(
    text: &str,
    kind: SourceKind,
    count_tokens: &dyn Fn(&str) -> usize,
    max_tokens: usize,
) -> Vec<Unit> {
    let mut pieces: Vec<(&str, Sep)> = Vec::new();
    let mut sep = Sep::Para;
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            sep = Sep::Para;
            continue;
        }
        match kind {
            // Indentation matters in code; each line is a unit.
            SourceKind::Code => pieces.push((line, sep)),
            // Transcripts of audio and video are prose too.
            SourceKind::Text
            | SourceKind::Pdf
            | SourceKind::Image
            | SourceKind::Audio
            | SourceKind::Video => {
                for (i, sentence) in sentences(line.trim_start()).into_iter().enumerate() {
                    pieces.push((sentence, if i == 0 { sep } else { Sep::Space }));
                }
            }
        }
        sep = Sep::Line;
    }

    let mut units = Vec::new();
    for (text, sep) in pieces {
        for (i, mut unit) in fit(text, count_tokens, max_tokens).into_iter().enumerate() {
            if i == 0 {
                unit.sep = sep;
            }
            units.push(unit);
        }
    }
    units
}

/// Splits a line of prose after `.`, `!` or `?` (and closing quotes or brackets) followed
/// by a space.
fn sentences(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = line.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if !matches!(c, '.' | '!' | '?' | '。' | '！' | '？') {
            continue;
        }
        while let Some(&(_, next)) = chars.peek() {
            if matches!(next, '"' | '\'' | ')' | ']' | '”' | '’' | '.' | '!' | '?') {
                chars.next();
            } else {
                break;
            }
        }
        if let Some(&(i, next)) = chars.peek()
            && next.is_whitespace()
        {
            let sentence = line[start..i].trim();
            if !sentence.is_empty() {
                out.push(sentence);
            }
            start = i;
        }
    }
    let rest = line[start..].trim();
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

/// `text` as pieces of at most `max_tokens`: halved at the whitespace nearest the middle
/// (or the middle character when there is none) until each fits.
fn fit(text: &str, count_tokens: &dyn Fn(&str) -> usize, max_tokens: usize) -> Vec<Unit> {
    let tokens = count_tokens(text);
    if tokens <= max_tokens || text.chars().nth(1).is_none() {
        return vec![Unit {
            text: text.to_string(),
            tokens,
            sep: Sep::Space,
        }];
    }
    let middle = text.len() / 2;
    let space = text
        .char_indices()
        .filter(|(_, c)| c.is_whitespace())
        .map(|(i, _)| i)
        .min_by_key(|&i| i.abs_diff(middle));
    let (left, right, sep) = match space {
        Some(i) if !text[..i].trim().is_empty() && !text[i..].trim().is_empty() => {
            (text[..i].trim_end(), text[i..].trim_start(), Sep::Space)
        }
        _ => {
            let at = text
                .char_indices()
                .map(|(i, _)| i)
                .filter(|&i| i > 0)
                .min_by_key(|&i| i.abs_diff(middle))
                .unwrap_or(middle);
            (&text[..at], &text[at..], Sep::None)
        }
    };
    let mut out = fit(left, count_tokens, max_tokens);
    let mut tail = fit(right, count_tokens, max_tokens);
    tail[0].sep = sep;
    out.append(&mut tail);
    out
}

fn join(units: &[Unit]) -> String {
    let mut text = String::new();
    for (i, unit) in units.iter().enumerate() {
        if i > 0 {
            text.push_str(unit.sep.as_str());
        }
        text.push_str(&unit.text);
    }
    text
}

fn pack(units: &[Unit], count_tokens: &dyn Fn(&str) -> usize, max_tokens: usize) -> Vec<String> {
    let overlap_budget = max_tokens / 10;
    let mut out = Vec::new();
    // The chunk being built is units[lo..end]; units before `next` were in an earlier
    // chunk (the overlap).
    let mut lo = 0;
    let mut next = 0;
    while next < units.len() {
        let mut tokens: usize = units[lo..next].iter().map(|u| u.tokens).sum();
        let mut end = next;
        while end < units.len() && tokens + units[end].tokens <= max_tokens {
            tokens += units[end].tokens;
            end += 1;
        }
        if end == next {
            if lo < next {
                // The overlap leaves no room for the next unit; shorten it.
                lo += 1;
                continue;
            }
            end = next + 1;
        }
        // End at a paragraph break when one keeps the chunk at least half full.
        if end < units.len() && units[end].sep != Sep::Para {
            let mut filled = 0;
            let mut best = None;
            for (i, unit) in units.iter().enumerate().take(end).skip(lo) {
                if i > next && unit.sep == Sep::Para && filled * 2 >= max_tokens {
                    best = Some(i);
                }
                filled += unit.tokens;
            }
            if let Some(i) = best {
                end = i;
            }
        }
        let mut text = join(&units[lo..end]);
        while count_tokens(&text) > max_tokens {
            if end > next + 1 {
                end -= 1;
            } else if lo < next {
                lo += 1;
            } else {
                break;
            }
            text = join(&units[lo..end]);
        }
        out.push(text);
        if end >= units.len() {
            break;
        }

        let mut start = end;
        let mut carried = 0;
        while start > lo + 1 && carried + units[start - 1].tokens <= overlap_budget {
            carried += units[start - 1].tokens;
            start -= 1;
        }
        lo = start;
        next = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::fake::FakeEmbedder;
    use crate::{Embedder, Page};

    use super::*;

    fn count(text: &str) -> usize {
        FakeEmbedder.count_tokens(text)
    }

    fn doc(pages: &[(Option<u32>, &str)]) -> Extracted {
        Extracted {
            pages: pages
                .iter()
                .map(|(number, text)| Page {
                    number: *number,
                    text: text.to_string(),
                })
                .collect(),
        }
    }

    /// `n` numbered sentences of five words each: "w1a w1b w1c w1d w1e."
    fn prose(prefix: &str, n: usize) -> String {
        (0..n)
            .map(|i| format!("{prefix}{i}a {prefix}{i}b {prefix}{i}c {prefix}{i}d {prefix}{i}e."))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn words(text: &str) -> Vec<String> {
        text.split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn short_text_is_one_chunk() {
        let chunks = chunk(
            &doc(&[(None, "Hello there.\n\nSecond paragraph.")]),
            SourceKind::Text,
            &count,
            100,
        );
        assert_eq!(
            chunks,
            vec![Chunk {
                text: "Hello there.\n\nSecond paragraph.".into(),
                page: None
            }]
        );
    }

    #[test]
    fn empty_pages_give_no_chunks() {
        let chunks = chunk(
            &doc(&[(Some(1), "  \n\n "), (Some(2), "")]),
            SourceKind::Pdf,
            &count,
            50,
        );
        assert!(chunks.is_empty());
    }

    #[test]
    fn chunks_never_exceed_the_limit_and_cover_everything() {
        let text = format!(
            "{}\n\n{}\n\n{}",
            prose("a", 40),
            prose("b", 7),
            prose("c", 25)
        );
        for max in [7, 12, 20, 50, 64, 100] {
            let chunks = chunk(&doc(&[(None, &text)]), SourceKind::Text, &count, max);
            assert!(chunks.len() > 1, "max {max}");
            for c in &chunks {
                assert!(
                    count(&c.text) <= max,
                    "max {max}: {} tokens",
                    count(&c.text)
                );
            }
            // Every word appears, and in order once overlaps are removed.
            let all: Vec<String> = chunks.iter().flat_map(|c| words(&c.text)).collect();
            for w in words(&text) {
                assert!(all.contains(&w), "max {max}: {w} missing");
            }
        }
    }

    #[test]
    fn adjacent_chunks_overlap_by_about_a_tenth() {
        let text = prose("s", 60); // one paragraph, 300 words
        let chunks = chunk(&doc(&[(None, &text)]), SourceKind::Text, &count, 100);
        assert!(chunks.len() >= 3);
        for pair in chunks.windows(2) {
            let first = words(&pair[0].text);
            let second = words(&pair[1].text);
            // The second starts with the first's last sentence or two.
            let shared = (1..=second.len())
                .rev()
                .find(|&n| first.ends_with(&second[..n]))
                .unwrap_or(0);
            assert!((5..=10).contains(&shared), "overlap {shared}");
        }
    }

    #[test]
    fn prose_breaks_at_paragraphs_then_sentences() {
        let text = format!("{}\n\n{}", prose("a", 8), prose("b", 8)); // 40 + 40 words
        let chunks = chunk(&doc(&[(None, &text)]), SourceKind::Text, &count, 60);
        // The first paragraph fits whole, so the first chunk is exactly it.
        assert_eq!(chunks[0].text, prose("a", 8));
        // Sentences are never cut.
        for c in &chunks {
            assert!(c.text.ends_with('.'), "{}", c.text);
        }
    }

    #[test]
    fn a_long_sentence_is_split_by_words() {
        let sentence = (0..250)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let chunks = chunk(&doc(&[(None, &sentence)]), SourceKind::Text, &count, 32);
        assert!(chunks.len() >= 8);
        assert!(chunks.iter().all(|c| count(&c.text) <= 32));
        let all: Vec<String> = chunks.iter().flat_map(|c| words(&c.text)).collect();
        assert!((0..250).all(|i| all.contains(&format!("w{i}"))));
    }

    #[test]
    fn code_is_cut_along_lines_and_keeps_indentation() {
        let code: String = (0..60)
            .map(|i| format!("fn f{i}() {{\n    let x{i} = {i};\n}}\n\n"))
            .collect();
        let chunks = chunk(&doc(&[(None, &code)]), SourceKind::Code, &count, 40);
        assert!(chunks.len() > 1);
        assert!(
            chunks[0]
                .text
                .starts_with("fn f0() {\n    let x0 = 0;\n}\n\nfn f1")
        );
        // Chunks end between blocks, not inside one.
        assert!(chunks.iter().all(|c| c.text.ends_with('}')));
        for c in &chunks {
            assert!(count(&c.text) <= 40);
            assert!(c.text.contains("\n    let x"), "{}", c.text);
            assert!(c.text.lines().all(|l| l.starts_with("fn ")
                || l.starts_with("    let")
                || l == "}"
                || l.is_empty()));
        }
    }

    #[test]
    fn pdf_pages_keep_their_numbers() {
        let one = prose("p", 30);
        let pages = doc(&[
            (Some(1), "Short first page."),
            (Some(2), ""),
            (Some(3), &one),
        ]);
        let chunks = chunk(&pages, SourceKind::Pdf, &count, 50);
        assert_eq!(
            chunks[0],
            Chunk {
                text: "Short first page.".into(),
                page: Some(1)
            }
        );
        assert!(chunks.len() > 2);
        assert!(chunks[1..].iter().all(|c| c.page == Some(3)));
        // No chunk mixes pages.
        assert!(!chunks[1..].iter().any(|c| c.text.contains("first page")));
    }

    #[test]
    fn a_word_longer_than_the_limit_is_cut() {
        // Counts characters: one "word" over the limit must still be split.
        let by_chars = |t: &str| t.chars().filter(|c| !c.is_whitespace()).count();
        let blob = "x".repeat(95);
        let chunks = chunk(&doc(&[(None, &blob)]), SourceKind::Text, &by_chars, 10);
        assert!(chunks.iter().all(|c| by_chars(&c.text) <= 10));
        let total: usize = chunks.iter().map(|c| by_chars(&c.text)).sum();
        assert!(total >= 95);
    }

    #[test]
    fn sentences_split_after_terminal_punctuation() {
        assert_eq!(
            sentences("One. Two! \"Three?\" Four e.g.five 3.14 done"),
            vec!["One.", "Two!", "\"Three?\"", "Four e.g.five 3.14 done"]
        );
    }

    #[test]
    fn whole_chunks_are_recounted() {
        // A counter where joining costs extra: every "\n\n" is 5 tokens.
        let costly = |t: &str| count(t) + 5 * t.matches("\n\n").count();
        let text = (0..20)
            .map(|i| format!("para{i} has four words"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let chunks = chunk(&doc(&[(None, &text)]), SourceKind::Text, &costly, 30);
        assert!(chunks.iter().all(|c| costly(&c.text) <= 30));
        let all: Vec<String> = chunks.iter().flat_map(|c| words(&c.text)).collect();
        assert!((0..20).all(|i| all.contains(&format!("para{i}"))));
    }
}
