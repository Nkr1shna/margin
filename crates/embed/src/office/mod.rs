//! Office files: `.docx`, `.pptx` and `.xlsx`, read by small pure-Rust readers
//! (`docs/basics-brief.md` D6). A Word document is one page of Markdown-shaped text
//! (headings as `#` lines, list items as `- ` lines, tables as `| a | b |` rows), a
//! presentation is one page per slide (`Page::number` is the slide number, its speaker
//! notes after a `Notes:` line), and a workbook is one `#` section per sheet with a line
//! of tab-separated cells per row. Pictures inside the files are not read.
//!
//! The files are zips of XML parts and come from anyone, so every part is read through a
//! cap (`OFFICE_PART_MAX_BYTES` each, `OFFICE_TOTAL_MAX_BYTES` together, measured while
//! inflating, not trusted from the zip's headers) and the reader never panics on a bad
//! file: each failure is an `EmbedError::Extract` saying what is wrong with it.

mod docx;
mod pptx;
mod xlsx;

use std::borrow::Cow;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use quick_xml::events::{BytesStart, Event};

use crate::{EmbedError, Extracted};

/// The most bytes one part of an Office file may inflate to.
pub const OFFICE_PART_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// The most bytes all the parts read from one file may inflate to.
pub const OFFICE_TOTAL_MAX_BYTES: u64 = 512 * 1024 * 1024;
/// The most entries an Office file's zip may hold.
pub const OFFICE_ENTRIES_MAX: usize = 50_000;
/// The most rows read from one sheet; past it the sheet ends with "… more rows".
pub const OFFICE_SHEET_ROWS: usize = 20_000;

/// The three formats the readers know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfficeFormat {
    Docx,
    Pptx,
    Xlsx,
}

impl OfficeFormat {
    /// The format a file name's extension says.
    pub fn from_name(name: &str) -> Option<OfficeFormat> {
        let ext = Path::new(name)
            .extension()?
            .to_string_lossy()
            .to_ascii_lowercase();
        match ext.as_str() {
            "docx" | "docm" | "dotx" | "dotm" => Some(OfficeFormat::Docx),
            "pptx" | "pptm" | "potx" | "potm" | "ppsx" | "ppsm" => Some(OfficeFormat::Pptx),
            "xlsx" | "xlsm" | "xltx" | "xltm" => Some(OfficeFormat::Xlsx),
            _ => None,
        }
    }
}

/// The format of the Office file at `path`, from its `[Content_Types].xml`, or `None`
/// when it isn't one (for files stored without an extension).
pub fn detect(path: &Path) -> Option<OfficeFormat> {
    let mut package = Package::open(path).ok()?;
    package.content_type_format().ok().flatten()
}

/// Reads the Office file at `path`. The format comes from the zip's
/// `[Content_Types].xml`, or from the extension when that says nothing.
pub fn extract(path: &Path) -> Result<Extracted, EmbedError> {
    let mut package = Package::open(path)?;
    let format = package
        .content_type_format()?
        .or_else(|| OfficeFormat::from_name(&path.to_string_lossy()))
        .ok_or_else(|| {
            extract_error("it isn't a Word document, presentation or spreadsheet Margin reads")
        })?;
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match format {
        OfficeFormat::Docx => docx::read(&mut package),
        OfficeFormat::Pptx => pptx::read(&mut package),
        OfficeFormat::Xlsx => xlsx::read(path, &mut package),
    }))
    .unwrap_or_else(|_| Err(extract_error("the file is damaged")))
}

fn extract_error(why: impl Into<String>) -> EmbedError {
    EmbedError::Extract(why.into())
}

/// The first bytes of an OLE compound file: an encrypted (password-protected) OOXML file,
/// or a legacy `.doc`/`.xls`/`.ppt`.
const CFB_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

