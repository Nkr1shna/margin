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

/// Render Markdown to sanitized HTML.
pub fn render(markdown: &str) -> String {
    let parser = Parser::new_ext(markdown, options());
    let mut raw = String::with_capacity(markdown.len() * 3 / 2);
    html::push_html(&mut raw, parser);
    sanitize(&raw)
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

/// Convert SubRip captions to WebVTT, which is what `<track>` accepts.
pub fn srt_to_vtt(srt: &str) -> String {
    let mut out = String::from("WEBVTT\n\n");
    let text = srt.replace("\r\n", "\n");
    for line in text.lines() {
        if line.contains("-->") {
            out.push_str(&line.replace(',', "."));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// A `data:` URL for text content (percent-encoded), used for the captions track so
/// the webview needs no cross-origin fetch.
pub fn text_data_url(mime: &str, text: &str) -> String {
    let mut out = format!("data:{mime};charset=utf-8,");
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~ ".contains(&b) {
            if b == b' ' {
                out.push_str("%20");
            } else {
                out.push(b as char);
            }
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_basic_markdown() {
        let html = render("# Title\n\nSome *emphasis* and `code`.\n\n- a\n- b\n");
        assert!(html.contains("<h1>Title</h1>"));
        assert!(html.contains("<em>emphasis</em>"));
        assert!(html.contains("<code>code</code>"));
        assert!(html.contains("<li>a</li>"));
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

    #[test]
    fn converts_srt_to_vtt() {
        let srt = "1\r\n00:00:01,000 --> 00:00:02,500\r\nHello, world\r\n\r\n";
        let vtt = srt_to_vtt(srt);
        assert!(vtt.starts_with("WEBVTT\n\n1\n00:00:01.000 --> 00:00:02.500\nHello, world\n"));
    }

    #[test]
    fn data_url_is_percent_encoded() {
        let url = text_data_url("text/vtt", "WEBVTT\n\na, b");
        assert_eq!(url, "data:text/vtt;charset=utf-8,WEBVTT%0A%0Aa%2C%20b");
    }
}
