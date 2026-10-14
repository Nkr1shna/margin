//! Cutting extracted text into chunks that fit the model. Two chunkers: `"pages-v1"`
//! (`chunk`, below) and `"sections-v2"` (`chunk_sections`, further down: headings, section
//! paths, line ranges, one-sentence overlap, atomic tables and code blocks).
//!
//! `"pages-v1"`: each page is split into units: sentences (and lines) of prose, lines of code. A unit
//! that alone is over the limit is halved at whitespace until it fits. Units are packed
//! greedily; when a chunk is full it ends at the last paragraph break if that keeps it at
//! least half full, so prose and code tend to break between paragraphs and blocks. The
//! next chunk starts with the previous one's last units, up to a tenth of the limit.
//! Unit counts are summed while packing, and every finished chunk is counted once more
//! and shortened if the whole comes to more than the parts.

use crate::{Chunk, Extracted, SectionsCfg, SourceKind, TextFormat};

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
                start_ms: None,
                end_ms: None,
                ..Default::default()
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

// ---------------------------------------------------------------------------
// "sections-v2": headings, section paths, line ranges, sentence overlap, atomic blocks
// ---------------------------------------------------------------------------

/// The format plain-looking text is written in, from its contents: LaTeX commands,
/// Markdown `#` headings or fences, reST underlined titles; otherwise `Plain`.
pub(crate) fn sniff_format(text: &str) -> TextFormat {
    let lines: Vec<&str> = text.lines().take(2000).collect();
    if lines.iter().any(|l| {
        let t = l.trim_start();
        t.starts_with("\\documentclass") || latex_heading(t).is_some()
    }) {
        return TextFormat::Latex;
    }
    if lines
        .iter()
        .any(|l| atx_heading(l).is_some() || fence_marker(l.trim_start()).is_some())
    {
        return TextFormat::Markdown;
    }
    if (0..lines.len().saturating_sub(1)).any(|i| rst_underlined(&lines, i).is_some()) {
        return TextFormat::Rst;
    }
    TextFormat::Plain
}

#[derive(Debug, Clone, PartialEq)]
enum BlockKind {
    /// A heading at `level` (1 = top) titled `title`.
    Heading { level: usize, title: String },
    /// A paragraph of prose, cut into sentences.
    Para,
    /// A table, code fence, directive or display math: kept whole when it fits.
    Atomic,
    /// Lines of code.
    Code,
}

#[derive(Debug)]
struct Block<'a> {
    kind: BlockKind,
    /// (1-based line number, the line).
    lines: Vec<(u32, &'a str)>,
}

#[derive(Debug, Clone)]
struct SUnit {
    text: String,
    tokens: usize,
    sep: Sep,
    start: u32,
    end: u32,
    /// May start the next chunk as its overlap (a sentence, or a line of code).
    overlap: bool,
}

pub(crate) fn chunk_sections(
    doc: &Extracted,
    kind: SourceKind,
    count_tokens: &dyn Fn(&str) -> usize,
    cfg: &SectionsCfg,
) -> Vec<Chunk> {
    let max_tokens = cfg.max_tokens.max(1);
    let target = cfg.target_tokens.clamp(1, max_tokens);
    let format = match kind {
        SourceKind::Text => doc.format,
        _ => TextFormat::Plain,
    };
    let has_lines = matches!(kind, SourceKind::Text | SourceKind::Code);
    let mut chunks = Vec::new();
    for page in &doc.pages {
        let lines: Vec<(u32, &str)> = page
            .text
            .lines()
            .enumerate()
            .map(|(i, l)| (i as u32 + 1, l))
            .collect();
        let blocks = match (kind, format) {
            (SourceKind::Code, _) => paragraphs(&lines, BlockKind::Code),
            (_, TextFormat::Markdown | TextFormat::Html) => markdown_blocks(&lines),
            (_, TextFormat::Rst) => rst_blocks(&lines),
            (_, TextFormat::Latex) => latex_blocks(&lines),
            (_, TextFormat::Plain) => paragraphs(&lines, BlockKind::Para),
        };
        let mut emit = |path: &str, units: &[SUnit]| {
            for (lo, end, text) in pack_section(units, count_tokens, max_tokens, target) {
                let (start_line, end_line) = if has_lines {
                    (
                        Some(units[lo].start),
                        units[lo..end].iter().map(|u| u.end).max(),
                    )
                } else {
                    (None, None)
                };
                chunks.push(Chunk {
                    hash: crate::chunk_hash(path, &text),
                    text,
                    page: page.number,
                    start_ms: None,
                    end_ms: None,
                    section_path: path.to_string(),
                    start_line,
                    end_line,
                });
            }
        };

        let mut stack: Vec<(usize, String)> = Vec::new();
        let mut path = String::new();
        let mut units: Vec<SUnit> = Vec::new();
        let mut has_content = false;
        for block in &blocks {
            match &block.kind {
                BlockKind::Heading { level, title } => {
                    // A section with only headings so far carries them into the next.
                    if has_content {
                        emit(&path, &units);
                        units.clear();
                        has_content = false;
                    }
                    while stack.last().is_some_and(|(l, _)| l >= level) {
                        stack.pop();
                    }
                    stack.push((*level, title.clone()));
                    path = stack
                        .iter()
                        .map(|(_, t)| t.as_str())
                        .collect::<Vec<_>>()
                        .join(" > ");
                    let (n, _) = block.lines[0];
                    let last = block.lines.last().map_or(n, |(n, _)| *n);
                    let text = heading_text(format, block);
                    units.extend(
                        fit(&text, count_tokens, max_tokens)
                            .into_iter()
                            .enumerate()
                            .map(|(i, u)| SUnit {
                                sep: if i == 0 { Sep::Para } else { u.sep },
                                text: u.text,
                                tokens: u.tokens,
                                start: n,
                                end: last,
                                overlap: false,
                            }),
                    );
                }
                _ => {
                    let new = block_units(block, count_tokens, max_tokens);
                    has_content |= !new.is_empty();
                    units.extend(new);
                }
            }
        }
        if !units.is_empty() {
            emit(&path, &units);
        }
    }
    chunks
}

