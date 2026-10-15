//! Citations in a lesson: their order, how their place in a source reads ("p. 3",
//! "1:05–1:20"), and what the source viewer shows for one. Pure data: no Dioxus here, so
//! it is unit-tested directly. The web pages a chat answer cites (numbered, with the
//! markers placed in the answer's text) are here too.

use margin_shared::{Citation, SourceKind, TranscriptSegment, WebCitation};

/// The lesson's citations by number, each number once (the first wins).
pub fn ordered(citations: &[Citation]) -> Vec<Citation> {
    let mut out: Vec<Citation> = Vec::with_capacity(citations.len());
    for c in citations {
        if !out.iter().any(|o| o.n == c.n) {
            out.push(c.clone());
        }
    }
    out.sort_by_key(|c| c.n);
    out
}

/// "0:07", "1:05", "1:02:05".
pub fn clock(ms: u64) -> String {
    let s = ms / 1000;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// The lines of a text document a citation spans: "lines 12–18", "line 12".
pub fn lines_text(c: &Citation) -> Option<String> {
    let start = c.start_line?;
    Some(match c.end_line.filter(|&e| e > start) {
        Some(end) => format!("lines {start}–{end}"),
        None => format!("line {start}"),
    })
}

/// Where in its source a citation points: "p. 3", "lines 12–18", "1:05–1:20", "1:05";
/// for a file of an attached folder without a page, its path in the folder (and its
/// lines); `None` when the whole source is meant (an image, a passage without lines).
pub fn locator(c: &Citation) -> Option<String> {
    if let Some(page) = c.page {
        return Some(format!("p. {page}"));
    }
    let lines = lines_text(c);
    if let Some(file) = &c.file {
        return Some(match lines {
            Some(lines) => format!("{} · {lines}", file.path),
            None => file.path.clone(),
        });
    }
    if lines.is_some() {
        return lines;
    }
    match (c.start_ms, c.end_ms) {
        (Some(start), Some(end)) if end > start => Some(format!("{}–{}", clock(start), clock(end))),
        (Some(start), _) => Some(clock(start)),
        _ => None,
    }
}

/// For a file of an attached folder: "In “notes” on MacBook Pro · ch1/intro.md" (from
/// the folder, when it is still attached), else "In a folder that is no longer attached
/// · ch1/intro.md". `None` for other citations.
pub fn folder_line(c: &Citation, workspace: Option<&margin_shared::Workspace>) -> Option<String> {
    let file = c.file.as_ref()?;
    Some(match workspace {
        Some(ws) => format!("In “{}” on {} · {}", ws.name, ws.device_name, file.path),
        None => format!("In a folder that is no longer attached · {}", file.path),
    })
}

/// What the viewer says when a folder file's picture or original no longer loads.
pub const FOLDER_FILE_GONE: &str =
    "This file is no longer in the folder; this is the passage as it was.";

pub fn kind_label(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Text => "Text",
        SourceKind::Code => "Code",
        SourceKind::Pdf => "PDF",
        SourceKind::Image => "Image",
        SourceKind::Audio => "Audio",
        SourceKind::Video => "Video",
    }
}

/// The accessible name of a citation marker: "Source 2: lecture-12.m4a".
pub fn marker_label(n: u32, source_name: &str) -> String {
    format!("Source {n}: {source_name}")
}

/// The citations before and after `n` in `ordered`, for the viewer's Previous and Next.
pub fn neighbors(ordered: &[Citation], n: u32) -> (Option<u32>, Option<u32>) {
    let Some(i) = ordered.iter().position(|c| c.n == n) else {
        return (None, None);
    };
    let prev = i.checked_sub(1).map(|j| ordered[j].n);
    let next = ordered.get(i + 1).map(|c| c.n);
    (prev, next)
}

/// `n`'s place in the list: (1-based position, count).
pub fn position(ordered: &[Citation], n: u32) -> Option<(usize, usize)> {
    ordered
        .iter()
        .position(|c| c.n == n)
        .map(|i| (i + 1, ordered.len()))
}

/// Seconds for a media element's `currentTime`.
pub fn seconds(ms: u64) -> f64 {
    ms as f64 / 1000.0
}

/// A media URL that starts playing at `start_ms` (a W3C media fragment, `#t=`). The
/// fragment never reaches the service, so a signed query string stays valid.
pub fn media_at(url: &str, start_ms: Option<u64>) -> String {
    let base = url.split('#').next().unwrap_or(url);
    match start_ms {
        Some(ms) if ms > 0 => format!("{base}#t={}", trim_float(seconds(ms))),
        _ => base.to_string(),
    }
}

