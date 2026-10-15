//! Presentations: one page per slide, in `ppt/presentation.xml`'s order, numbered from 1
//! so a citation says "slide 3". Each shape's `a:p` paragraphs are lines (a title
//! placeholder is a `#` heading), tables are `| a | b |` rows, and the slide's speaker
//! notes follow a `Notes:` line.

use super::{Blocks, Node, Package, Tables, attr, collapse, inline, walk};
use crate::{EmbedError, Extracted, Page, TextFormat};

pub(super) fn read(package: &mut Package) -> Result<Extracted, EmbedError> {
    let main = package.main_part("ppt/presentation.xml")?;
    let xml = package
        .part(&main)?
        .ok_or_else(|| super::extract_error("the presentation has no ppt/presentation.xml"))?;
    let mut ids = Vec::new();
    walk(&xml, &main, |node| {
        if let Node::Start(e) | Node::Empty(e) = node
            && e.local_name().as_ref() == b"sldId"
            && let Some(id) = e
                .attributes()
                .with_checks(false)
                .flatten()
                .find(|a| a.key.prefix().is_some() && a.key.local_name().as_ref() == b"id")
        {
            ids.push(String::from_utf8_lossy(&id.value).into_owned());
        }
    })?;
    let rels = package.rels(&main)?;
    let mut pages = Vec::new();
    for (n, id) in ids.iter().enumerate() {
        let Some(slide) = rels.iter().find(|r| &r.id == id).map(|r| r.target.clone()) else {
            continue;
        };
        let Some(xml) = package.part(&slide)? else {
            continue;
        };
        let mut text = shapes_text(&xml, &slide, false)?;
        let notes = package
            .rels(&slide)?
            .into_iter()
            .find(|r| r.typ.ends_with("/notesSlide"));
        if let Some(notes) = notes
            && let Some(xml) = package.part(&notes.target)?
        {
            let notes = shapes_text(&xml, &notes.target, true)?;
            if !notes.is_empty() {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str("Notes:\n\n");
                text.push_str(&notes);
            }
        }
        pages.push(Page {
            number: Some(n as u32 + 1),
            text,
        });
    }
    Ok(Extracted {
        pages,
        format: TextFormat::Markdown,
        ..Default::default()
    })
}

#[derive(Default)]
struct Shape {
    title: bool,
    /// A notes page's slide image, number, header, footer or date: not the notes.
    skip: bool,
    lines: Vec<String>,
}

/// The text of a slide's (or notes page's) shapes and tables.
fn shapes_text(xml: &[u8], part: &str, notes: bool) -> Result<String, EmbedError> {
    let mut blocks = Blocks::default();
    let mut tables = Tables::default();
    let mut shapes: Vec<Shape> = Vec::new();
    let mut para: Option<String> = None;
    let mut in_t = false;
    walk(xml, part, |node| match node {
        Node::Start(e) | Node::Empty(e) => {
            let empty = matches!(node, Node::Empty(_));
            match e.local_name().as_ref() {
                b"sp" if !empty => shapes.push(Shape::default()),
                b"ph" => {
                    if let Some(shape) = shapes.last_mut() {
                        let typ = attr(e, b"type").unwrap_or_default();
                        shape.title = matches!(typ.as_str(), "title" | "ctrTitle");
                        shape.skip = notes
                            && matches!(typ.as_str(), "sldImg" | "sldNum" | "hdr" | "ftr" | "dt");
                    }
                }
                b"p" if !empty => para = Some(String::new()),
                b"t" if !empty => in_t = true,
                b"br" => {
                    if let Some(p) = para.as_mut() {
                        p.push(' ');
                    }
                }
                b"tbl" if !empty => tables.start_table(),
                b"tr" if !empty => tables.start_row(),
                _ => {}
            }
        }
        Node::End(name) => match name {
            b"t" => in_t = false,
            b"p" => {
                let Some(text) = para.take() else { return };
                if tables.open() {
                    tables.push_text(&text);
                } else if let Some(shape) = shapes.last_mut() {
                    shape.lines.push(text);
                } else {
                    blocks.push(&inline(&text), false);
                }
            }
            b"sp" => {
                let Some(shape) = shapes.pop() else { return };
                if shape.skip {
                    return;
                }
                if shape.title {
                    let title = collapse(&shape.lines.join(" "));
                    if !title.is_empty() {
                        blocks.push(&format!("# {title}"), false);
                    }
                } else {
                    let lines: Vec<String> = shape
                        .lines
                        .iter()
                        .map(|l| inline(l))
                        .filter(|l| !l.is_empty())
                        .collect();
                    blocks.push(&lines.join("\n"), false);
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
            if in_t && let Some(p) = para.as_mut() {
                p.push_str(t);
            }
        }
    })?;
    Ok(blocks.into_text())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_titles_tables_and_notes() {
        let xml = r#"<p:sld xmlns:p="p" xmlns:a="a"><p:cSld><p:spTree>
            <p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>Q3 </a:t></a:r><a:r><a:t>review</a:t></a:r></a:p></p:txBody></p:sp>
            <p:grpSp><p:sp><p:txBody><a:p><a:r><a:t>Revenue up</a:t></a:r><a:br/><a:r><a:t>a lot</a:t></a:r></a:p><a:p/><a:p><a:pPr lvl="1"/><a:r><a:t># costs</a:t></a:r></a:p></p:txBody></p:sp></p:grpSp>
            <p:graphicFrame><a:graphic><a:graphicData><a:tbl><a:tr><a:tc><a:txBody><a:p><a:r><a:t>EU</a:t></a:r></a:p></a:txBody></a:tc><a:tc><a:txBody><a:p><a:r><a:t>12</a:t></a:r></a:p></a:txBody></a:tc></a:tr></a:tbl></a:graphicData></a:graphic></p:graphicFrame>
            </p:spTree></p:cSld></p:sld>"#;
        assert_eq!(
            shapes_text(xml.as_bytes(), "s", false).unwrap(),
            "# Q3 review\n\nRevenue up a lot\n\\# costs\n\n| EU | 12 |"
        );
        let notes = r#"<p:notes xmlns:p="p" xmlns:a="a"><p:cSld><p:spTree>
            <p:sp><p:nvSpPr><p:nvPr><p:ph type="sldImg"/></p:nvPr></p:nvSpPr></p:sp>
            <p:sp><p:nvSpPr><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>Say the number.</a:t></a:r></a:p></p:txBody></p:sp>
            <p:sp><p:nvSpPr><p:nvPr><p:ph type="sldNum"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>3</a:t></a:r></a:p></p:txBody></p:sp>
            </p:spTree></p:cSld></p:notes>"#;
        assert_eq!(
            shapes_text(notes.as_bytes(), "n", true).unwrap(),
            "Say the number."
        );
    }
}
