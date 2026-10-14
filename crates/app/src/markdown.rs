//! Markdown to HTML for lessons and notes, plus the text helpers margin notes use.
//!
//! Raw HTML in the Markdown is kept (lessons use `<details>` for "Check yourself"
//! answers) and then sanitized with ammonia so scripts and event handlers never reach
//! the webview. Math (`$…$`, `$$…$$`) is emitted as `<span class="math math-inline">`
//! / `<span class="math math-display">` holding the TeX source; KaTeX renders those
//! spans in the webview after each render.
//!
//! Lessons cite their sources with footnote references, `[^n]`, and never define the
//! footnotes: `render_lesson` turns each reference to a known citation into a small
//! citation button (`button.cite[data-cite=n]`) the app opens the source viewer from.

use std::collections::HashMap;

use margin_shared::Citation;
use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd, TextMergeStream, html};

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
    sanitize(&raw, false)
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

/// Render a lesson body to sanitized HTML, with each `[^n]` that names one of
/// `citations` as a citation button. References to unknown numbers stay plain text, and
/// so do references inside code, links and image descriptions.
pub fn render_lesson(markdown: &str, citations: &[Citation]) -> String {
    let known: HashMap<u32, &Citation> = citations.iter().map(|c| (c.n, c)).collect();
    let mut events: Vec<Event> = Vec::new();
    // Inside code blocks, links and images, text is left alone.
    let mut verbatim = 0usize;
    for event in TextMergeStream::new(Parser::new_ext(markdown, options())).map(demote_heading) {
        match &event {
            Event::Start(Tag::CodeBlock(_) | Tag::Link { .. } | Tag::Image { .. }) => verbatim += 1,
            Event::End(TagEnd::CodeBlock | TagEnd::Link | TagEnd::Image) => {
                verbatim = verbatim.saturating_sub(1)
            }
            _ => {}
        }
        match event {
            Event::Text(text) if verbatim == 0 => {
                let mut rest: &str = &text;
                while let Some((start, end, n)) = next_marker(rest) {
                    let Some(citation) = known.get(&n) else {
                        events.push(Event::Text(CowStr::from(rest[..end].to_string())));
                        rest = &rest[end..];
                        continue;
                    };
                    if start > 0 {
                        events.push(Event::Text(CowStr::from(rest[..start].to_string())));
                    }
                    events.push(Event::InlineHtml(CowStr::from(marker_html(citation))));
                    rest = &rest[end..];
                }
                if !rest.is_empty() {
                    events.push(Event::Text(CowStr::from(rest.to_string())));
                }
            }
            // Only reached if a body ever defines its footnotes.
            Event::FootnoteReference(label) => {
                match label.parse::<u32>().ok().and_then(|n| known.get(&n)) {
                    Some(citation) => {
                        events.push(Event::InlineHtml(CowStr::from(marker_html(citation))))
                    }
                    None => events.push(Event::Text(CowStr::from(format!("[^{label}]")))),
                }
            }
            other => events.push(other),
        }
    }
    let mut raw = String::with_capacity(markdown.len() * 3 / 2);
    html::push_html(&mut raw, events.into_iter());
    sanitize(&raw, true)
}

/// The first `[^n]` in `text`: its byte range and number.
fn next_marker(text: &str) -> Option<(usize, usize, u32)> {
    let mut from = 0;
    while let Some(i) = text[from..].find("[^") {
        let start = from + i;
        let digits = &text[start + 2..];
        let len = digits.bytes().take_while(u8::is_ascii_digit).count();
        if (1..=6).contains(&len)
            && digits[len..].starts_with(']')
            && let Ok(n) = digits[..len].parse()
        {
            return Some((start, start + 2 + len + 1, n));
        }
        from = start + 2;
    }
    None
}

/// The citation button for `[^n]`: the number, named "Source n: <name>" for assistive
/// technology, with the source and its place as the tooltip.
fn marker_html(c: &Citation) -> String {
    let label = crate::citations::marker_label(c.n, &c.source_name);
    let tip = match crate::citations::locator(c) {
        Some(at) => format!("{} · {at}", c.source_name),
        None => c.source_name.clone(),
    };
    format!(
        r#"<button type="button" class="cite" data-cite="{n}" aria-label="{label}" title="{tip}">{n}</button>"#,
        n = c.n,
        label = escape_attr(&label),
        tip = escape_attr(&tip),
    )
}