/// What a heading contributes to its chunk's text: its line as written (Markdown,
/// LaTeX), or just the title (reST, whose adornment lines are noise).
fn heading_text(format: TextFormat, block: &Block) -> String {
    match (&block.kind, format) {
        (BlockKind::Heading { title, .. }, TextFormat::Rst) => title.clone(),
        _ => block
            .lines
            .iter()
            .map(|(_, l)| l.trim())
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .to_string(),
    }
}

fn block_units(
    block: &Block,
    count_tokens: &dyn Fn(&str) -> usize,
    max_tokens: usize,
) -> Vec<SUnit> {
    let mut out = Vec::new();
    let push = |text: &str, sep: Sep, n: u32, last: u32, overlap: bool, out: &mut Vec<SUnit>| {
        for (i, u) in fit(text, count_tokens, max_tokens).into_iter().enumerate() {
            out.push(SUnit {
                sep: if i == 0 { sep } else { u.sep },
                text: u.text,
                tokens: u.tokens,
                start: n,
                end: last,
                overlap,
            });
        }
    };
    match block.kind {
        BlockKind::Heading { .. } => {}
        BlockKind::Para => {
            let mut first = true;
            for &(n, line) in &block.lines {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                for (i, sentence) in sentences(line).into_iter().enumerate() {
                    let sep = match (first, i) {
                        (true, _) => Sep::Para,
                        (false, 0) => Sep::Line,
                        _ => Sep::Space,
                    };
                    first = false;
                    push(sentence, sep, n, n, true, &mut out);
                }
            }
        }
        BlockKind::Code => {
            for (i, &(n, line)) in block.lines.iter().enumerate() {
                let line = line.trim_end();
                if line.trim().is_empty() {
                    continue;
                }
                push(
                    line,
                    if i == 0 { Sep::Para } else { Sep::Line },
                    n,
                    n,
                    true,
                    &mut out,
                );
            }
        }
        BlockKind::Atomic => {
            let first = block.lines.first().map_or(1, |(n, _)| *n);
            let last = block.lines.last().map_or(first, |(n, _)| *n);
            let text = block
                .lines
                .iter()
                .map(|(_, l)| l.trim_end())
                .collect::<Vec<_>>()
                .join("\n");
            let text = text.trim_matches('\n');
            if text.trim().is_empty() {
                return out;
            }
            let tokens = count_tokens(text);
            if tokens <= max_tokens {
                out.push(SUnit {
                    text: text.to_string(),
                    tokens,
                    sep: Sep::Para,
                    start: first,
                    end: last,
                    overlap: false,
                });
            } else {
                // Too big to keep whole: line by line.
                for (i, &(n, line)) in block.lines.iter().enumerate() {
                    let line = line.trim_end();
                    if line.trim().is_empty() {
                        continue;
                    }
                    push(
                        line,
                        if i == 0 { Sep::Para } else { Sep::Line },
                        n,
                        n,
                        false,
                        &mut out,
                    );
                }
            }
        }
    }
    out
}

