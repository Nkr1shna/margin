//! Workbooks, through `calamine`: one `#` section per worksheet (its name the section
//! path), each non-empty row a line of tab-separated cells, at most `OFFICE_SHEET_ROWS`
//! rows a sheet (the rest noted as "… more rows"). Cells are streamed, never loaded as a
//! whole range, and a sheet stops being read at its row cap or the workbook's text budget.

use std::collections::HashSet;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use calamine::{DataRef, Reader, SheetType, Xlsx, XlsxError};

use super::{
    Blocks, Node, OFFICE_SHEET_ROWS, Package, attr, collapse, escape_line, extract_error, walk,
};
use crate::{EmbedError, Extracted, Page, TextFormat};

/// Excel's widest sheet; a cell further right is from a broken file and is dropped.
const MAX_COLUMNS: u32 = 16_384;
/// The most text a workbook gives; reading stops there, noted like rows past the cap.
const TEXT_MAX_BYTES: usize = 32 * 1024 * 1024;
/// The most worksheets read from one workbook.
const MAX_SHEETS: usize = 1_000;

pub(super) fn read(path: &Path, package: &mut Package) -> Result<Extracted, EmbedError> {
    let distinct = distinct_sheets(package)?;
    // calamine inflates the parts itself: measure them through the caps first.
    package.check_sizes()?;
    let file = File::open(path).map_err(|e| extract_error(e.to_string()))?;
    let mut book: Xlsx<_> = Xlsx::new(BufReader::new(file)).map_err(xlsx_error)?;
    let mut seen = HashSet::new();
    let sheets: Vec<String> = book
        .sheets_metadata()
        .iter()
        .filter(|s| {
            s.typ == SheetType::WorkSheet
                && distinct.as_ref().is_none_or(|d| d.contains(&s.name))
                && seen.insert(s.name.clone())
        })
        .take(MAX_SHEETS)
        .map(|s| s.name.clone())
        .collect();
    let mut blocks = Blocks::default();
    let mut budget = TEXT_MAX_BYTES;
    for name in sheets {
        let (rows, out_of_budget) = sheet_rows(&mut book, &name, &mut budget)?;
        if !rows.is_empty() {
            let title = collapse(&name);
            blocks.push(&format!("# {title}"), false);
            blocks.push(&rows, false);
        }
        if out_of_budget {
            break;
        }
    }
    let text = blocks.into_text();
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

/// The names of the sheets to read: the first one listed for each worksheet part, since
/// `calamine` inflates a part again for every sheet naming it. `None` when the workbook
/// can't be read here; `calamine` then says what is wrong with it.
fn distinct_sheets(package: &mut Package) -> Result<Option<HashSet<String>>, EmbedError> {
    let main = package.main_part("xl/workbook.xml")?;
    let Some(xml) = package.part(&main)? else {
        return Ok(None);
    };
    let mut sheets = Vec::new();
    let walked = walk(&xml, &main, |node| {
        if let Node::Start(e) | Node::Empty(e) = node
            && e.local_name().as_ref() == b"sheet"
            && let (Some(name), Some(id)) = (attr(e, b"name"), attr(e, b"id"))
        {
            sheets.push((name, id));
        }
    });
    if walked.is_err() {
        return Ok(None);
    }
    let rels = package.rels(&main)?;
    let mut targets = HashSet::new();
    Ok(Some(
        sheets
            .into_iter()
            .filter(|(_, id)| {
                rels.iter()
                    .find(|r| &r.id == id)
                    .is_none_or(|r| targets.insert(r.target.clone()))
            })
            .map(|(name, _)| name)
            .collect(),
    ))
}

fn xlsx_error(e: XlsxError) -> EmbedError {
    match e {
        XlsxError::Password => extract_error(
            "the document is password-protected; save a copy without the password and add that",
        ),
        e => extract_error(format!("the spreadsheet is damaged ({e})")),
    }
}

/// The sheet's rows as lines, with "… more rows" past the caps, and whether the
/// workbook's text budget ran out on it.
fn sheet_rows(
    book: &mut Xlsx<BufReader<File>>,
    name: &str,
    budget: &mut usize,
) -> Result<(String, bool), EmbedError> {
    let mut cells = book.worksheet_cells_reader(name).map_err(xlsx_error)?;
    let mut lines = Rows::new(budget);
    let mut current: Option<u32> = None;
    let mut row: Vec<String> = Vec::new();
    let mut has_text = false;
    while let Some(cell) = cells.next_cell().map_err(xlsx_error)? {
        let (r, c) = cell.get_position();
        if current != Some(r) {
            lines.push(std::mem::take(&mut row), std::mem::take(&mut has_text));
            current = Some(r);
        }
        if c >= MAX_COLUMNS {
            continue;
        }
        let text = cell_text(cell.get_value());
        if text.is_empty() {
            continue;
        }
        has_text = true;
        if lines.full() {
            // A row past the caps has text: note it and stop reading the sheet.
            lines.truncated = true;
            return Ok(lines.finish());
        }
        let c = c as usize;
        if row.len() <= c {
            row.resize(c + 1, String::new());
        }
        row[c] = text;
    }
    lines.push(row, has_text);
    Ok(lines.finish())
}

/// The lines of one sheet, under the row cap and the workbook's text budget.
struct Rows<'a> {
    lines: Vec<String>,
    truncated: bool,
    budget: &'a mut usize,
    out_of_budget: bool,
}

impl<'a> Rows<'a> {
    fn new(budget: &'a mut usize) -> Rows<'a> {
        Rows {
            lines: Vec::new(),
            truncated: false,
            budget,
            out_of_budget: false,
        }
    }

    fn full(&self) -> bool {
        self.lines.len() >= OFFICE_SHEET_ROWS || self.out_of_budget
    }

    /// Ends a row: kept as a line when it has text and there is room, noted when it has
    /// text and there isn't.
    fn push(&mut self, cells: Vec<String>, has_text: bool) {
        if !has_text {
            return;
        }
        if self.full() {
            self.truncated = true;
            return;
        }
        let last = cells.iter().rposition(|c| !c.is_empty()).unwrap_or(0);
        let line = escape_line(cells[..=last].join("\t"));
        if line.len() > *self.budget {
            self.out_of_budget = true;
            self.truncated = true;
            return;
        }
        *self.budget -= line.len();
        self.lines.push(line);
    }

    fn finish(self) -> (String, bool) {
        let mut text = self.lines.join("\n");
        if self.truncated {
            text.push_str("\n… more rows");
        }
        (text, self.out_of_budget)
    }
}

fn cell_text(value: &DataRef) -> String {
    let text = match value {
        DataRef::Int(i) => i.to_string(),
        DataRef::Float(f) => {
            if f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{}", *f as i64)
            } else {
                f.to_string()
            }
        }
        DataRef::String(s) => s.clone(),
        DataRef::SharedString(s) => s.to_string(),
        DataRef::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        DataRef::DateTime(d) => match d.as_datetime() {
            Some(dt) if d.is_datetime() => {
                if dt.format("%H:%M:%S").to_string() == "00:00:00" {
                    dt.format("%Y-%m-%d").to_string()
                } else {
                    dt.format("%Y-%m-%d %H:%M:%S").to_string()
                }
            }
            _ => d.to_string(),
        },
        DataRef::DateTimeIso(s) | DataRef::DurationIso(s) => s.clone(),
        DataRef::Error(e) => e.to_string(),
        DataRef::Empty => String::new(),
    };
    collapse(&text)
}