/// Why a compound file can't be read: an encrypted OOXML file keeps its zip in an
/// `EncryptedPackage` stream, whose name (UTF-16) is in the file's directory.
fn compound_file_error(path: &Path) -> EmbedError {
    let marker: Vec<u8> = "EncryptedPackage"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let mut bytes = Vec::new();
    let encrypted = File::open(path)
        .and_then(|f| f.take(OFFICE_PART_MAX_BYTES).read_to_end(&mut bytes))
        .is_ok()
        && bytes.windows(marker.len()).any(|w| w == marker.as_slice());
    if encrypted {
        extract_error(
            "the document is password-protected; save a copy without the password and add that",
        )
    } else {
        extract_error(
            "it is an old Office file (.doc, .xls or .ppt), which Margin doesn't read; \
             save it as .docx, .xlsx or .pptx and add that",
        )
    }
}

/// An Office file's zip, with the inflate budget left for its parts.
pub(crate) struct Package {
    zip: zip::ZipArchive<BufReader<File>>,
    budget: u64,
}

impl Package {
    fn open(path: &Path) -> Result<Package, EmbedError> {
        let mut file = File::open(path).map_err(|e| extract_error(e.to_string()))?;
        let mut magic = [0u8; 8];
        let n = read_up_to(&mut file, &mut magic).map_err(|e| extract_error(e.to_string()))?;
        if n == magic.len() && magic == CFB_MAGIC {
            return Err(compound_file_error(path));
        }
        let file = File::open(path).map_err(|e| extract_error(e.to_string()))?;
        let zip = zip::ZipArchive::new(BufReader::new(file)).map_err(|_| {
            extract_error("the file is damaged or isn't an Office document (it isn't a zip)")
        })?;
        if zip.len() > OFFICE_ENTRIES_MAX {
            return Err(extract_error(format!(
                "the file holds more than {OFFICE_ENTRIES_MAX} parts"
            )));
        }
        Ok(Package {
            zip,
            budget: OFFICE_TOTAL_MAX_BYTES,
        })
    }

    /// The part's name as stored: `name` itself, or the entry that matches it ignoring
    /// case (part names are case-insensitive in OOXML).
    fn entry_name(&self, name: &str) -> Option<String> {
        let name = name.trim_start_matches('/');
        if self.zip.index_for_name(name).is_some() {
            return Some(name.to_string());
        }
        self.zip
            .file_names()
            .find(|n| n.trim_start_matches('/').eq_ignore_ascii_case(name))
            .map(str::to_string)
    }

    /// The part `name`, inflated (at most `OFFICE_PART_MAX_BYTES`), or `None` when the
    /// file has no such part.
    pub(crate) fn part(&mut self, name: &str) -> Result<Option<Vec<u8>>, EmbedError> {
        let Some(entry) = self.entry_name(name) else {
            return Ok(None);
        };
        let cap = OFFICE_PART_MAX_BYTES.min(self.budget);
        let file = self.zip.by_name(&entry).map_err(|e| match e {
            zip::result::ZipError::UnsupportedArchive(_)
            | zip::result::ZipError::InvalidPassword => extract_error(
                "the document is password-protected; save a copy without the password and add that",
            ),
            _ => extract_error(format!("the part {entry} is damaged")),
        })?;
        let mut bytes = Vec::new();
        file.take(cap + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| extract_error(format!("the part {entry} is damaged")))?;
        if bytes.len() as u64 > cap {
            return Err(extract_error(if cap < OFFICE_PART_MAX_BYTES {
                format!(
                    "the file unpacks to more than {} MB",
                    OFFICE_TOTAL_MAX_BYTES >> 20
                )
            } else {
                format!(
                    "the part {entry} unpacks to more than {} MB",
                    OFFICE_PART_MAX_BYTES >> 20
                )
            }));
        }
        self.budget -= bytes.len() as u64;
        Ok(Some(bytes))
    }

    /// Inflates every part through the caps without keeping it: for readers (`calamine`)
    /// that inflate parts themselves.
    fn check_sizes(&mut self) -> Result<(), EmbedError> {
        for i in 0..self.zip.len() {
            let cap = OFFICE_PART_MAX_BYTES.min(self.budget);
            let file = self
                .zip
                .by_index(i)
                .map_err(|_| extract_error("the file is damaged"))?;
            let name = file.name().to_string();
            let n = std::io::copy(&mut file.take(cap + 1), &mut std::io::sink())
                .map_err(|_| extract_error(format!("the part {name} is damaged")))?;
            if n > cap {
                return Err(extract_error(if cap < OFFICE_PART_MAX_BYTES {
                    format!(
                        "the file unpacks to more than {} MB",
                        OFFICE_TOTAL_MAX_BYTES >> 20
                    )
                } else {
                    format!(
                        "the part {name} unpacks to more than {} MB",
                        OFFICE_PART_MAX_BYTES >> 20
                    )
                }));
            }
            self.budget -= n;
        }
        Ok(())
    }