/// Packs one section's units into chunks of about `target` tokens (never more than
/// `max_tokens`), returning `(first unit, end, text)`; units before a chunk's first new
/// unit are its overlap: the previous chunk's last sentence.
fn pack_section(
    units: &[SUnit],
    count_tokens: &dyn Fn(&str) -> usize,
    max_tokens: usize,
    target: usize,
) -> Vec<(usize, usize, String)> {
    let join = |units: &[SUnit]| {
        let mut text = String::new();
        for (i, unit) in units.iter().enumerate() {
            if i > 0 {
                text.push_str(unit.sep.as_str());
            }
            text.push_str(&unit.text);
        }
        text
    };
    let n = units.len();
    let mut out = Vec::new();
    let (mut lo, mut next) = (0, 0);
    while next < n {
        let carried: usize = units[lo..next].iter().map(|u| u.tokens).sum();
        let rest: usize = units[next..].iter().map(|u| u.tokens).sum();
        let mut end;
        if carried + rest <= max_tokens {
            // The rest fits: no short tail chunk.
            end = n;
        } else {
            let mut tokens = carried;
            end = next;
            while end < n && tokens + units[end].tokens <= target {
                tokens += units[end].tokens;
                end += 1;
            }
            if end == next {
                if lo < next {
                    // The overlap leaves no room; drop it.
                    lo = next;
                    continue;
                }
                // One unit over the target (`fit` keeps it within `max_tokens`).
                end = next + 1;
            }
            // End at a paragraph break when one keeps the chunk at least half full.
            if end < n && units[end].sep != Sep::Para {
                let mut filled = 0;
                let mut best = None;
                for (i, unit) in units.iter().enumerate().take(end).skip(lo) {
                    if i > next && unit.sep == Sep::Para && filled * 2 >= target {
                        best = Some(i);
                    }
                    filled += unit.tokens;
                }
                if let Some(i) = best {
                    end = i;
                }
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
        out.push((lo, end, text));
        if end >= n {
            break;
        }
        // One sentence of overlap, when the last one is a sentence and short.
        let last = &units[end - 1];
        lo = if last.overlap && last.tokens * 4 <= target {
            end - 1
        } else {
            end
        };
        next = end;
    }
    out
}

/// Blank-line separated runs of lines, each a block of `kind`.
fn paragraphs<'a>(lines: &[(u32, &'a str)], kind: BlockKind) -> Vec<Block<'a>> {
    let mut out = Vec::new();
    let mut current: Vec<(u32, &str)> = Vec::new();
    for &(n, line) in lines {
        if line.trim().is_empty() {
            if !current.is_empty() {
                out.push(Block {
                    kind: kind.clone(),
                    lines: std::mem::take(&mut current),
                });
            }
        } else {
            current.push((n, line));
        }
    }
    if !current.is_empty() {
        out.push(Block {
            kind,
            lines: current,
        });
    }
    out
}

/// `# Title` (up to six `#`, closing `#`s dropped): (level, title).
fn atx_heading(line: &str) -> Option<(usize, String)> {
    let t = line.trim_start();
    if line.len() - t.len() > 3 {
        return None;
    }
    let level = t.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &t[level..];
    if !rest.starts_with([' ', '\t']) {
        return None;
    }
    let title = rest.trim().trim_end_matches('#').trim();
    if title.is_empty() {
        return None;
    }
    Some((level, title.to_string()))
}

/// The marker of a code fence line (```` ``` ```` or `~~~`, three or more).
fn fence_marker(t: &str) -> Option<&str> {
    for c in ['`', '~'] {
        let n = t.chars().take_while(|&x| x == c).count();
        if n >= 3 {
            return Some(&t[..n]);
        }
    }
    None
}

/// A Markdown setext underline: `===` (level 1) or `---` (level 2).
fn setext_level(line: &str) -> Option<usize> {
    let t = line.trim();
    if t.len() >= 3 && t.chars().all(|c| c == '=') {
        Some(1)
    } else if t.len() >= 3 && t.chars().all(|c| c == '-') {
        Some(2)
    } else {
        None
    }
}

fn is_rule(t: &str) -> bool {
    let t: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    t.len() >= 3
        && (t.chars().all(|c| c == '-')
            || t.chars().all(|c| c == '*')
            || t.chars().all(|c| c == '_'))
}