fn trim_float(v: f64) -> String {
    let s = format!("{v:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The URL that opens the whole source in the browser: at the cited page for a PDF, at
/// the cited time for audio and video. `None` when the service offers no original.
pub fn open_url(c: &Citation, media_url: Option<&str>) -> Option<String> {
    let url = media_url?;
    let base = url.split('#').next().unwrap_or(url);
    Some(match (c.kind, c.page) {
        (SourceKind::Pdf, Some(page)) => format!("{base}#page={page}"),
        (SourceKind::Audio | SourceKind::Video, _) => media_at(base, c.start_ms),
        _ => base.to_string(),
    })
}

/// "Open PDF", "Open video"…: the label of the viewer's button for `open_url`.
pub fn open_label(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Pdf => "Open PDF",
        SourceKind::Image => "Open image",
        SourceKind::Audio => "Open audio",
        SourceKind::Video => "Open video",
        SourceKind::Text | SourceKind::Code => "Open file",
    }
}

/// A timed line of an audio or video source, as the viewer's transcript lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    /// The citation it comes from (0 for a line of the whole transcript).
    pub n: u32,
    pub start_ms: u64,
    pub end_ms: Option<u64>,
    pub text: String,
    /// Within the passage the open citation points at.
    pub current: bool,
    /// Within a passage another of the lesson's citations points at.
    pub cited: bool,
}

/// What the source viewer shows for a citation.
#[derive(Debug, Clone, PartialEq)]
pub enum Viewer {
    /// An image file, full size (fit to the dialog).
    Image { url: String },
    /// A PDF page: its rendered image when the service has one, and the page's text.
    PdfPage {
        page: Option<u32>,
        image: Option<String>,
        excerpt: String,
    },
    /// An audio clip: the player from `start_ms`, and the source's whole transcript (or,
    /// without one, its cited passages) with this citation's span marked.
    Audio {
        src: Option<String>,
        start_ms: u64,
        transcript: Vec<Segment>,
    },
    /// A video from the frame's time; the frame is the poster until it plays.
    Video {
        src: Option<String>,
        start_ms: u64,
        poster: Option<String>,
        transcript: Vec<Segment>,
    },
    /// A text or code passage.
    Passage { excerpt: String, code: bool },
    /// A text or code passage that knows its lines: the original file's lines around
    /// them (fetched from `original`), the cited ones marked; the passage alone until
    /// then, or when the original can't be had or no longer matches.
    Lines {
        excerpt: String,
        code: bool,
        start_line: u32,
        end_line: u32,
        /// The original file's URL, when its lines are the ones the passage counts
        /// (not for text read out of HTML).
        original: Option<String>,
    },
    /// Nothing to show: the source has no picture and the passage no text.
    Missing,
}

/// What to show for `c`, given the lesson's citations (for an audio or video source's
/// other passages), the source's `media_url` (`None` when the source is gone or the
/// service offers no original) and its whole transcript (`lines`; empty when there is
/// none or it hasn't loaded).
pub fn viewer(
    c: &Citation,
    all: &[Citation],
    media_url: Option<&str>,
    lines: &[TranscriptSegment],
) -> Viewer {
    let excerpt = c.excerpt.trim().to_string();
    match c.kind {
        SourceKind::Image => match c.image_url.as_deref().or(media_url) {
            Some(url) => Viewer::Image {
                url: url.to_string(),
            },
            None if !excerpt.is_empty() => Viewer::Passage {
                excerpt,
                code: false,
            },
            None => Viewer::Missing,
        },
        SourceKind::Pdf => Viewer::PdfPage {
            page: c.page,
            image: c.image_url.clone(),
            excerpt,
        },
        SourceKind::Audio => Viewer::Audio {
            src: media_url.map(str::to_string),
            start_ms: c.start_ms.unwrap_or(0),
            transcript: whole_transcript(c, all, lines),
        },
        SourceKind::Video => Viewer::Video {
            src: media_url.map(str::to_string),
            start_ms: c.start_ms.unwrap_or(0),
            poster: c.image_url.clone(),
            transcript: whole_transcript(c, all, lines),
        },
        SourceKind::Text | SourceKind::Code if excerpt.is_empty() => Viewer::Missing,
        SourceKind::Text | SourceKind::Code => match c.start_line {
            Some(start_line) => Viewer::Lines {
                excerpt,
                code: c.kind == SourceKind::Code,
                start_line,
                end_line: c.end_line.unwrap_or(start_line).max(start_line),
                original: media_url
                    .filter(|_| lines_are_the_files(&c.source_name))
                    .map(str::to_string),
            },
            None => Viewer::Passage {
                excerpt,
                code: c.kind == SourceKind::Code,
            },
        },
    }
}

