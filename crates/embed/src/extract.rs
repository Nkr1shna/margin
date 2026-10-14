//! Reading source files. Text and code are decoded as UTF-8 (lossy) with newlines
//! normalized; Jupyter notebooks become their cells' sources and HTML loses its markup.
//! Notebooks and HTML are recognized by extension or, for files stored without one, by
//! their first bytes. PDFs are read in `pdf.rs`, images with ImageIO (`image.rs`), audio
//! and video with AVFoundation (`av.rs`).

use std::path::Path;

use crate::{EmbedError, Extracted, Page, SourceKind};

pub(crate) fn extract(
    path: &Path,
    kind: SourceKind,
    media_dir: &Path,
) -> Result<Extracted, EmbedError> {
    match kind {
        SourceKind::Pdf => pdf(path, media_dir),
        SourceKind::Image => image(path),
        SourceKind::Audio => audio(path),
        SourceKind::Video => video(path, media_dir),
        SourceKind::Text | SourceKind::Code => {
            let bytes = std::fs::read(path).map_err(|e| EmbedError::Extract(e.to_string()))?;
            let text = normalize_newlines(&String::from_utf8_lossy(&bytes));
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            let text = if ext == "ipynb" || (ext.is_empty() && looks_like_notebook(&text)) {
                notebook(&text).unwrap_or(text)
            } else if matches!(ext.as_str(), "html" | "htm")
                || (ext.is_empty() && looks_like_html(&text))
            {
                strip_html(&text)
            } else {
                text
            };
            Ok(Extracted {
                pages: vec![Page { number: None, text }],
                ..Default::default()
            })
        }
    }
}

#[cfg(target_os = "macos")]
fn pdf(path: &Path, media_dir: &Path) -> Result<Extracted, EmbedError> {
    crate::pdf::extract(path, Some(media_dir))
}

/// The image itself is the picture; decoding it now finds a broken file early.
#[cfg(target_os = "macos")]
fn image(path: &Path) -> Result<Extracted, EmbedError> {
    let image = crate::image::load(path, Some(64))?;
    let (width, height) = crate::image::size(&image);
    if width == 0 || height == 0 {
        return Err(EmbedError::Extract("the image is empty".into()));
    }
    Ok(Extracted {
        pictures: vec![crate::Picture {
            path: path.to_path_buf(),
            page: None,
            at_ms: None,
            tokens: crate::ImageTokens::High,
        }],
        ..Default::default()
    })
}

#[cfg(target_os = "macos")]
fn audio(path: &Path) -> Result<Extracted, EmbedError> {
    let Some(samples) = crate::av::sound(path)? else {
        return Err(EmbedError::Extract("there's no sound in this file".into()));
    };
    if samples.is_empty() {
        return Err(EmbedError::Extract("there's no sound in this file".into()));
    }
    Ok(Extracted {
        duration_ms: Some(crate::sample_ms(samples.len())),
        sound: Some(samples),
        ..Default::default()
    })
}

/// Frames at `frame_times` as JPEGs in `media_dir`, plus the soundtrack if there is one.
#[cfg(target_os = "macos")]
fn video(path: &Path, media_dir: &Path) -> Result<Extracted, EmbedError> {
    if !crate::av::has_video(path)? {
        return Err(EmbedError::Extract(
            "there's no picture in this video".into(),
        ));
    }
    let duration = crate::av::duration_ms(path)?;
    let (_, times) = crate::frame_times(duration);
    std::fs::create_dir_all(media_dir)?;
    let mut pictures = Vec::new();
    crate::av::frames(path, &times, &mut |ms, image| {
        let file = media_dir.join(crate::frame_file(ms));
        crate::image::write(image, &file, crate::image::Format::Jpeg(0.8))?;
        pictures.push(crate::Picture {
            path: file,
            page: None,
            at_ms: Some(ms),
            tokens: crate::ImageTokens::Medium,
        });
        Ok(())
    })?;
    if pictures.is_empty() {
        return Err(EmbedError::Extract(
            "couldn't take any pictures from this video".into(),
        ));
    }
    let sound = crate::av::sound(path)?.filter(|s| !s.is_empty());
    Ok(Extracted {
        pictures,
        sound,
        duration_ms: Some(duration),
        ..Default::default()
    })
}