fn markdown_blocks<'a>(lines: &[(u32, &'a str)]) -> Vec<Block<'a>> {
    let n = lines.len();
    let blank = |i: usize| lines[i].1.trim().is_empty();
    let starts_block = |i: usize| {
        let t = lines[i].1.trim_start();
        atx_heading(lines[i].1).is_some()
            || fence_marker(t).is_some()
            || t.starts_with('|')
            || t.starts_with("$$")
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let (_, line) = lines[i];
        let t = line.trim();
        if t.is_empty() {
            i += 1;
            continue;
        }
        if let Some(marker) = fence_marker(line.trim_start()) {
            let mut j = i + 1;
            while j < n && !lines[j].1.trim_start().starts_with(marker) {
                j += 1;
            }
            let end = j.min(n - 1);
            out.push(Block {
                kind: BlockKind::Atomic,
                lines: lines[i..=end].to_vec(),
            });
            i = end + 1;
            continue;
        }
        if t.starts_with("$$") && !(t.len() > 4 && t.ends_with("$$")) {
            let mut j = i + 1;
            while j < n && !lines[j].1.contains("$$") {
                j += 1;
            }
            let end = j.min(n - 1);
            out.push(Block {
                kind: BlockKind::Atomic,
                lines: lines[i..=end].to_vec(),
            });
            i = end + 1;
            continue;
        }
        if let Some((level, title)) = atx_heading(line) {
            out.push(Block {
                kind: BlockKind::Heading { level, title },
                lines: vec![lines[i]],
            });
            i += 1;
            continue;
        }
        if i + 1 < n
            && let Some(level) = setext_level(lines[i + 1].1)
            && !t.starts_with(['|', '-', '*', '+', '>'])
        {
            out.push(Block {
                kind: BlockKind::Heading {
                    level,
                    title: t.to_string(),
                },
                lines: lines[i..=i + 1].to_vec(),
            });
            i += 2;
            continue;
        }
        if t.starts_with('|') {
            let mut j = i;
            while j < n && lines[j].1.trim_start().starts_with('|') {
                j += 1;
            }
            out.push(Block {
                kind: BlockKind::Atomic,
                lines: lines[i..j].to_vec(),
            });
            i = j;
            continue;
        }
        if is_rule(t) {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < n
            && !blank(j)
            && !starts_block(j)
            && !(j + 1 < n && setext_level(lines[j + 1].1).is_some())
        {
            j += 1;
        }
        out.push(Block {
            kind: BlockKind::Para,
            lines: lines[i..j].to_vec(),
        });
        i = j;
    }
    out
}

/// The character of a reST adornment line (`=====`, `-----`, `~~~~`…).
fn rst_adornment(line: &str) -> Option<char> {
    let t = line.trim_end();
    if t.len() < 2 || t.starts_with(char::is_whitespace) {
        return None;
    }
    let c = t.chars().next()?;
    if !"=-`:'\"~^_*+#<>.".contains(c) || !t.chars().all(|x| x == c) {
        return None;
    }
    Some(c)
}

/// `Title` over an adornment at least as long: (adornment character, title).
fn rst_underlined(lines: &[&str], i: usize) -> Option<(char, String)> {
    let title = lines[i].trim();
    if title.is_empty()
        || lines[i].starts_with(char::is_whitespace)
        || rst_adornment(lines[i]).is_some()
    {
        return None;
    }
    let under = lines.get(i + 1)?;
    let c = rst_adornment(under)?;
    if under.trim_end().chars().count() < title.chars().count() {
        return None;
    }
    Some((c, title.to_string()))
}

/// A reST simple-table border: runs of `=` separated by spaces, at least two columns.
fn rst_simple_border(line: &str) -> bool {
    let t = line.trim_end();
    t.starts_with('=')
        && t.chars().all(|c| c == '=' || c == ' ')
        && t.split_whitespace().count() >= 2
}