/// Whether a text file's own lines are the ones its passages count: not for HTML, whose
/// text is read out of the markup first.
pub fn lines_are_the_files(name: &str) -> bool {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    !matches!(ext.as_str(), "html" | "htm" | "xhtml")
}

/// Lines shown before and after the cited ones.
pub const CONTEXT_LINES: u32 = 12;

/// A line of a text file as the viewer shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct NumberedLine {
    /// 1-based.
    pub number: u32,
    pub text: String,
    /// Within the cited lines.
    pub cited: bool,
}

/// The lowercase words of `text` (letters and digits), for comparing a passage with the
/// file's lines whatever the spacing or markup between them.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The lines of `original` from `CONTEXT_LINES` before `start` to as many after `end`,
/// the cited ones (`start..=end`) marked. `None` when the file doesn't hold the passage
/// there (it changed since it was read): its first words must be among the cited lines.
pub fn line_window(
    original: &str,
    start: u32,
    end: u32,
    excerpt: &str,
) -> Option<Vec<NumberedLine>> {
    let lines: Vec<&str> = original.lines().collect();
    let total = lines.len() as u32;
    let end = end.max(start);
    if start == 0 || start > total {
        return None;
    }
    let end = end.min(total);
    let cited = words(&lines[(start - 1) as usize..end as usize].join("\n"));
    let first: Vec<String> = words(excerpt).into_iter().take(6).collect();
    if first.is_empty() || !cited.windows(first.len()).any(|w| w == first.as_slice()) {
        return None;
    }
    let from = start.saturating_sub(CONTEXT_LINES).max(1);
    let to = (end + CONTEXT_LINES).min(total);
    Some(
        (from..=to)
            .map(|n| NumberedLine {
                number: n,
                text: lines[(n - 1) as usize].to_string(),
                cited: (start..=end).contains(&n),
            })
            .collect(),
    )
}

/// The stretch of the recording a citation points at: its span, or for a moment (a
/// video frame) that moment.
fn cited_span(c: &Citation) -> Option<(u64, u64)> {
    let start = c.start_ms?;
    Some((start, c.end_ms.filter(|&e| e > start).unwrap_or(start)))
}

/// Whether a transcript line [`start`, `end`] falls within the cited `span`: they
/// overlap, or (for a moment) the line is the one being said then.
fn line_in(start: u64, end: u64, span: (u64, u64)) -> bool {
    if span.0 == span.1 {
        start <= span.0 && span.0 <= end
    } else {
        start < span.1 && span.0 < end.max(start + 1)
    }
}

/// `c`'s source's whole transcript, line by line, the lines within `c`'s span marked
/// `current` and those within the lesson's other citations of it `cited`. Without one
/// (`lines` empty), the passages the lesson cites (`transcript`).
pub fn whole_transcript(
    c: &Citation,
    all: &[Citation],
    lines: &[TranscriptSegment],
) -> Vec<Segment> {
    if lines.is_empty() {
        return transcript(c, all);
    }
    let here = cited_span(c);
    let others: Vec<(u64, u64)> = all
        .iter()
        .filter(|o| o.source_id == c.source_id && o.n != c.n)
        .filter_map(cited_span)
        .collect();
    let mut out: Vec<Segment> = lines
        .iter()
        .map(|l| Segment {
            n: 0,
            start_ms: l.start_ms,
            end_ms: Some(l.end_ms),
            text: l.text.clone(),
            current: here.is_some_and(|s| line_in(l.start_ms, l.end_ms, s)),
            cited: others.iter().any(|&s| line_in(l.start_ms, l.end_ms, s)),
        })
        .collect();
    // A moment between two lines (a pause) marks the line after it.
    if let Some(span) = here
        && !out.iter().any(|s| s.current)
        && let Some(next) = out.iter_mut().find(|s| s.start_ms >= span.0)
    {
        next.current = true;
    }
    out
}

/// Index of the first line the open citation points at, to scroll it into view.
pub fn first_current(segments: &[Segment]) -> Option<usize> {
    segments.iter().position(|s| s.current)
}