#[cfg(not(target_os = "macos"))]
fn pdf(_path: &Path, _media_dir: &Path) -> Result<Extracted, EmbedError> {
    Err(EmbedError::Extract("reading PDFs needs macOS".into()))
}

#[cfg(not(target_os = "macos"))]
fn image(_path: &Path) -> Result<Extracted, EmbedError> {
    Err(EmbedError::Extract("reading images needs macOS".into()))
}

#[cfg(not(target_os = "macos"))]
fn audio(_path: &Path) -> Result<Extracted, EmbedError> {
    Err(EmbedError::Extract("reading audio needs macOS".into()))
}

#[cfg(not(target_os = "macos"))]
fn video(_path: &Path, _media_dir: &Path) -> Result<Extracted, EmbedError> {
    Err(EmbedError::Extract("reading video needs macOS".into()))
}

fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn looks_like_notebook(text: &str) -> bool {
    text.trim_start().starts_with('{')
        && text.contains("\"cells\"")
        && text.contains("\"nbformat\"")
}

fn looks_like_html(text: &str) -> bool {
    let start: String = text
        .trim_start()
        .chars()
        .take(15)
        .collect::<String>()
        .to_ascii_lowercase();
    start.starts_with("<!doctype html") || start.starts_with("<html")
}

/// The sources of a notebook's cells (code and markdown), one paragraph each; outputs
/// are left out. `None` when it isn't a notebook after all.
fn notebook(text: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(text).ok()?;
    // nbformat 4 keeps cells at the top; 3 inside worksheets, with code under "input".
    let cells: Vec<&serde_json::Value> = match json.get("cells") {
        Some(cells) => cells.as_array()?.iter().collect(),
        None => json
            .get("worksheets")?
            .as_array()?
            .iter()
            .filter_map(|w| w.get("cells")?.as_array())
            .flatten()
            .collect(),
    };
    let source = |cell: &serde_json::Value| -> Option<String> {
        let value = cell.get("source").or_else(|| cell.get("input"))?;
        match value {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Array(lines) => {
                Some(lines.iter().filter_map(|l| l.as_str()).collect::<String>())
            }
            _ => None,
        }
    };
    let parts: Vec<String> = cells
        .into_iter()
        .filter_map(source)
        .map(|s| normalize_newlines(s.trim_end()))
        .filter(|s| !s.trim().is_empty())
        .collect();
    Some(parts.join("\n\n"))
}

/// Crude HTML to text: drops comments, `<script>`/`<style>` and every tag, turns block
/// tags into line breaks and decodes common entities.
fn strip_html(html: &str) -> String {
    const BLOCKS: &[&str] = &[
        "p",
        "div",
        "br",
        "li",
        "ul",
        "ol",
        "tr",
        "td",
        "th",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "pre",
        "blockquote",
        "section",
        "article",
        "header",
        "footer",
        "table",
        "hr",
    ];
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while i < html.len() {
        let rest = &html[i..];
        if lower[i..].starts_with("<!--") {
            i += lower[i..].find("-->").map_or(rest.len(), |e| e + 3);
            continue;
        }
        if rest.starts_with('<') {
            let end = rest.find('>').map_or(rest.len(), |e| e + 1);
            let tag = lower[i + 1..i + end]
                .trim_start_matches('/')
                .split(|c: char| c.is_whitespace() || c == '>' || c == '/')
                .next()
                .unwrap_or("")
                .to_string();
            i += end;
            if (tag == "script" || tag == "style") && !lower[i - end..i].starts_with("</") {
                let close = format!("</{tag}");
                i += lower[i..].find(&close).unwrap_or(html.len() - i);
                continue;
            }
            if BLOCKS.contains(&tag.as_str()) {
                out.push('\n');
            }
            continue;
        }
        let next = rest.find('<').unwrap_or(rest.len());
        out.push_str(&rest[..next]);
        i += next;
    }
    let text = decode_entities(&out);
    // Collapse runs of spaces and of blank lines left by the markup.
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() && lines.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        lines.push(line);
    }
    lines.join("\n").trim().to_string()
}

fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let entity = rest[1..]
            .find(';')
            .filter(|&e| e <= 10)
            .map(|e| &rest[1..e + 1]);
        let decoded = entity.and_then(|name| match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => {
                let number = name.strip_prefix('#')?;
                let code = match number.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                    None => number.parse().ok()?,
                };
                char::from_u32(code)
            }
        });
        match (entity, decoded) {
            (Some(name), Some(c)) => {
                out.push(c);
                rest = &rest[name.len() + 2..];
            }
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("margin-embed-extract-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn text_of(path: &Path, kind: SourceKind) -> String {
        let doc = extract(path, kind, Path::new("/nonexistent")).unwrap();
        assert_eq!(doc.pages.len(), 1);
        assert_eq!(doc.pages[0].number, None);
        doc.pages[0].text.clone()
    }

    #[test]
    fn text_is_decoded_lossily_with_unix_newlines() {
        let path = write("notes.txt", b"caf\xc3\xa9\r\nline two\rline three\xff\n");
        assert_eq!(
            text_of(&path, SourceKind::Text),
            "café\nline two\nline three\u{fffd}\n"
        );
    }

    #[test]
    fn code_is_kept_as_is() {
        let code = "fn main() {\n    println!(\"<b>hi</b>\");\n}\n";
        let path = write("main.rs", code.as_bytes());
        assert_eq!(text_of(&path, SourceKind::Code), code);
    }

    #[test]
    fn notebooks_become_their_cell_sources() {
        let nb = r##"{"cells": [
            {"cell_type": "markdown", "source": ["# Title\n", "Some text."]},
            {"cell_type": "code", "source": "import numpy as np\nnp.zeros(3)",
             "outputs": [{"text": "array([0., 0., 0.])"}]},
            {"cell_type": "code", "source": []}
        ], "nbformat": 4, "metadata": {}}"##;
        let expected = "# Title\nSome text.\n\nimport numpy as np\nnp.zeros(3)";
        assert_eq!(
            text_of(&write("a.ipynb", nb.as_bytes()), SourceKind::Code),
            expected
        );
        // Stored without an extension, it is recognized by its contents.
        assert_eq!(
            text_of(&write("nb-no-ext", nb.as_bytes()), SourceKind::Code),
            expected
        );
    }

    #[test]
    fn html_loses_its_markup() {
        let html = "<!DOCTYPE html><html><head><title>T</title><style>p { color: red }</style>\
            <script>var x = \"<p>\";</script></head><body><!-- hidden -->\
            <h1>Heading</h1><p>Fish &amp; chips &lt;3 &#233;t&#xE9;&#x21; &bogus; a&b</p>\
            <ul><li>one</li><li>two</li></ul></body></html>";
        let expected = "T\nHeading\n\nFish & chips <3 été! &bogus; a&b\n\none\n\ntwo";
        assert_eq!(
            text_of(&write("page.html", html.as_bytes()), SourceKind::Text),
            expected
        );
        assert_eq!(
            text_of(&write("page-no-ext", html.as_bytes()), SourceKind::Text),
            expected
        );
    }

    #[test]
    fn a_missing_file_is_an_extract_error() {
        let err = extract(
            Path::new("/nonexistent/margin.txt"),
            SourceKind::Text,
            Path::new("/nonexistent"),
        )
        .unwrap_err();
        assert!(matches!(err, EmbedError::Extract(_)));
    }
}