    /// The format `[Content_Types].xml` names for the main part.
    fn content_type_format(&mut self) -> Result<Option<OfficeFormat>, EmbedError> {
        let Some(xml) = self.part("[Content_Types].xml")? else {
            return Ok(None);
        };
        let mut format = None;
        walk(&xml, "[Content_Types].xml", |node| {
            if let Node::Start(e) | Node::Empty(e) = node
                && e.local_name().as_ref() == b"Override"
                && let Some(t) = attr(e, b"ContentType")
            {
                let t = t.to_ascii_lowercase();
                let found = if t.contains("wordprocessingml.document.main")
                    || t.contains("wordprocessingml.template.main")
                    || t.contains("ms-word.document.macroenabled.main")
                    || t.contains("ms-word.template.macroenabledtemplate.main")
                {
                    Some(OfficeFormat::Docx)
                } else if t.contains("presentationml.presentation.main")
                    || t.contains("presentationml.slideshow.main")
                    || t.contains("presentationml.template.main")
                    || t.contains("ms-powerpoint.")
                        && (t.contains("presentation.macroenabled.main")
                            || t.contains("slideshow.macroenabled.main")
                            || t.contains("template.macroenabled.main"))
                {
                    Some(OfficeFormat::Pptx)
                } else if t.contains("spreadsheetml.sheet.main")
                    || t.contains("spreadsheetml.template.main")
                    || t.contains("ms-excel.sheet.macroenabled.main")
                    || t.contains("ms-excel.template.macroenabled.main")
                {
                    Some(OfficeFormat::Xlsx)
                } else {
                    None
                };
                format = format.or(found);
            }
        })?;
        Ok(format)
    }

    /// The relationships of `part` (`dir/_rels/name.rels`).
    pub(crate) fn rels(&mut self, part: &str) -> Result<Vec<Rel>, EmbedError> {
        let (dir, name) = split_part(part);
        let rels_name = format!("{dir}_rels/{name}.rels");
        let Some(xml) = self.part(&rels_name)? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        walk(&xml, &rels_name, |node| {
            if let Node::Start(e) | Node::Empty(e) = node
                && e.local_name().as_ref() == b"Relationship"
                && attr(e, b"TargetMode").is_none_or(|m| m != "External")
                && let (Some(id), Some(typ), Some(target)) =
                    (attr(e, b"Id"), attr(e, b"Type"), attr(e, b"Target"))
            {
                out.push(Rel {
                    id,
                    typ,
                    target: resolve(dir, &target),
                });
            }
        })?;
        Ok(out)
    }

    /// The main part (`word/document.xml`, `ppt/presentation.xml`) the package's
    /// relationships name, or `default`.
    pub(crate) fn main_part(&mut self, default: &str) -> Result<String, EmbedError> {
        Ok(self
            .rels("")?
            .into_iter()
            .find(|r| r.typ.ends_with("/officeDocument"))
            .map_or_else(|| default.to_string(), |r| r.target))
    }
}