/// The timed passages of `c`'s source that the lesson cites, in time order, with `c`'s
/// marked. Passages without text (bare video frames) are left out; one cited twice is
/// listed once.
pub fn transcript(c: &Citation, all: &[Citation]) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    for other in all.iter().filter(|o| o.source_id == c.source_id) {
        let (Some(start_ms), text) = (other.start_ms, other.excerpt.trim()) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        let current = other.n == c.n;
        if let Some(seen) = out
            .iter_mut()
            .find(|s| s.start_ms == start_ms && s.text == text)
        {
            seen.current |= current;
            seen.cited = !seen.current;
            continue;
        }
        out.push(Segment {
            n: other.n,
            start_ms,
            end_ms: other.end_ms,
            text: text.to_string(),
            current,
            cited: !current,
        });
    }
    out.sort_by_key(|s| (s.start_ms, s.n));
    out
}

// ---------------------------------------------------------------------------
// Web citations: the pages a hosted web tool cited in a chat answer
// (`AssistantText.citations`, `docs/agent-providers-brief.md` D8).
// ---------------------------------------------------------------------------

/// A page an answer cites, numbered by first appearance: one number per address, however
/// many passages of it are cited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSource {
    pub n: u32,
    pub url: String,
    /// The page's title, else its host.
    pub title: String,
    /// "en.wikipedia.org" (without "www.").
    pub host: String,
    /// The cited passages, in order, each once.
    pub quotes: Vec<String>,
}

/// The host of `url` without "www.": "https://www.example.com/a" → "example.com"; the
/// address itself when it has none.
pub fn host(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(rest)
        .rsplit('@')
        .next()
        .unwrap_or_default();
    let host = host.strip_prefix("www.").unwrap_or(host);
    if host.is_empty() {
        url.to_string()
    } else {
        host.to_string()
    }
}

/// The pages `citations` cite, numbered in order of first appearance.
pub fn web_sources(citations: &[WebCitation]) -> Vec<WebSource> {
    let mut out: Vec<WebSource> = Vec::new();
    for c in citations {
        let quote = c
            .cited_text
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty());
        if let Some(source) = out.iter_mut().find(|s| s.url == c.url) {
            if let Some(q) = quote
                && !source.quotes.iter().any(|have| have == q)
            {
                source.quotes.push(q.to_string());
            }
            continue;
        }
        let host = host(&c.url);
        let title = c
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(String::from)
            .unwrap_or_else(|| host.clone());
        out.push(WebSource {
            n: out.len() as u32 + 1,
            url: c.url.clone(),
            title,
            host,
            quotes: quote.map(|q| vec![q.to_string()]).unwrap_or_default(),
        });
    }
    out
}