fn escape_attr(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Sanitize rendered HTML. With `citations`, citation buttons are kept too: `button`
/// with `class="cite"`, `type="button"`, a numeric `data-cite`, an `aria-label` and a
/// `title`, and nothing else.
fn sanitize(raw: &str, citations: bool) -> String {
    let mut builder = ammonia::Builder::default();
    builder
        .add_generic_attributes(&["class"])
        .add_tag_attributes("details", &["open"])
        .add_tag_attributes("input", &["type", "checked", "disabled"])
        .add_tags(&["input"]);
    if citations {
        builder
            .add_tags(&["button"])
            .add_tag_attributes("button", &["type", "data-cite", "aria-label", "title"])
            .attribute_filter(|element, attribute, value| {
                if element != "button" {
                    return Some(value.into());
                }
                let ok = match attribute {
                    "type" => value == "button",
                    "class" => value == "cite",
                    "data-cite" => !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
                    _ => true,
                };
                ok.then(|| value.into())
            });
    }
    builder.clean(raw).to_string()
}

/// The visible text of a Markdown document (what a reader can select), with
/// whitespace collapsed. Math is included as its TeX source.
pub fn plain_text(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    for event in TextMergeStream::new(Parser::new_ext(markdown, options())) {
        match event {
            Event::Text(t) => out.push_str(&strip_markers(&t)),
            Event::Code(t) | Event::InlineMath(t) | Event::DisplayMath(t) => out.push_str(&t),
            Event::InlineHtml(t) | Event::Html(t) => out.push_str(&strip_tags(&t)),
            Event::SoftBreak | Event::HardBreak => out.push(' '),
            Event::End(TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item)
            | Event::Start(Tag::Paragraph) => out.push(' '),
            _ => {}
        }
    }
    normalize(&out)
}

/// `text` without its citation markers (`[^n]`).
fn strip_markers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((start, end, _)) = next_marker(rest) {
        out.push_str(&rest[..start]);
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
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

    fn citation(n: u32, name: &str) -> Citation {
        Citation {
            n,
            source_id: format!("s{n}"),
            source_name: name.into(),
            kind: margin_shared::SourceKind::Pdf,
            page: Some(3),
            start_ms: None,
            end_ms: None,
            excerpt: "passage".into(),
            image_url: None,
            document_id: None,
            start_line: None,
            end_line: None,
            file: None,
            section_path: None,
        }
    }

    #[test]
    fn citation_markers_become_buttons() {
        let cites = [
            citation(1, "Oppenheim \"ch3\".pdf"),
            citation(2, "talk.m4a"),
        ];
        let html = render_lesson(
            "The period sets it[^1]. Both agree[^2][^1], but [^7] is unknown.\n",
            &cites,
        );
        assert!(
            html.contains(
                r#"sets it<button type="button" class="cite" data-cite="1" aria-label="Source 1: Oppenheim &quot;ch3&quot;.pdf" title="Oppenheim &quot;ch3&quot;.pdf · p. 3">1</button>."#
            ),
            "{html}"
        );
        assert!(
            html.contains(r#"data-cite="2" aria-label="Source 2: talk.m4a""#),
            "{html}"
        );
        assert_eq!(html.matches("<button").count(), 3, "{html}");
        assert!(html.contains("but [^7] is unknown."), "{html}");
    }

    #[test]
    fn citation_markers_in_code_and_links_stay_text() {
        let cites = [citation(1, "a.pdf")];
        let html = render_lesson(
            "Inline `x[^1]` and [a link[^1]](https://example.com).\n\n```\nlet a = b[^1];\n```\n\n**Bold[^1]**\n",
            &cites,
        );
        assert!(html.contains("<code>x[^1]</code>"), "{html}");
        assert!(html.contains("a link[^1]</a>"), "{html}");
        assert!(html.contains("let a = b[^1];"), "{html}");
        assert!(
            html.contains(r#"<strong>Bold<button type="button" class="cite" data-cite="1""#),
            "{html}"
        );
        // Notes don't cite: there the marker is text.
        assert!(render("Bold[^1]").contains("Bold[^1]"));
    }

    #[test]
    fn lesson_html_cant_forge_other_buttons() {
        let html = render_lesson(
            "<button class=\"cite\" data-cite=\"x\" onclick=\"alert(1)\" type=\"submit\">1</button>\n\n<button class=\"evil\" data-cite=\"2\">2</button>\n",
            &[citation(1, "a.pdf")],
        );
        assert!(!html.contains("onclick"), "{html}");
        assert!(!html.contains("submit"), "{html}");
        assert!(!html.contains("data-cite=\"x\""), "{html}");
        assert!(!html.contains("evil"), "{html}");
        // Notes keep no buttons at all.
        assert!(!render("<button>x</button>").contains("<button"));
    }

    #[test]
    fn plain_text_leaves_out_citation_markers() {
        assert_eq!(
            plain_text("The period[^1] sets it[^12]. Keep `[^3]` and [^x]."),
            "The period sets it. Keep [^3] and [^x]."
        );
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