/// `read`, but a short read only at the end of the file.
fn read_up_to(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// One relationship: `Target` resolved to a part name.
#[derive(Debug, Clone)]
pub(crate) struct Rel {
    pub(crate) id: String,
    pub(crate) typ: String,
    pub(crate) target: String,
}

/// `("word/", "document.xml")` for `word/document.xml`; `("", "")` for the package.
fn split_part(part: &str) -> (&str, &str) {
    match part.rfind('/') {
        Some(i) => (&part[..=i], &part[i + 1..]),
        None => ("", part),
    }
}

/// A relationship target, relative to the folder `dir` of its source part (or absolute
/// from the package root), as a part name.
fn resolve(dir: &str, target: &str) -> String {
    let joined = match target.strip_prefix('/') {
        Some(abs) => abs.to_string(),
        None => format!("{dir}{target}"),
    };
    let mut parts: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// What `walk` hands its callback.
pub(crate) enum Node<'a, 'b> {
    Start(&'a BytesStart<'b>),
    Empty(&'a BytesStart<'b>),
    End(&'a [u8]),
    Text(&'a str),
}

/// Streams the elements of an XML part to `f` (by local name), with entity and character
/// references resolved into text. Everything inside `mc:Fallback` is skipped: it repeats
/// its `mc:Choice` for older readers.
pub(crate) fn walk(
    xml: &[u8],
    part: &str,
    mut f: impl FnMut(Node<'_, '_>),
) -> Result<(), EmbedError> {
    let mut reader = quick_xml::Reader::from_reader(xml);
    reader.config_mut().check_end_names = false;
    let mut buf = Vec::new();
    let mut fallback = 0usize;
    let damaged = || extract_error(format!("the part {part} is damaged"));
    loop {
        buf.clear();
        let event = reader.read_event_into(&mut buf).map_err(|_| damaged())?;
        match &event {
            Event::Start(e) if e.local_name().as_ref() == b"Fallback" => {
                fallback += 1;
                continue;
            }
            Event::End(e) if fallback > 0 && e.local_name().as_ref() == b"Fallback" => {
                fallback -= 1;
                continue;
            }
            Event::Eof => break,
            _ if fallback > 0 => continue,
            _ => {}
        }
        match &event {
            Event::Start(e) => f(Node::Start(e)),
            Event::Empty(e) => f(Node::Empty(e)),
            Event::End(e) => f(Node::End(e.local_name().as_ref())),
            Event::Text(t) => {
                let text = t.decode().map_err(|_| damaged())?;
                f(Node::Text(&text));
            }
            Event::CData(t) => {
                let text = t.decode().map_err(|_| damaged())?;
                f(Node::Text(&text));
            }
            Event::GeneralRef(r) => {
                let text: Cow<str> = match r.resolve_char_ref() {
                    Ok(Some(c)) => c.to_string().into(),
                    Ok(None) => match r.decode().map_err(|_| damaged())?.as_ref() {
                        "amp" => "&".into(),
                        "lt" => "<".into(),
                        "gt" => ">".into(),
                        "quot" => "\"".into(),
                        "apos" => "'".into(),
                        _ => "".into(),
                    },
                    Err(_) => "".into(),
                };
                f(Node::Text(&text));
            }
            _ => {}
        }
    }
    Ok(())
}

/// The value of the attribute with local name `name` (any prefix), unescaped.
pub(crate) fn attr(e: &BytesStart, name: &[u8]) -> Option<String> {
    e.attributes()
        .with_checks(false)
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name)
        .map(|a| {
            a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map(Cow::into_owned)
                .unwrap_or_else(|_| String::from_utf8_lossy(&a.value).into_owned())
        })
}

/// Text from a file, made safe to put on a line of Markdown: whitespace runs (newlines
/// included) become one space, and a line that the chunker would read as structure (a
/// heading, a fence, a table row, display math or a rule) is escaped with `\`.
pub(crate) fn inline(text: &str) -> String {
    escape_line(collapse(text))
}

/// `text` on one line: whitespace runs (newlines included) become one space.
pub(crate) fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `line` with a `\` before it when the chunker would read it as structure.
pub(crate) fn escape_line(line: String) -> String {
    let t = line.as_str();
    let rule_like = t.len() >= 3 && t.chars().all(|c| matches!(c, '-' | '=' | '*' | '_' | ' '));
    if t.starts_with(['#', '|', '>'])
        || t.starts_with("```")
        || t.starts_with("~~~")
        || t.starts_with("$$")
        || rule_like
    {
        format!("\\{line}")
    } else {
        line
    }
}

/// A table cell: one line, with `|` escaped.
pub(crate) fn cell(text: &str) -> String {
    collapse(text).replace('|', "\\|")
}

/// Builds tables (`w:tbl`, `a:tbl`) from cell text, nested tables included.
#[derive(Default)]
pub(crate) struct Tables {
    stack: Vec<Table>,
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: String,
}

impl Tables {
    pub(crate) fn open(&self) -> bool {
        !self.stack.is_empty()
    }

    pub(crate) fn start_table(&mut self) {
        self.stack.push(Table::default());
    }

    pub(crate) fn start_row(&mut self) {
        if let Some(t) = self.stack.last_mut() {
            t.row.clear();
        }
    }

    pub(crate) fn end_cell(&mut self) {
        if let Some(t) = self.stack.last_mut() {
            let c = cell(&std::mem::take(&mut t.cell));
            t.row.push(c);
        }
    }

    pub(crate) fn end_row(&mut self) {
        if let Some(t) = self.stack.last_mut() {
            let row = std::mem::take(&mut t.row);
            if row.iter().any(|c| !c.is_empty()) {
                t.rows.push(row);
            }
        }
    }

    /// Text inside the innermost open cell (a paragraph of it).
    pub(crate) fn push_text(&mut self, text: &str) {
        if let Some(t) = self.stack.last_mut() {
            if !t.cell.is_empty() && !text.is_empty() {
                t.cell.push(' ');
            }
            t.cell.push_str(text);
        }
    }

    /// Closes the innermost table: its rows as `| a | b |` lines, or, for a table inside
    /// a cell, its text added to that cell (and `None`).
    pub(crate) fn end_table(&mut self) -> Option<String> {
        let t = self.stack.pop()?;
        if self.open() {
            let text = t
                .rows
                .iter()
                .map(|r| r.join(" "))
                .collect::<Vec<_>>()
                .join(" ");
            self.push_text(&text);
            return None;
        }
        let lines: Vec<String> = t
            .rows
            .iter()
            .map(|r| format!("| {} |", r.join(" | ")))
            .collect();
        (!lines.is_empty()).then(|| lines.join("\n"))
    }
}

/// Text blocks joined as Markdown: list items on consecutive lines, everything else
/// apart by a blank line.
#[derive(Default)]
pub(crate) struct Blocks {
    text: String,
    last_was_item: bool,
}

impl Blocks {
    pub(crate) fn push(&mut self, block: &str, item: bool) {
        if block.trim().is_empty() {
            return;
        }
        if !self.text.is_empty() {
            self.text.push_str(if item && self.last_was_item {
                "\n"
            } else {
                "\n\n"
            });
        }
        self.text.push_str(block);
        self.last_was_item = item;
    }

    pub(crate) fn into_text(self) -> String {
        self.text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_resolve_against_their_part() {
        assert_eq!(
            resolve("ppt/", "slides/slide1.xml"),
            "ppt/slides/slide1.xml"
        );
        assert_eq!(
            resolve("ppt/slides/", "../notesSlides/notesSlide1.xml"),
            "ppt/notesSlides/notesSlide1.xml"
        );
        assert_eq!(resolve("", "/word/document.xml"), "word/document.xml");
        assert_eq!(resolve("a/", "../../../x.xml"), "x.xml");
        assert_eq!(split_part("word/document.xml"), ("word/", "document.xml"));
        assert_eq!(split_part(""), ("", ""));
    }

    #[test]
    fn structure_in_text_is_escaped() {
        assert_eq!(inline("# not a heading"), "\\# not a heading");
        assert_eq!(inline("  a\n b\t c "), "a b c");
        assert_eq!(inline("---"), "\\---");
        assert_eq!(inline("| x"), "\\| x");
        assert_eq!(inline("```rust"), "\\```rust");
        assert_eq!(inline("plain - text"), "plain - text");
        assert_eq!(cell("a | b\nc"), "a \\| b c");
    }

    #[test]
    fn references_and_fallbacks() {
        let xml = br#"<r xmlns:mc="m"><t>a &amp; b &#x41;&#66;</t><mc:AlternateContent><mc:Choice><t>one</t></mc:Choice><mc:Fallback><t>two</t></mc:Fallback></mc:AlternateContent></r>"#;
        let mut text = String::new();
        walk(xml, "x", |n| {
            if let Node::Text(t) = n {
                text.push_str(t);
            }
        })
        .unwrap();
        assert_eq!(text, "a & b ABone");
    }
}
