//! Markdown to HTML for lessons and notes, plus the text helpers margin notes use.
//!
//! Raw HTML in the Markdown is kept (lessons use `<details>` for "Check yourself"
//! answers) and then sanitized with ammonia so scripts and event handlers never reach
//! the webview. Math (`$…$`, `$$…$$`) is emitted as `<span class="math math-inline">`
//! / `<span class="math math-display">` holding the TeX source; KaTeX renders those
//! spans in the webview after each render.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd, html};

fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_MATH
        | Options::ENABLE_FOOTNOTES
}

/// Render Markdown to sanitized HTML. Headings move down a level (`#` is `<h2>`): the
/// screen around the Markdown owns the page's `<h1>` (a lesson's or note's title).
pub fn render(markdown: &str) -> String {
    let parser = Parser::new_ext(markdown, options()).map(demote_heading);
    let mut raw = String::with_capacity(markdown.len() * 3 / 2);
    html::push_html(&mut raw, parser);
    sanitize(&raw)
}

fn demote_heading(event: Event<'_>) -> Event<'_> {
    use pulldown_cmark::HeadingLevel::{self, *};
    fn lower(level: HeadingLevel) -> HeadingLevel {
        match level {
            H1 => H2,
            H2 => H3,
            H3 => H4,
            H4 => H5,
            H5 | H6 => H6,
        }
    }
    match event {
        Event::Start(Tag::Heading {
            level,
            id,
            classes,
            attrs,
        }) => Event::Start(Tag::Heading {
            level: lower(level),
            id,
            classes,
            attrs,
        }),
        Event::End(TagEnd::Heading(level)) => Event::End(TagEnd::Heading(lower(level))),
        other => other,
    }
}

fn sanitize(raw: &str) -> String {
    ammonia::Builder::default()
        .add_generic_attributes(&["class"])
        .add_tag_attributes("details", &["open"])
        .add_tag_attributes("input", &["type", "checked", "disabled"])
        .add_tags(&["input"])
        .clean(raw)
        .to_string()
}

/// The visible text of a Markdown document (what a reader can select), with
/// whitespace collapsed. Math is included as its TeX source.
pub fn plain_text(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    for event in Parser::new_ext(markdown, options()) {
        match event {
            Event::Text(t) | Event::Code(t) | Event::InlineMath(t) | Event::DisplayMath(t) => {
                out.push_str(&t)
            }
            Event::InlineHtml(t) | Event::Html(t) => out.push_str(&strip_tags(&t)),
            Event::SoftBreak | Event::HardBreak => out.push(' '),
            Event::End(TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item)
            | Event::Start(Tag::Paragraph) => out.push(' '),
            _ => {}
        }
    }
    normalize(&out)
}

fn strip_tags(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Collapse every run of whitespace to one space and trim.
pub fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Where a margin note's quote appears in the lesson, as a character offset into the
/// lesson's plain text. `None` when the quote is not found (it then sorts last).
pub fn quote_position(lesson_plain: &str, quote: &str) -> Option<usize> {
    let quote = normalize(quote);
    if quote.is_empty() {
        return None;
    }
    if let Some(i) = lesson_plain.find(&quote) {
        return Some(i);
    }
    // Selections across rendered math or formatting may not match exactly; fall back
    // to the first few words.
    let head: String = quote.split(' ').take(4).collect::<Vec<_>>().join(" ");
    if head.len() >= 8 {
        return lesson_plain.find(&head);
    }
    None
}

/// Sort key for margin notes: by where the quote appears, unmatched last, then by age.
pub fn margin_order<'a, T>(
    lesson_plain: &str,
    items: impl IntoIterator<Item = (Option<&'a str>, T)>,
) -> Vec<T> {
    let mut keyed: Vec<(usize, usize, T)> = items
        .into_iter()
        .enumerate()
        .map(|(i, (quote, item))| {
            let pos = quote
                .and_then(|q| quote_position(lesson_plain, q))
                .unwrap_or(usize::MAX);
            (pos, i, item)
        })
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    keyed.into_iter().map(|(_, _, item)| item).collect()
}

/// A short title for a note: the first non-empty line of the body (without Markdown
/// heading marks), else the quote, cut to 60 characters.
pub fn derive_title(body: &str, quote: Option<&str>) -> String {
    let line = body
        .lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty())
        .map(str::to_string)
        .or_else(|| quote.map(normalize))
        .unwrap_or_else(|| "Untitled note".to_string());
    truncate(&line, 60)
}

pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

/// One caption: start and end in seconds, and its text (lines joined with newlines).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Cue {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

/// Parses SubRip (`.srt`) captions into cues. Malformed blocks are skipped.
pub fn parse_srt(srt: &str) -> Vec<Cue> {
    fn seconds(t: &str) -> Option<f64> {
        let t = t.trim().replace(',', ".");
        let mut parts = t.split(':');
        let (h, m, s) = (parts.next()?, parts.next()?, parts.next()?);
        Some(
            h.parse::<f64>().ok()? * 3600.0
                + m.parse::<f64>().ok()? * 60.0
                + s.parse::<f64>().ok()?,
        )
    }
    let text = srt.replace("\r\n", "\n");
    let mut cues = Vec::new();
    for block in text.split("\n\n") {
        let mut lines = block.lines().skip_while(|l| !l.contains("-->"));
        let Some(timing) = lines.next() else { continue };
        let Some((a, b)) = timing.split_once("-->") else {
            continue;
        };
        let b = b.split_whitespace().next().unwrap_or("");
        let (Some(start), Some(end)) = (seconds(a), seconds(b)) else {
            continue;
        };
        let body: Vec<&str> = lines.filter(|l| !l.trim().is_empty()).collect();
        if !body.is_empty() && end > start {
            cues.push(Cue {
                start,
                end,
                text: body.join("\n"),
            });
        }
    }
    cues
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_srt_cues() {
        let cues = parse_srt(
            "1\r\n00:00:00,500 --> 00:00:04,000\r\nEvery periodic signal\r\nhides waves.\r\n\r\n2\n00:01:04,500 --> 00:01:09,250\nLet's find them.\n\n3\nbroken\n",
        );
        assert_eq!(cues.len(), 2);
        assert_eq!(
            cues[0],
            Cue {
                start: 0.5,
                end: 4.0,
                text: "Every periodic signal\nhides waves.".into()
            }
        );
        assert_eq!((cues[1].start, cues[1].end), (64.5, 69.25));
    }

    #[test]
    fn renders_basic_markdown() {
        let html = render("# Title\n\nSome *emphasis* and `code`.\n\n- a\n- b\n");
        // Headings sit under the screen's own <h1>.
        assert!(html.contains("<h2>Title</h2>"), "{html}");
        assert!(html.contains("<em>emphasis</em>"));
        assert!(html.contains("<code>code</code>"));
        assert!(html.contains("<li>a</li>"));
    }

    #[test]
    fn demotes_every_heading_level() {
        let html = render("# a\n\n## b\n\n### c\n\n#### d\n\n##### e\n\n###### f\n");
        for tag in [
            "<h2>a</h2>",
            "<h3>b</h3>",
            "<h4>c</h4>",
            "<h5>d</h5>",
            "<h6>e</h6>",
            "<h6>f</h6>",
        ] {
            assert!(html.contains(tag), "{tag} in {html}");
        }
        assert!(!html.contains("<h1>"));
    }

    #[test]
    fn keeps_details_and_summary() {
        let md =
            "## Check yourself\n\n<details><summary>Why?</summary>\n\nBecause.\n\n</details>\n";
        let html = render(md);
        assert!(html.contains("<details>"), "{html}");
        assert!(html.contains("<summary>Why?</summary>"), "{html}");
        assert!(html.contains("Because."), "{html}");
    }

    #[test]
    fn strips_scripts_and_handlers() {
        let html = render("<script>alert(1)</script>\n\n<img src=\"x.png\" onerror=\"alert(1)\">");
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("onerror"), "{html}");
    }

    #[test]
    fn emits_math_spans_for_katex() {
        let html = render("Euler: $e^{i\\pi} + 1 = 0$\n\n$$\\sum_{n=1}^\\infty a_n$$\n");
        assert!(
            html.contains(r#"<span class="math math-inline">e^{i\pi} + 1 = 0</span>"#),
            "{html}"
        );
        assert!(html.contains(r#"class="math math-display""#), "{html}");
    }

    #[test]
    fn escapes_html_inside_math() {
        let html = render("$a < b$");
        assert!(html.contains("a &lt; b"), "{html}");
    }

    #[test]
    fn plain_text_and_quote_positions() {
        let md = "# Why sums of sines?\n\nThe slowest wave sets the **period**.\nFaster ones repeat.\n\nAnother paragraph here.";
        let plain = plain_text(md);
        assert_eq!(
            plain,
            "Why sums of sines? The slowest wave sets the period. Faster ones repeat. Another paragraph here."
        );
        let a = quote_position(&plain, "Another paragraph").unwrap();
        let b = quote_position(&plain, "sets the\n period").unwrap();
        assert!(b < a);
        assert!(quote_position(&plain, "not in the text at all").is_none());
    }

    #[test]
    fn margin_order_sorts_by_quote_position() {
        let plain = plain_text("First line.\n\nSecond line.\n\nThird line.");
        let items = vec![
            (Some("Third line"), "c"),
            (None, "none"),
            (Some("First line"), "a"),
            (Some("Second"), "b"),
        ];
        assert_eq!(margin_order(&plain, items), vec!["a", "b", "c", "none"]);
    }

    #[test]
    fn titles_and_truncation() {
        assert_eq!(
            derive_title("\n## Gibbs overshoot\nmore", None),
            "Gibbs overshoot"
        );
        assert_eq!(derive_title("   ", Some("the  quote")), "the quote");
        assert_eq!(derive_title("", None), "Untitled note");
        assert_eq!(truncate("abcdef", 4), "abc…");
    }
}