fn rst_blocks<'a>(lines: &[(u32, &'a str)]) -> Vec<Block<'a>> {
    let n = lines.len();
    let plain: Vec<&str> = lines.iter().map(|(_, l)| *l).collect();
    let blank = |i: usize| plain[i].trim().is_empty();
    let indented = |i: usize| plain[i].starts_with(char::is_whitespace);
    // Heading styles in order of first appearance: (character, overlined).
    let mut styles: Vec<(char, bool)> = Vec::new();
    let level_of = |style: (char, bool), styles: &mut Vec<(char, bool)>| {
        if let Some(p) = styles.iter().position(|s| *s == style) {
            p + 1
        } else {
            styles.push(style);
            styles.len()
        }
    };
    let mut out = Vec::new();
    let mut literal_next = false;
    let mut i = 0;
    while i < n {
        if blank(i) {
            i += 1;
            continue;
        }
        // An indented block after a paragraph ending in "::" is a literal block.
        if literal_next && indented(i) {
            let mut j = i;
            while j < n && (blank(j) || indented(j)) {
                j += 1;
            }
            let mut end = j;
            while end > i && blank(end - 1) {
                end -= 1;
            }
            out.push(Block {
                kind: BlockKind::Atomic,
                lines: lines[i..end].to_vec(),
            });
            literal_next = false;
            i = j;
            continue;
        }
        literal_next = false;
        // Overlined title.
        if let Some(c) = rst_adornment(plain[i])
            && i + 2 < n
            && !blank(i + 1)
            && rst_adornment(plain[i + 2]) == Some(c)
        {
            let level = level_of((c, true), &mut styles);
            out.push(Block {
                kind: BlockKind::Heading {
                    level,
                    title: plain[i + 1].trim().to_string(),
                },
                lines: lines[i..=i + 2].to_vec(),
            });
            i += 3;
            continue;
        }
        if let Some((c, title)) = rst_underlined(&plain, i) {
            let level = level_of((c, false), &mut styles);
            out.push(Block {
                kind: BlockKind::Heading { level, title },
                lines: lines[i..=i + 1].to_vec(),
            });
            i += 2;
            continue;
        }
        let t = plain[i].trim_start();
        // A directive (`.. math::`, `.. code-block::`, `.. figure::`…) and its body.
        if t.starts_with(".. ") && !indented(i) {
            let mut j = i + 1;
            while j < n && (blank(j) || indented(j)) {
                j += 1;
            }
            let mut end = j;
            while end > i + 1 && blank(end - 1) {
                end -= 1;
            }
            out.push(Block {
                kind: BlockKind::Atomic,
                lines: lines[i..end].to_vec(),
            });
            i = j;
            continue;
        }
        // Grid tables.
        if t.starts_with("+-") || t.starts_with("+=") {
            let mut j = i;
            while j < n && plain[j].trim_start().starts_with(['+', '|']) {
                j += 1;
            }
            out.push(Block {
                kind: BlockKind::Atomic,
                lines: lines[i..j].to_vec(),
            });
            i = j;
            continue;
        }
        // Simple tables: border, rows (a header and its border), border.
        if rst_simple_border(plain[i]) {
            let mut borders = 1;
            let mut j = i + 1;
            let mut end = n - 1;
            while j < n {
                if rst_simple_border(plain[j]) {
                    borders += 1;
                    if borders >= 2 && (j + 1 >= n || blank(j + 1)) {
                        end = j;
                        break;
                    }
                }
                j += 1;
            }
            out.push(Block {
                kind: BlockKind::Atomic,
                lines: lines[i..=end].to_vec(),
            });
            i = end + 1;
            continue;
        }
        // A paragraph: up to a blank line or the next title.
        let mut j = i + 1;
        while j < n && !blank(j) && rst_underlined(&plain, j).is_none() {
            j += 1;
        }
        literal_next = plain[j - 1].trim_end().ends_with("::");
        out.push(Block {
            kind: BlockKind::Para,
            lines: lines[i..j].to_vec(),
        });
        i = j;
    }
    out
}

/// `\section{Title}` and friends (starred, with an optional short title): (level, title).
fn latex_heading(t: &str) -> Option<(usize, String)> {
    const COMMANDS: [&str; 7] = [
        "part",
        "chapter",
        "section",
        "subsection",
        "subsubsection",
        "paragraph",
        "subparagraph",
    ];
    let rest = t.strip_prefix('\\')?;
    let (level, rest) = COMMANDS
        .iter()
        .enumerate()
        .filter_map(|(i, c)| rest.strip_prefix(c).map(|r| (i + 1, r)))
        // `\subsection` also starts with `\sub…`; the longest command wins.
        .min_by_key(|(_, r)| r.len())?;
    let rest = rest.strip_prefix('*').unwrap_or(rest).trim_start();
    let rest = if let Some(r) = rest.strip_prefix('[') {
        &r[r.find(']')? + 1..]
    } else {
        rest
    };
    let rest = rest.trim_start().strip_prefix('{')?;
    let mut depth = 1;
    for (i, c) in rest.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let title = rest[..i].trim();
                    return (!title.is_empty()).then(|| (level, title.to_string()));
                }
            }
            _ => {}
        }
    }
    None
}

