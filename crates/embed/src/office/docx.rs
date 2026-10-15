//! Word documents: the body of `word/document.xml` (`w:p` paragraphs of `w:t` runs,
//! `w:tbl` tables), with `Heading1`–`Heading6` (or an outline level) as `#` headings so
//! the chunker cuts at them, and numbered or bulleted paragraphs as `- ` items.

use super::{Blocks, Node, Package, Tables, attr, collapse, inline, walk};
use crate::{EmbedError, Extracted, Page, TextFormat};

#[derive(Default)]
struct Para {
    text: String,
    heading: Option<usize>,
    item: Option<usize>,
}

pub(super) fn read(package: &mut Package) -> Result<Extracted, EmbedError> {
    let main = package.main_part("word/document.xml")?;
    let xml = package
        .part(&main)?
        .ok_or_else(|| super::extract_error("the document has no body (word/document.xml)"))?;
    let text = body_text(&xml, &main)?;
    Ok(Extracted {
        pages: if text.is_empty() {
            Vec::new()
        } else {
            vec![Page { number: None, text }]
        },
        format: TextFormat::Markdown,
        ..Default::default()
    })
}

/// A paragraph style's heading level: `Title` 1, `Heading1`–`Heading9` 1–6.
fn style_level(style: &str) -> Option<usize> {
    let s = style.to_ascii_lowercase().replace([' ', '-', '_'], "");
    if s == "title" {
        return Some(1);
    }
    let n: usize = s.strip_prefix("heading")?.parse().ok()?;
    (n >= 1).then_some(n.min(6))
}

fn body_text(xml: &[u8], part: &str) -> Result<String, EmbedError> {
    let mut blocks = Blocks::default();
    let mut tables = Tables::default();
    let mut paras: Vec<Para> = Vec::new();
    let mut in_t = false;
    let mut in_ppr = false;
    walk(xml, part, |node| match node {
        Node::Start(e) | Node::Empty(e) => {
            let empty = matches!(node, Node::Empty(_));
            let para = paras.last_mut();
            match e.local_name().as_ref() {
                b"p" if !empty => paras.push(Para::default()),
                b"pPr" if !empty => in_ppr = true,
                b"pStyle" => {
                    if let (Some(p), Some(level)) =
                        (para, attr(e, b"val").as_deref().and_then(style_level))
                    {
                        p.heading = Some(level);
                    }
                }
                b"outlineLvl" => {
                    if let Some(p) = para
                        && p.heading.is_none()
                        && let Some(n) = attr(e, b"val").and_then(|v| v.parse::<usize>().ok())
                        && n < 6
                    {
                        p.heading = Some(n + 1);
                    }
                }
                b"numPr" if in_ppr => {
                    if let Some(p) = para {
                        p.item.get_or_insert(0);
                    }
                }
                b"ilvl" if in_ppr => {
                    if let Some(p) = para {
                        p.item = Some(attr(e, b"val").and_then(|v| v.parse().ok()).unwrap_or(0));
                    }
                }
                b"numId" if in_ppr => {
                    if let Some(p) = para
                        && attr(e, b"val").as_deref() == Some("0")
                    {
                        p.item = None;
                    }
                }
                b"t" if !empty => in_t = true,
                b"tab" | b"br" | b"cr" if !in_ppr => {
                    if let Some(p) = para {
                        p.text.push(' ');
                    }
                }
                b"tbl" if !empty => tables.start_table(),
                b"tr" if !empty => tables.start_row(),
                _ => {}
            }
        }
        Node::End(name) => match name {
            b"t" => in_t = false,
            b"pPr" => in_ppr = false,
            b"p" => {
                let Some(p) = paras.pop() else { return };
                if tables.open() {
                    tables.push_text(&p.text);
                } else if let Some(level) = p.heading {
                    let title = collapse(&p.text);
                    if !title.is_empty() {
                        blocks.push(&format!("{} {title}", "#".repeat(level)), false);
                    }
                } else if let Some(level) = p.item {
                    let text = collapse(&p.text);
                    if !text.is_empty() {
                        blocks.push(&format!("{}- {text}", "  ".repeat(level.min(8))), true);
                    }
                } else {
                    blocks.push(&inline(&p.text), false);
                }
            }
            b"tc" => tables.end_cell(),
            b"tr" => tables.end_row(),
            b"tbl" => {
                if let Some(table) = tables.end_table() {
                    blocks.push(&table, false);
                }
            }
            _ => {}
        },
        Node::Text(t) => {
            if in_t && let Some(p) = paras.last_mut() {
                p.text.push_str(t);
            }
        }
    })?;
    Ok(blocks.into_text())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(body: &str) -> String {
        let xml = format!(
            r#"<?xml version="1.0"?><w:document xmlns:w="w"><w:body>{body}</w:body></w:document>"#
        );
        body_text(xml.as_bytes(), "word/document.xml").unwrap()
    }

    fn p(style: &str, text: &str) -> String {
        let ppr = if style.is_empty() {
            String::new()
        } else {
            format!(r#"<w:pPr><w:pStyle w:val="{style}"/></w:pPr>"#)
        };
        format!("<w:p>{ppr}<w:r><w:t>{text}</w:t></w:r></w:p>")
    }

    #[test]
    fn headings_paragraphs_lists_and_tables() {
        let item = |lvl: u8, text: &str| {
            format!(
                r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="{lvl}"/><w:numId w:val="3"/></w:numPr><w:tabs><w:tab w:val="left"/></w:tabs></w:pPr><w:r><w:t>{text}</w:t></w:r></w:p>"#
            )
        };
        let body = [
            p("Title", "Plan"),
            p("Heading2", "Goals"),
            r#"<w:p><w:r><w:t xml:space="preserve">Ship </w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>fast</w:t></w:r><w:r><w:delText>slow</w:delText></w:r><w:r><w:tab/><w:t>&amp; well.</w:t></w:r></w:p>"#.to_string(),
            item(0, "one"),
            item(1, "two"),
            p("", "# not a heading"),
            "<w:tbl><w:tr><w:tc><w:p><w:r><w:t>a</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>b|c</w:t></w:r></w:p><w:p><w:r><w:t>d</w:t></w:r></w:p></w:tc></w:tr><w:tr><w:tc><w:p/></w:tc><w:tc><w:p/></w:tc></w:tr><w:tr><w:tc><w:tbl><w:tr><w:tc><w:p><w:r><w:t>in</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:tc><w:tc><w:p><w:r><w:t>2</w:t></w:r></w:p></w:tc></w:tr></w:tbl>".to_string(),
            p("Heading 3", "Risks"),
        ]
        .concat();
        assert_eq!(
            doc(&body),
            "# Plan\n\n## Goals\n\nShip fast & well.\n\n- one\n  - two\n\n\\# not a heading\n\n| a | b\\|c d |\n| in | 2 |\n\n### Risks"
        );
    }

    #[test]
    fn styles_and_empty_documents() {
        assert_eq!(style_level("Heading1"), Some(1));
        assert_eq!(style_level("heading 9"), Some(6));
        assert_eq!(style_level("Heading0"), None);
        assert_eq!(style_level("Normal"), None);
        assert_eq!(doc(""), "");
        assert_eq!(doc("<w:p/><w:p><w:r><w:t> </w:t></w:r></w:p>"), "");
    }
}