/// A source's tooltip: its cited passages, quoted, else its address.
pub fn web_tooltip(source: &WebSource) -> String {
    if source.quotes.is_empty() {
        return source.url.clone();
    }
    source
        .quotes
        .iter()
        .map(|q| format!("“{q}”"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Where a marker for a cited span ending at byte `end` goes in `text`: on a character
/// boundary, after the word it would split, and before the whitespace the span ends with
/// (so a span that ends a paragraph keeps its marker in that paragraph).
fn marker_at(text: &str, end: usize) -> usize {
    let mut at = end.min(text.len());
    while !text.is_char_boundary(at) {
        at += 1;
    }
    // Don't split a word.
    if text[..at]
        .chars()
        .next_back()
        .is_some_and(char::is_alphanumeric)
    {
        at += text[at..]
            .char_indices()
            .find(|(_, c)| !c.is_alphanumeric())
            .map_or(text.len() - at, |(i, _)| i);
    }
    // Back over trailing whitespace.
    while at > 0
        && text[..at]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
    {
        at -= text[..at].chars().next_back().map_or(1, char::len_utf8);
    }
    at
}

/// The answer's text with a `[^n]` marker where each cited span ends (citations that
/// give offsets; the others are only listed under the answer). Markers at one place are
/// in number order, each once.
pub fn web_marked(text: &str, citations: &[WebCitation]) -> String {
    let sources = web_sources(citations);
    let mut marks: Vec<(usize, u32)> = citations
        .iter()
        .filter_map(|c| {
            let end = c.end? as usize;
            let n = sources.iter().find(|s| s.url == c.url)?.n;
            Some((marker_at(text, end), n))
        })
        .collect();
    marks.sort();
    marks.dedup();
    let mut out = String::with_capacity(text.len() + marks.len() * 4);
    let mut from = 0;
    for (at, n) in marks {
        out.push_str(&text[from..at]);
        out.push_str(&format!("[^{n}]"));
        from = at;
    }
    out.push_str(&text[from..]);
    out
}

/// The sources as citations the lesson renderer makes marker buttons for
/// (`markdown::render_lesson`): numbered alike, named "Title · host".
pub fn web_marker_citations(sources: &[WebSource]) -> Vec<Citation> {
    sources
        .iter()
        .map(|s| Citation {
            n: s.n,
            source_id: s.url.clone(),
            source_name: if s.title == s.host {
                s.host.clone()
            } else {
                format!("{} · {}", s.title, s.host)
            },
            kind: SourceKind::Text,
            page: None,
            start_ms: None,
            end_ms: None,
            excerpt: s.quotes.join(" "),
            image_url: None,
            document_id: None,
            start_line: None,
            end_line: None,
            file: None,
            section_path: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cite(n: u32, source: &str, kind: SourceKind) -> Citation {
        Citation {
            n,
            source_id: source.into(),
            source_name: format!("{source} file"),
            kind,
            page: None,
            start_ms: None,
            end_ms: None,
            excerpt: format!("passage {n}"),
            image_url: None,
            document_id: None,
            start_line: None,
            end_line: None,
            file: None,
            section_path: None,
        }
    }

    #[test]
    fn citations_are_listed_by_number_once() {
        let list = vec![
            cite(3, "a", SourceKind::Text),
            cite(1, "b", SourceKind::Pdf),
            cite(2, "c", SourceKind::Code),
            Citation {
                excerpt: "a duplicate".into(),
                ..cite(1, "z", SourceKind::Text)
            },
        ];
        let ordered = ordered(&list);
        let ns: Vec<u32> = ordered.iter().map(|c| c.n).collect();
        assert_eq!(ns, vec![1, 2, 3]);
        assert_eq!(ordered[0].source_id, "b");
        assert_eq!(neighbors(&ordered, 1), (None, Some(2)));
        assert_eq!(neighbors(&ordered, 2), (Some(1), Some(3)));
        assert_eq!(neighbors(&ordered, 3), (Some(2), None));
        assert_eq!(neighbors(&ordered, 9), (None, None));
        assert_eq!(position(&ordered, 2), Some((2, 3)));
        assert_eq!(position(&ordered, 9), None);
    }

    #[test]
    fn locators_read_as_pages_and_times() {
        assert_eq!(clock(7_400), "0:07");
        assert_eq!(clock(65_000), "1:05");
        assert_eq!(clock(3_725_000), "1:02:05");
        let page = Citation {
            page: Some(3),
            ..cite(1, "a", SourceKind::Pdf)
        };
        assert_eq!(locator(&page).as_deref(), Some("p. 3"));
        let clip = Citation {
            start_ms: Some(65_000),
            end_ms: Some(80_500),
            ..cite(2, "b", SourceKind::Audio)
        };
        assert_eq!(locator(&clip).as_deref(), Some("1:05–1:20"));
        let frame = Citation {
            start_ms: Some(4_000),
            ..cite(3, "c", SourceKind::Video)
        };
        assert_eq!(locator(&frame).as_deref(), Some("0:04"));
        assert_eq!(locator(&cite(4, "d", SourceKind::Image)), None);
        assert_eq!(marker_label(2, "talk.m4a"), "Source 2: talk.m4a");
    }

    #[test]
    fn locators_read_as_lines() {
        let lines = Citation {
            start_line: Some(12),
            end_line: Some(18),
            ..cite(1, "a", SourceKind::Text)
        };
        assert_eq!(locator(&lines).as_deref(), Some("lines 12–18"));
        let one = Citation {
            start_line: Some(4),
            end_line: Some(4),
            ..cite(2, "a", SourceKind::Code)
        };
        assert_eq!(locator(&one).as_deref(), Some("line 4"));
        assert_eq!(lines_text(&cite(3, "a", SourceKind::Text)), None);
        // A folder's file: its path, then its lines.
        let mut file = lines.clone();
        file.file = Some(margin_shared::WorkspaceFile {
            workspace_id: "w1".into(),
            path: "ch1/intro.md".into(),
        });
        assert_eq!(
            locator(&file).as_deref(),
            Some("ch1/intro.md · lines 12–18")
        );
        // A page wins over lines.
        let page = Citation {
            page: Some(2),
            ..lines.clone()
        };
        assert_eq!(locator(&page).as_deref(), Some("p. 2"));
    }

    #[test]
    fn cited_lines_open_in_the_file() {
        let c = Citation {
            excerpt: "Overshoot near the jump stays.".into(),
            start_line: Some(14),
            end_line: Some(15),
            ..cite(1, "a", SourceKind::Text)
        };
        let all = std::slice::from_ref(&c);
        assert_eq!(
            viewer(&c, all, Some("orig"), &[]),
            Viewer::Lines {
                excerpt: "Overshoot near the jump stays.".into(),
                code: false,
                start_line: 14,
                end_line: 15,
                original: Some("orig".into()),
            }
        );
        // HTML's lines are of the text read out of it, not of the file.
        let html = Citation {
            source_name: "page.html".into(),
            ..c.clone()
        };
        assert!(matches!(
            viewer(&html, all, Some("orig"), &[]),
            Viewer::Lines { original: None, .. }
        ));
        assert!(lines_are_the_files("notes/a.MD"));
        assert!(!lines_are_the_files("x/page.HTM"));

        let file: String = (1..=40)
            .map(|n| match n {
                14 => "## Gibbs: overshoot".to_string(),
                15 => "near the *jump* stays.".to_string(),
                n => format!("line {n}"),
            })
            .collect::<Vec<_>>()
            .join("\n");
        let window = line_window(&file, 14, 15, &c.excerpt).unwrap();
        assert_eq!(window.first().unwrap().number, 14 - CONTEXT_LINES);
        assert_eq!(window.last().unwrap().number, 15 + CONTEXT_LINES);
        let cited: Vec<u32> = window
            .iter()
            .filter(|l| l.cited)
            .map(|l| l.number)
            .collect();
        assert_eq!(cited, [14, 15]);
        assert_eq!(window[CONTEXT_LINES as usize].text, "## Gibbs: overshoot");
        // Near the ends the window stops at the file's first and last lines.
        let top = line_window(&file, 1, 1, "line 1").unwrap();
        assert_eq!((top[0].number, top.len()), (1, 1 + CONTEXT_LINES as usize));
        let bottom = line_window(&file, 40, 99, "line 40").unwrap();
        assert_eq!(bottom.last().unwrap().number, 40);
        // The file changed since it was read, or the lines are past its end: none.
        assert_eq!(line_window(&file, 20, 21, &c.excerpt), None);
        assert_eq!(line_window(&file, 41, 42, "line"), None);
        assert_eq!(line_window(&file, 0, 2, "line 1"), None);
        assert_eq!(line_window(&file, 14, 15, "  "), None);
    }

    #[test]
    fn media_urls_start_at_the_cited_time_and_page() {
        let signed = "http://s/media/sources/a/original?exp=1&sig=ab";
        assert_eq!(media_at(signed, Some(65_500)), format!("{signed}#t=65.5"));
        assert_eq!(media_at(signed, Some(0)), signed);
        assert_eq!(media_at(&format!("{signed}#t=3"), None), signed);
        let page = Citation {
            page: Some(3),
            ..cite(1, "a", SourceKind::Pdf)
        };
        assert_eq!(
            open_url(&page, Some(signed)),
            Some(format!("{signed}#page=3"))
        );
        let clip = Citation {
            start_ms: Some(12_000),
            ..cite(2, "b", SourceKind::Video)
        };
        assert_eq!(
            open_url(&clip, Some(signed)),
            Some(format!("{signed}#t=12"))
        );
        assert_eq!(
            open_url(&cite(3, "c", SourceKind::Text), Some(signed)),
            Some(signed.to_string())
        );
        assert_eq!(open_url(&clip, None), None);
    }

    #[test]
    fn the_viewer_shows_each_kind() {
        let image = Citation {
            image_url: Some("img".into()),
            excerpt: String::new(),
            ..cite(1, "i", SourceKind::Image)
        };
        assert_eq!(
            viewer(&image, &[], Some("orig"), &[]),
            Viewer::Image { url: "img".into() }
        );
        // Without a picture of its own, an image shows the original.
        let bare = Citation {
            excerpt: String::new(),
            ..cite(1, "i", SourceKind::Image)
        };
        assert_eq!(
            viewer(&bare, &[], Some("orig"), &[]),
            Viewer::Image { url: "orig".into() }
        );
        assert_eq!(viewer(&bare, &[], None, &[]), Viewer::Missing);

        let page = Citation {
            page: Some(4),
            image_url: Some("page-4.png".into()),
            ..cite(2, "p", SourceKind::Pdf)
        };
        assert_eq!(
            viewer(&page, &[], Some("orig"), &[]),
            Viewer::PdfPage {
                page: Some(4),
                image: Some("page-4.png".into()),
                excerpt: "passage 2".into()
            }
        );

        let code = cite(3, "c", SourceKind::Code);
        assert_eq!(
            viewer(&code, &[], None, &[]),
            Viewer::Passage {
                excerpt: "passage 3".into(),
                code: true
            }
        );
        let empty = Citation {
            excerpt: "  ".into(),
            ..cite(4, "t", SourceKind::Text)
        };
        assert_eq!(viewer(&empty, &[], None, &[]), Viewer::Missing);
    }

    #[test]
    fn audio_and_video_list_the_sources_cited_passages() {
        let clip = |n, start, text: &str| Citation {
            start_ms: Some(start),
            end_ms: Some(start + 5_000),
            excerpt: text.into(),
            ..cite(n, "talk", SourceKind::Audio)
        };
        let all = vec![
            clip(5, 40_000, "Later on."),
            clip(2, 10_000, "The overshoot stays."),
            cite(3, "other", SourceKind::Text),
            clip(6, 10_000, "The overshoot stays."),
            clip(7, 20_000, ""),
        ];
        let Viewer::Audio {
            src,
            start_ms,
            transcript,
        } = viewer(&all[3], &all, Some("http://s/a.m4a"), &[])
        else {
            panic!("not audio");
        };
        assert_eq!(src.as_deref(), Some("http://s/a.m4a"));
        assert_eq!(start_ms, 10_000);
        let texts: Vec<(&str, bool)> = transcript
            .iter()
            .map(|s| (s.text.as_str(), s.current))
            .collect();
        assert_eq!(
            texts,
            vec![("The overshoot stays.", true), ("Later on.", false)]
        );
        assert_eq!(transcript[0].n, 2);

        let frame = Citation {
            start_ms: Some(4_000),
            image_url: Some("frame-4000.jpg".into()),
            excerpt: String::new(),
            ..cite(1, "film", SourceKind::Video)
        };
        assert_eq!(
            viewer(&frame, std::slice::from_ref(&frame), None, &[]),
            Viewer::Video {
                src: None,
                start_ms: 4_000,
                poster: Some("frame-4000.jpg".into()),
                transcript: Vec::new(),
            }
        );
    }

    #[test]
    fn the_whole_transcript_marks_the_cited_spans() {
        let line = |start_ms, end_ms, text: &str| TranscriptSegment {
            start_ms,
            end_ms,
            text: text.into(),
        };
        let lines = vec![
            line(0, 1_050, "Lecture twelve."),
            line(1_200, 3_325, "Today we look at a jump."),
            line(3_725, 6_000, "Near a discontinuity,"),
            line(6_000, 8_560, "the sums overshoot."),
            line(8_960, 13_752, "More terms squeeze it."),
            line(14_152, 16_420, "That is Gibbs."),
        ];
        let clip = |n, start, end| Citation {
            start_ms: Some(start),
            end_ms: Some(end),
            ..cite(n, "talk", SourceKind::Audio)
        };
        let all = vec![
            clip(6, 3_725, 8_560),
            clip(7, 8_960, 13_752),
            cite(2, "other", SourceKind::Text),
        ];
        let Viewer::Audio { transcript, .. } = viewer(&all[0], &all, Some("a"), &lines) else {
            panic!("not audio");
        };
        // Every line, the open citation's span current, the other one's cited.
        assert_eq!(transcript.len(), 6);
        let marks: Vec<(bool, bool)> = transcript.iter().map(|s| (s.current, s.cited)).collect();
        assert_eq!(
            marks,
            vec![
                (false, false),
                (false, false),
                (true, false),
                (true, false),
                (false, true),
                (false, false)
            ]
        );
        assert_eq!(first_current(&transcript), Some(2));
        assert_eq!(transcript[3].end_ms, Some(8_560));

        // A video frame's moment marks the line said then, or after a pause the next.
        let frame = |at| Citation {
            start_ms: Some(at),
            ..cite(1, "talk", SourceKind::Video)
        };
        let at = whole_transcript(&frame(2_000), &[], &lines);
        assert_eq!(first_current(&at), Some(1));
        assert_eq!(at.iter().filter(|s| s.current).count(), 1);
        let pause = whole_transcript(&frame(3_500), &[], &lines);
        assert_eq!(first_current(&pause), Some(2));
        // Without a transcript, the cited passages stand in.
        assert_eq!(
            whole_transcript(&all[0], &all, &[]),
            super::transcript(&all[0], &all)
        );
    }

    #[test]
    fn folder_citations_say_where_the_file_is() {
        use margin_shared::{Platform, Workspace, WorkspaceFile};
        let mut c = cite(1, "f1", SourceKind::Text);
        assert_eq!(folder_line(&c, None), None);
        c.file = Some(WorkspaceFile {
            workspace_id: "w1".into(),
            path: "ch1/intro.md".into(),
        });
        assert_eq!(locator(&c).as_deref(), Some("ch1/intro.md"));
        let ws = Workspace {
            id: "w1".into(),
            name: "notes".into(),
            device_id: "d1".into(),
            device_name: "MacBook Pro".into(),
            platform: Platform::Macos,
            location: "~/notes".into(),
            created_at: chrono::Utc::now(),
            sync: Default::default(),
            unreadable: Vec::new(),
        };
        assert_eq!(
            folder_line(&c, Some(&ws)).as_deref(),
            Some("In “notes” on MacBook Pro · ch1/intro.md")
        );
        assert_eq!(
            folder_line(&c, None).as_deref(),
            Some("In a folder that is no longer attached · ch1/intro.md")
        );
        c.page = Some(4);
        assert_eq!(locator(&c).as_deref(), Some("p. 4"));
    }

    fn web(
        url: &str,
        title: Option<&str>,
        quote: Option<&str>,
        span: Option<(u32, u32)>,
    ) -> WebCitation {
        WebCitation {
            url: url.into(),
            title: title.map(String::from),
            cited_text: quote.map(String::from),
            start: span.map(|s| s.0),
            end: span.map(|s| s.1),
        }
    }

    #[test]
    fn web_sources_number_each_page_once() {
        assert_eq!(host("https://www.example.com/a?b#c"), "example.com");
        assert_eq!(host("https://user@en.wikipedia.org"), "en.wikipedia.org");
        assert_eq!(host("not a url"), "not a url");
        let cites = [
            web("https://a.org/x", Some("A page"), Some("one"), None),
            web("https://b.org/", None, None, None),
            web("https://a.org/x", Some("A page"), Some("two"), None),
            web("https://a.org/x", Some("A page"), Some("one"), None),
        ];
        let sources = web_sources(&cites);
        assert_eq!(sources.len(), 2);
        assert_eq!((sources[0].n, sources[1].n), (1, 2));
        assert_eq!(sources[0].quotes, vec!["one", "two"]);
        assert_eq!(sources[1].title, "b.org");
        assert_eq!(web_tooltip(&sources[0]), "“one”\n“two”");
        assert_eq!(web_tooltip(&sources[1]), "https://b.org/");
        let markers = web_marker_citations(&sources);
        assert_eq!(markers[0].source_name, "A page · a.org");
        assert_eq!(markers[1].source_name, "b.org");
    }

    #[test]
    fn web_markers_go_where_cited_spans_end() {
        let text = "First fact. Second fact.\n\nThird.";
        let cites = [
            web("https://a.org", None, None, Some((0, 11))),
            web("https://b.org", None, None, Some((12, 26))),
            // Mid-word: after the word. Same place and page twice: one marker.
            web("https://a.org", None, None, Some((28, 30))),
            web("https://a.org", None, None, Some((28, 30))),
            // No offsets: listed, not marked.
            web("https://c.org", None, None, None),
        ];
        assert_eq!(
            web_marked(text, &cites),
            "First fact.[^1] Second fact.[^2]\n\nThird[^1]."
        );
        // Offsets past the end or inside a character are safe.
        let cites = [web("https://a.org", None, None, Some((0, 99)))];
        assert_eq!(web_marked("Ünï", &cites), "Ünï[^1]");
        let cites = [web("https://a.org", None, None, Some((0, 1)))];
        assert_eq!(web_marked("Ü x", &cites), "Ü[^1] x");
        // The sample answer: every marker lands at the end of its sentence.
        let (text, cites) = crate::fake::sample_web_answer();
        let marked = web_marked(&text, &cites);
        assert!(
            marked.contains("(July and October).[^1] It defined"),
            "{marked}"
        );
        assert!(
            marked.contains("into print.[^2]\n\nA year later"),
            "{marked}"
        );
        assert!(marked.ends_with("Warren Weaver.[^1]"), "{marked}");
    }
}