fn latex_blocks<'a>(lines: &[(u32, &'a str)]) -> Vec<Block<'a>> {
    // Environments that hold prose rather than one display.
    const PROSE: [&str; 9] = [
        "document",
        "itemize",
        "enumerate",
        "description",
        "abstract",
        "quote",
        "quotation",
        "center",
        "minipage",
    ];
    let n = lines.len();
    let blank = |i: usize| lines[i].1.trim().is_empty();
    let environment = |t: &str| -> Option<String> {
        let name = t.strip_prefix("\\begin{")?;
        let name = &name[..name.find('}')?];
        (!PROSE.contains(&name)).then(|| name.to_string())
    };
    let display = |t: &str| t.starts_with("\\[") || t.starts_with("$$");
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let t = lines[i].1.trim();
        if t.is_empty() || t.starts_with('%') {
            i += 1;
            continue;
        }
        if let Some((level, title)) = latex_heading(t) {
            out.push(Block {
                kind: BlockKind::Heading { level, title },
                lines: vec![lines[i]],
            });
            i += 1;
            continue;
        }
        let close = if let Some(name) = environment(t) {
            Some(format!("\\end{{{name}}}"))
        } else if t.starts_with("\\[") {
            Some("\\]".to_string())
        } else if t.starts_with("$$") {
            Some("$$".to_string())
        } else {
            None
        };
        if let Some(close) = close {
            // The closing mark may be on the opening line (after the opener itself).
            let opener_len = if t.starts_with("$$") || t.starts_with("\\[") {
                2
            } else {
                0
            };
            let mut j = i;
            if !t[opener_len..].contains(close.as_str()) {
                j = i + 1;
                while j < n && !lines[j].1.contains(close.as_str()) {
                    j += 1;
                }
            }
            let end = j.min(n - 1);
            out.push(Block {
                kind: BlockKind::Atomic,
                lines: lines[i..=end].to_vec(),
            });
            i = end + 1;
            continue;
        }
        let mut j = i + 1;
        while j < n && !blank(j) {
            let t = lines[j].1.trim();
            if latex_heading(t).is_some()
                || environment(t).is_some()
                || display(t)
                || t.starts_with('%')
            {
                break;
            }
            j += 1;
        }
        out.push(Block {
            kind: BlockKind::Para,
            lines: lines[i..j].to_vec(),
        });
        i = j;
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
            ..Default::default()
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
                page: None,
                start_ms: None,
                end_ms: None,
                ..Default::default()
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
                page: Some(1),
                start_ms: None,
                end_ms: None,
                ..Default::default()
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

    // -----------------------------------------------------------------------
    // sections-v2
    // -----------------------------------------------------------------------

    fn text_doc(format: TextFormat, text: &str) -> Extracted {
        Extracted {
            format,
            ..doc(&[(None, text)])
        }
    }

    fn sections(format: TextFormat, text: &str, max: usize, target: usize) -> Vec<Chunk> {
        chunk_sections(
            &text_doc(format, text),
            SourceKind::Text,
            &count,
            &SectionsCfg {
                max_tokens: max,
                target_tokens: target,
            },
        )
    }

    /// The lines `c` claims, from `text`.
    fn lines_of<'a>(text: &'a str, c: &Chunk) -> Vec<&'a str> {
        let (a, b) = (c.start_line.unwrap() as usize, c.end_line.unwrap() as usize);
        text.lines().skip(a - 1).take(b - a + 1).collect()
    }

    #[test]
    fn markdown_sections_carry_paths_lines_and_hashes() {
        let text = "# Fourier series\n\nIntro sentence one. Intro two.\n\n## Periodic signals\n\n\
                    A signal repeats. It has a period.\n\n### Symmetry\n\nEven functions have \
                    cosines only.\n\n## Gibbs\n\nOvershoot near jumps.\n";
        let chunks = sections(TextFormat::Markdown, text, 100, 80);
        let paths: Vec<&str> = chunks.iter().map(|c| c.section_path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "Fourier series",
                "Fourier series > Periodic signals",
                "Fourier series > Periodic signals > Symmetry",
                "Fourier series > Gibbs"
            ]
        );
        assert_eq!(
            chunks[0].text,
            "# Fourier series\n\nIntro sentence one. Intro two."
        );
        assert_eq!(
            (chunks[0].start_line, chunks[0].end_line),
            (Some(1), Some(3))
        );
        assert_eq!(
            (chunks[3].start_line, chunks[3].end_line),
            (Some(13), Some(15))
        );
        for c in &chunks {
            // Every line of the chunk's text is in its line range.
            let claimed = lines_of(text, c).join("\n");
            for line in c.text.lines().filter(|l| !l.is_empty()) {
                assert!(claimed.contains(line), "{line:?} not in {claimed:?}");
            }
            assert_eq!(c.hash, crate::chunk_hash(&c.section_path, &c.text));
            assert_eq!(c.hash.len(), 64);
        }
        // A section that is only a heading goes with the next one.
        let chunks = sections(TextFormat::Markdown, "# A\n\n## B\n\nText of b.\n", 100, 80);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "# A\n\n## B\n\nText of b.");
        assert_eq!(chunks[0].section_path, "A > B");
        // A heading alone at the end still has a chunk.
        let chunks = sections(TextFormat::Markdown, "Text.\n\n# Last\n", 100, 80);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[1].text, "# Last");
    }

    #[test]
    fn setext_headings_rules_and_closing_hashes() {
        let text = "Title\n=====\n\nOne.\n\nPart\n----\n\nTwo.\n\n---\n\n## Three ##\n\nThree.\n";
        let chunks = sections(TextFormat::Markdown, text, 100, 80);
        let paths: Vec<&str> = chunks.iter().map(|c| c.section_path.as_str()).collect();
        assert_eq!(paths, ["Title", "Title > Part", "Title > Three"]);
        assert!(!chunks[1].text.contains("---\n\n"));
    }

    #[test]
    fn sections_fill_to_the_target_and_overlap_by_one_sentence() {
        // One section of 60 five-word sentences in 6 paragraphs.
        let body: Vec<String> = (0..6).map(|p| prose(&format!("p{p}x"), 10)).collect();
        let text = format!("# Only\n\n{}", body.join("\n\n"));
        let chunks = sections(TextFormat::Markdown, &text, 120, 100);
        assert!(chunks.len() >= 3, "{}", chunks.len());
        for c in &chunks {
            assert!(count(&c.text) <= 120);
            assert_eq!(c.section_path, "Only");
        }
        for pair in chunks.windows(2) {
            // The second starts with exactly the first's last sentence.
            let last = pair[0].text.rsplit(['\n', ' ']).take(5).collect::<Vec<_>>();
            let last: Vec<&str> = last.into_iter().rev().collect();
            let first_words: Vec<&str> = pair[1].text.split([' ', '\n']).take(5).collect();
            assert_eq!(last, first_words);
            // …and not more: the sentence before it isn't repeated.
            let before = pair[0].text.split([' ', '\n']).rev().nth(5).unwrap();
            assert!(!pair[1].text.starts_with(before));
        }
        // Every sentence is in some chunk.
        let all: Vec<String> = chunks.iter().flat_map(|c| words(&c.text)).collect();
        for w in words(&text) {
            assert!(all.contains(&w), "{w}");
        }
        // A short tail stays with the rest when the whole fits the hard cap.
        let text = format!("# T\n\n{}\n\n{}", prose("a", 18), prose("b", 2));
        let chunks = sections(TextFormat::Markdown, &text, 110, 90);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn tables_fences_and_math_stay_whole() {
        let table = "| Signal | Series |\n|---|---|\n| Square | sin(nt)/n odd |\n| Saw | alt sin |";
        let fence = "```python\nimport numpy as np\n\ndef f(x):\n    return x\n```";
        let text = format!(
            "# Doc\n\n{}\n\n{table}\n\n{}\n\n{fence}\n\n$$\na^2 + b^2\n= c^2\n$$\n\n{}",
            prose("a", 6),
            prose("b", 6),
            prose("c", 6)
        );
        let chunks = sections(TextFormat::Markdown, &text, 40, 30);
        assert!(chunks.iter().any(|c| c.text.contains(table)), "{chunks:#?}");
        assert!(chunks.iter().any(|c| c.text.contains(fence)), "{chunks:#?}");
        assert!(
            chunks
                .iter()
                .any(|c| c.text.contains("$$\na^2 + b^2\n= c^2\n$$"))
        );
        // The fence keeps its blank line and indentation.
        let with_fence = chunks.iter().find(|c| c.text.contains(fence)).unwrap();
        assert!(lines_of(&text, with_fence).contains(&"    return x"));
        // A table too big for a chunk is cut between rows.
        let rows: String = (0..30).map(|i| format!("| r{i} | v{i} |\n")).collect();
        let chunks = sections(TextFormat::Markdown, &format!("# T\n\n{rows}"), 20, 16);
        assert!(chunks.len() > 2);
        for c in &chunks {
            assert!(count(&c.text) <= 20);
            assert!(
                c.text
                    .lines()
                    .filter(|l| !l.is_empty() && !l.starts_with('#'))
                    .all(|l| l.starts_with("| r") && l.ends_with(" |"))
            );
        }
    }

    #[test]
    fn rst_titles_take_levels_by_first_appearance() {
        let text = "============\nHeat engines\n============\n\nIntro.\n\n\
                    Efficiency\n==========\n\nThe first law.\n\n.. math::\n\n   \\eta = W / Q_h\n\n\
                    Carnot\n------\n\nReversible. Example::\n\n    code here\n    more\n\n\
                    ===== =====\nA     B\n===== =====\n1     2\n===== =====\n\n\
                    The second law\n==============\n\nEntropy.\n";
        let chunks = sections(TextFormat::Rst, text, 200, 150);
        let paths: Vec<&str> = chunks.iter().map(|c| c.section_path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "Heat engines",
                "Heat engines > Efficiency",
                "Heat engines > Efficiency > Carnot",
                "Heat engines > The second law"
            ]
        );
        // The title, not its adornment.
        assert_eq!(chunks[0].text, "Heat engines\n\nIntro.");
        assert_eq!(chunks[0].start_line, Some(1));
        assert!(chunks[1].text.contains(".. math::\n\n   \\eta = W / Q_h"));
        assert!(chunks[2].text.contains("    code here\n    more"));
        assert!(
            chunks[2]
                .text
                .contains("===== =====\nA     B\n===== =====\n1     2\n===== =====")
        );
    }

    #[test]
    fn latex_sections_and_environments() {
        let text = "\\documentclass{article}\n\\begin{document}\n\\section{Intro}\nWords here. \
                    More words.\n\n\\subsection*{Detail}\nA claim.\n\\begin{equation}\nE = mc^2\n\
                    \\end{equation}\n% a comment\n\\section[short]{Results {big}}\nDone.\n\
                    \\end{document}\n";
        let chunks = sections(TextFormat::Latex, text, 200, 150);
        let paths: Vec<&str> = chunks.iter().map(|c| c.section_path.as_str()).collect();
        assert_eq!(paths, ["", "Intro", "Intro > Detail", "Results {big}"]);
        assert!(
            chunks[2]
                .text
                .contains("\\begin{equation}\nE = mc^2\n\\end{equation}")
        );
        assert!(!chunks.iter().any(|c| c.text.contains("a comment")));
        assert_eq!(latex_heading("\\paragraph{P}"), Some((6, "P".into())));
        assert_eq!(latex_heading("\\partial{x}"), None);
    }

    #[test]
    fn pdf_pages_and_plain_text_have_no_headings_or_lines() {
        let pages = Extracted {
            format: TextFormat::Markdown, // ignored for PDFs
            ..doc(&[
                (Some(1), "# Not a heading\n\nFirst page."),
                (Some(2), &prose("q", 30)),
            ])
        };
        let chunks = chunk_sections(
            &pages,
            SourceKind::Pdf,
            &count,
            &SectionsCfg {
                max_tokens: 60,
                target_tokens: 50,
            },
        );
        assert_eq!(chunks[0].page, Some(1));
        assert_eq!(chunks[0].section_path, "");
        assert!(
            chunks
                .iter()
                .all(|c| c.start_line.is_none() && c.end_line.is_none())
        );
        assert!(chunks[1..].iter().all(|c| c.page == Some(2)));
        // Code: lines, never headings, indentation kept.
        let code = "# comment\nfn main() {\n    run();\n}\n";
        let chunks = chunk_sections(
            &doc(&[(None, code)]),
            SourceKind::Code,
            &count,
            &SectionsCfg::default(),
        );
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, code.trim_end());
        assert_eq!(
            (chunks[0].start_line, chunks[0].end_line),
            (Some(1), Some(4))
        );
        assert_eq!(chunks[0].section_path, "");
    }

    #[test]
    fn hashes_change_only_where_the_text_or_its_section_does() {
        let a = format!(
            "# One\n\n{}\n\n# Two\n\n{}\n\n# Three\n\n{}",
            prose("a", 4),
            prose("b", 4),
            prose("c", 4)
        );
        let edited = a.replace("b1a", "changed");
        let renamed = a.replace("# Three", "# Drei");
        let hashes = |t: &str| -> Vec<String> {
            sections(TextFormat::Markdown, t, 100, 80)
                .into_iter()
                .map(|c| c.hash)
                .collect()
        };
        let (h, e, r) = (hashes(&a), hashes(&a), hashes(&edited));
        assert_eq!(h, e);
        assert_eq!(h.len(), 3);
        assert_eq!(
            (h[0] == r[0], h[1] == r[1], h[2] == r[2]),
            (true, false, true)
        );
        let n = hashes(&renamed);
        assert_eq!(
            (h[0] == n[0], h[1] == n[1], h[2] == n[2]),
            (true, true, false)
        );
    }

    #[test]
    fn sniffing_finds_the_format() {
        assert_eq!(sniff_format("## Heading\ntext"), TextFormat::Markdown);
        assert_eq!(sniff_format("```\ncode\n```"), TextFormat::Markdown);
        assert_eq!(sniff_format("Title\n-----\n"), TextFormat::Rst);
        assert_eq!(sniff_format("\\section{A}"), TextFormat::Latex);
        assert_eq!(sniff_format("#hashtag and words"), TextFormat::Plain);
        assert_eq!(sniff_format("Short\n--\n"), TextFormat::Plain);
    }
}
