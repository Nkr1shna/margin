//! The Office readers on small files made here: Word headings, lists and tables as
//! Markdown sections, slides as numbered pages with their notes, sheets as sections, and
//! damaged, encrypted, legacy and oversized files as errors (never a panic).

use std::io::Write;
use std::path::{Path, PathBuf};

use margin_embed::office::{self, OFFICE_SHEET_ROWS, OfficeFormat};
use margin_embed::{EmbedError, SectionsCfg, SourceKind, chunk_sections};
use zip::write::SimpleFileOptions;

const CT_HEAD: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/>"#;
const RELS_HEAD: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#;
const OFFICE_DOC: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let dir = std::env::temp_dir().join(format!("margin-office-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Dir(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_zip(path: &Path, entries: &[(&str, Vec<u8>)]) {
    let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in entries {
        zip.start_file(*name, options).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap();
}

fn rel(id: &str, typ: &str, target: &str) -> String {
    format!(r#"<Relationship Id="{id}" Type="{typ}" Target="{target}"/>"#)
}

fn ct(overrides: &[(&str, &str)]) -> Vec<u8> {
    let mut s = CT_HEAD.to_string();
    for (part, t) in overrides {
        s.push_str(&format!(
            r#"<Override PartName="{part}" ContentType="{t}"/>"#
        ));
    }
    s.push_str("</Types>");
    s.into_bytes()
}

fn rels(items: &[String]) -> Vec<u8> {
    format!("{RELS_HEAD}{}</Relationships>", items.concat()).into_bytes()
}

fn docx_entries(body: &str) -> Vec<(&'static str, Vec<u8>)> {
    vec![
        (
            "[Content_Types].xml",
            ct(&[(
                "/word/document.xml",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
            )]),
        ),
        ("_rels/.rels", rels(&[rel("rId1", OFFICE_DOC, "word/document.xml")])),
        (
            "word/document.xml",
            format!(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}<w:sectPr/></w:body></w:document>"#
            )
            .into_bytes(),
        ),
    ]
}

fn para(style: &str, text: &str) -> String {
    let ppr = if style.is_empty() {
        String::new()
    } else {
        format!(r#"<w:pPr><w:pStyle w:val="{style}"/></w:pPr>"#)
    };
    format!(r#"<w:p>{ppr}<w:r><w:t xml:space="preserve">{text}</w:t></w:r></w:p>"#)
}

fn sections(doc: &margin_embed::Extracted) -> Vec<margin_embed::Chunk> {
    // Office text is Markdown-shaped; the chunker reads it as it reads a text document.
    let count = |s: &str| s.split_whitespace().count();
    chunk_sections(doc, SourceKind::Text, &count, &SectionsCfg::default())
}

fn extract_err(path: &Path) -> String {
    match office::extract(path) {
        Err(EmbedError::Extract(why)) => why,
        other => panic!("expected an extract error, got {other:?}"),
    }
}

#[test]
fn docx_headings_lists_and_tables_become_sections() {
    let dir = Dir::new("docx");
    let body = [
        para("Title", "Onboarding"),
        para("", "Welcome to the team."),
        para("Heading1", "Accounts"),
        r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Request a laptop</w:t></w:r></w:p>"#.to_string(),
        r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>from IT</w:t></w:r></w:p>"#.to_string(),
        para("Heading2", "Contacts"),
        "<w:tbl><w:tr><w:tc><w:p><w:r><w:t>Team</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Person</w:t></w:r></w:p></w:tc></w:tr><w:tr><w:tc><w:p><w:r><w:t>IT</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Dana</w:t></w:r></w:p></w:tc></w:tr></w:tbl>".to_string(),
    ]
    .concat();
    let path = dir.path("onboarding.docx");
    write_zip(&path, &docx_entries(&body));
    assert_eq!(office::detect(&path), Some(OfficeFormat::Docx));
    let doc = office::extract(&path).unwrap();
    assert_eq!(doc.pages.len(), 1);
    assert_eq!(doc.pages[0].number, None);
    assert_eq!(
        doc.pages[0].text,
        "# Onboarding\n\nWelcome to the team.\n\n# Accounts\n\n- Request a laptop\n  - from IT\n\n## Contacts\n\n| Team | Person |\n| IT | Dana |"
    );
    let chunks = sections(&doc);
    let paths: Vec<&str> = chunks.iter().map(|c| c.section_path.as_str()).collect();
    assert_eq!(paths, ["Onboarding", "Accounts", "Accounts > Contacts"]);
    assert!(chunks[2].text.contains("| IT | Dana |"));

    // Without an extension the zip's content types say what it is.
    let bare = dir.path("onboarding");
    std::fs::copy(&path, &bare).unwrap();
    assert_eq!(office::extract(&bare).unwrap(), doc);
}

fn pptx_entries() -> Vec<(&'static str, Vec<u8>)> {
    let slide = |title: &str, body: &str| {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>{title}</a:t></a:r></a:p></p:txBody></p:sp><p:sp><p:nvSpPr><p:cNvPr id="3" name="Body"/><p:cNvSpPr/><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>{body}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
        )
        .into_bytes()
    };
    let notes = r#"<?xml version="1.0"?><p:notes xmlns:a="a" xmlns:p="p"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:nvPr><p:ph type="sldImg"/></p:nvPr></p:nvSpPr></p:sp><p:sp><p:nvSpPr><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>Mention the churn number.</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:notes>"#;
    let slide_rel = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide";
    let notes_rel =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/notesSlide";
    vec![
        (
            "[Content_Types].xml",
            ct(&[(
                "/ppt/presentation.xml",
                "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml",
            )]),
        ),
        ("_rels/.rels", rels(&[rel("rId1", OFFICE_DOC, "ppt/presentation.xml")])),
        // The deck shows slide2.xml first: order is the list's, not the file names'.
        (
            "ppt/presentation.xml",
            br#"<?xml version="1.0"?><p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldIdLst><p:sldId id="256" r:id="rId7"/><p:sldId id="257" r:id="rId3"/><p:sldId id="258" r:id="rId9"/></p:sldIdLst></p:presentation>"#.to_vec(),
        ),
        (
            "ppt/_rels/presentation.xml.rels",
            rels(&[
                rel("rId3", slide_rel, "slides/slide1.xml"),
                rel("rId7", slide_rel, "slides/slide2.xml"),
                rel("rId9", slide_rel, "/ppt/slides/slide3.xml"),
            ]),
        ),
        ("ppt/slides/slide1.xml", slide("Metrics", "Churn fell to 2%")),
        ("ppt/slides/slide2.xml", slide("Q3 review", "Agenda")),
        ("ppt/slides/slide3.xml", slide("", "")),
        (
            "ppt/slides/_rels/slide1.xml.rels",
            rels(&[rel("rId1", notes_rel, "../notesSlides/notesSlide1.xml")]),
        ),
        ("ppt/notesSlides/notesSlide1.xml", notes.as_bytes().to_vec()),
    ]
}

#[test]
fn pptx_slides_are_numbered_pages_with_their_notes() {
    let dir = Dir::new("pptx");
    let path = dir.path("q3.pptx");
    write_zip(&path, &pptx_entries());
    let doc = office::extract(&path).unwrap();
    let pages: Vec<(Option<u32>, &str)> = doc
        .pages
        .iter()
        .map(|p| (p.number, p.text.as_str()))
        .collect();
    assert_eq!(
        pages,
        [
            (Some(1), "# Q3 review\n\nAgenda"),
            (
                Some(2),
                "# Metrics\n\nChurn fell to 2%\n\nNotes:\n\nMention the churn number."
            ),
            (Some(3), ""),
        ]
    );
    let chunks = sections(&doc);
    let churn = chunks.iter().find(|c| c.text.contains("Churn")).unwrap();
    assert_eq!(churn.page, Some(2));
    assert_eq!(churn.section_path, "Metrics");
    assert!(chunks.iter().all(|c| c.page != Some(3)));
}

fn xlsx_entries(sheets: &[(&str, String)]) -> Vec<(String, Vec<u8>)> {
    let ws_rel = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet";
    let mut overrides = vec![(
        "/xl/workbook.xml".to_string(),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml".to_string(),
    )];
    let mut sheet_list = String::new();
    let mut wb_rels = Vec::new();
    let mut entries = Vec::new();
    for (i, (name, data)) in sheets.iter().enumerate() {
        let n = i + 1;
        overrides.push((
            format!("/xl/worksheets/sheet{n}.xml"),
            "application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml".into(),
        ));
        sheet_list.push_str(&format!(
            r#"<sheet name="{name}" sheetId="{n}" r:id="rId{n}"/>"#
        ));
        wb_rels.push(rel(
            &format!("rId{n}"),
            ws_rel,
            &format!("worksheets/sheet{n}.xml"),
        ));
        entries.push((
            format!("xl/worksheets/sheet{n}.xml"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{data}</sheetData></worksheet>"#
            )
            .into_bytes(),
        ));
    }
    let overrides: Vec<(&str, &str)> = overrides
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let mut all = vec![
        ("[Content_Types].xml".to_string(), ct(&overrides)),
        ("_rels/.rels".to_string(), rels(&[rel("rId1", OFFICE_DOC, "xl/workbook.xml")])),
        (
            "xl/workbook.xml".to_string(),
            format!(
                r#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets>{sheet_list}</sheets></workbook>"#
            )
            .into_bytes(),
        ),
        ("xl/_rels/workbook.xml.rels".to_string(), rels(&wb_rels)),
    ];
    all.extend(entries);
    all
}

fn write_xlsx(path: &Path, sheets: &[(&str, String)]) {
    let entries = xlsx_entries(sheets);
    let entries: Vec<(&str, Vec<u8>)> = entries
        .iter()
        .map(|(n, b)| (n.as_str(), b.clone()))
        .collect();
    write_zip(path, &entries);
}

fn row(r: u32, cells: &[(&str, &str)]) -> String {
    let cells: String = cells
        .iter()
        .map(|(col, v)| match v.parse::<f64>() {
            Ok(_) => format!(r#"<c r="{col}{r}"><v>{v}</v></c>"#),
            Err(_) => format!(r#"<c r="{col}{r}" t="inlineStr"><is><t>{v}</t></is></c>"#),
        })
        .collect();
    format!(r#"<row r="{r}">{cells}</row>"#)
}

#[test]
fn xlsx_sheets_are_sections_of_rows() {
    let dir = Dir::new("xlsx");
    let path = dir.path("budget.xlsx");
    let costs = [
        row(1, &[("A", "Item"), ("B", "Cost")]),
        row(2, &[("A", "Laptops"), ("B", "12000"), ("D", "approved")]),
        row(3, &[]),
        row(5, &[("B", "2.5")]),
        row(6, &[("A", "#hashtag")]),
    ]
    .concat();
    write_xlsx(
        &path,
        &[
            ("Costs", costs),
            ("Empty", String::new()),
            ("People", row(1, &[("A", "Dana"), ("B", "IT")])),
        ],
    );
    assert_eq!(office::detect(&path), Some(OfficeFormat::Xlsx));
    let doc = office::extract(&path).unwrap();
    assert_eq!(doc.pages.len(), 1);
    assert_eq!(
        doc.pages[0].text,
        "# Costs\n\nItem\tCost\nLaptops\t12000\t\tapproved\n\t2.5\n\\#hashtag\n\n# People\n\nDana\tIT"
    );
    let chunks = sections(&doc);
    let paths: Vec<&str> = chunks.iter().map(|c| c.section_path.as_str()).collect();
    assert_eq!(paths, ["Costs", "People"]);
}

#[test]
fn xlsx_rows_past_the_cap_are_counted() {
    let dir = Dir::new("xlsx-cap");
    let path = dir.path("big.xlsx");
    let extra = 5;
    let data: String = (1..=(OFFICE_SHEET_ROWS + extra) as u32)
        .map(|r| format!(r#"<row r="{r}"><c r="A{r}"><v>{r}</v></c></row>"#))
        .collect();
    write_xlsx(&path, &[("Log", data)]);
    let text = &office::extract(&path).unwrap().pages[0].text;
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2 + OFFICE_SHEET_ROWS + 1);
    assert_eq!(
        lines[2 + OFFICE_SHEET_ROWS - 1],
        OFFICE_SHEET_ROWS.to_string()
    );
    assert_eq!(*lines.last().unwrap(), "… more rows");
}

#[test]
fn damaged_files_are_errors() {
    let dir = Dir::new("damaged");
    // Not a zip at all.
    let path = dir.path("junk.docx");
    std::fs::write(&path, b"PK\x03\x04 this is not really a zip").unwrap();
    assert!(extract_err(&path).contains("damaged"));
    let empty = dir.path("empty.docx");
    std::fs::write(&empty, b"").unwrap();
    extract_err(&empty);

    // A real document cut in half.
    let whole = dir.path("whole.docx");
    write_zip(&whole, &docx_entries(&para("", "Some text here.")));
    let bytes = std::fs::read(&whole).unwrap();
    let cut = dir.path("cut.docx");
    std::fs::write(&cut, &bytes[..bytes.len() / 2]).unwrap();
    extract_err(&cut);

    // Every prefix and a few corruptions of each format: errors or text, never a panic.
    let pptx = dir.path("deck.pptx");
    write_zip(&pptx, &pptx_entries());
    let xlsx = dir.path("sheet.xlsx");
    write_xlsx(&xlsx, &[("S", row(1, &[("A", "x")]))]);
    for source in [&whole, &pptx, &xlsx] {
        let bytes = std::fs::read(source).unwrap();
        let name = source.file_name().unwrap().to_string_lossy();
        let broken = dir.path(&format!("broken-{name}"));
        for k in (0..bytes.len()).step_by(97) {
            let mut b = bytes.clone();
            b[k] ^= 0x5a;
            std::fs::write(&broken, &b).unwrap();
            let _ = office::extract(&broken);
            std::fs::write(&broken, &bytes[..k]).unwrap();
            let _ = office::extract(&broken);
        }
    }

    // A zip with no document in it.
    let hollow = dir.path("hollow.docx");
    write_zip(
        &hollow,
        &[(
            "[Content_Types].xml",
            ct(&[(
                "/word/document.xml",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
            )]),
        )],
    );
    assert!(extract_err(&hollow).contains("no body"));

    // Broken XML in the body.
    let bad_xml = dir.path("bad.docx");
    let mut entries = docx_entries("");
    entries[2].1 = b"<w:document><w:body><w:p><w:t>a</w:t".to_vec();
    write_zip(&bad_xml, &entries);
    let _ = office::extract(&bad_xml);

    // A zip that is no Office file.
    let other = dir.path("photos.zip");
    write_zip(&other, &[("a.txt", b"hello".to_vec())]);
    assert!(extract_err(&other).contains("isn't a Word document"));
    assert_eq!(office::detect(&other), None);
}

/// An OLE compound file: its magic, then `streams` (UTF-16) somewhere in it.
fn compound_file(streams: &[&str]) -> Vec<u8> {
    let mut bytes = vec![0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    bytes.resize(512, 0);
    for s in streams {
        bytes.extend(s.encode_utf16().flat_map(u16::to_le_bytes));
        bytes.resize(bytes.len() + 64, 0);
    }
    bytes
}

#[test]
fn password_protected_and_legacy_files_say_why() {
    let dir = Dir::new("cfb");
    let locked = dir.path("locked.docx");
    std::fs::write(
        &locked,
        compound_file(&["EncryptionInfo", "EncryptedPackage"]),
    )
    .unwrap();
    assert!(extract_err(&locked).contains("password-protected"));
    let old = dir.path("old.doc");
    std::fs::write(&old, compound_file(&["WordDocument"])).unwrap();
    assert!(extract_err(&old).contains("old Office file"));
}

#[test]
fn oversized_parts_are_refused() {
    let dir = Dir::new("bomb");
    // 65 MB of spaces deflates to well under a megabyte.
    let big = vec![b' '; (office::OFFICE_PART_MAX_BYTES + (1 << 20)) as usize];
    let mut entries = docx_entries("");
    entries[2].1 = big.clone();
    let path = dir.path("bomb.docx");
    write_zip(&path, &entries);
    assert!(std::fs::metadata(&path).unwrap().len() < 1 << 20);
    assert!(extract_err(&path).contains("more than 64 MB"));

    let xlsx = dir.path("bomb.xlsx");
    let mut entries = xlsx_entries(&[("S", row(1, &[("A", "x")]))]);
    entries.push(("xl/sharedStrings.xml".into(), big));
    let entries: Vec<(&str, Vec<u8>)> = entries
        .iter()
        .map(|(n, b)| (n.as_str(), b.clone()))
        .collect();
    write_zip(&xlsx, &entries);
    assert!(extract_err(&xlsx).contains("more than 64 MB"));
}

#[test]
fn repeated_sheet_and_slide_references_are_read_once() {
    let dir = Dir::new("repeats");
    let limit = std::time::Duration::from_secs(20);

    // A thousand sheets, every one naming the same big worksheet part.
    let data: String = (1..=(OFFICE_SHEET_ROWS + 100) as u32)
        .map(|r| row(r, &[("A", "x"), ("B", &r.to_string())]))
        .collect();
    let mut entries = xlsx_entries(&[("S0", data)]);
    let sheet_list: String = (0..1000)
        .map(|i| format!(r#"<sheet name="S{i}" sheetId="{}" r:id="rId1"/>"#, i + 1))
        .collect();
    entries[2].1 = format!(
        r#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets>{sheet_list}</sheets></workbook>"#
    )
    .into_bytes();
    let entries: Vec<(&str, Vec<u8>)> = entries
        .iter()
        .map(|(n, b)| (n.as_str(), b.clone()))
        .collect();
    let xlsx = dir.path("repeats.xlsx");
    write_zip(&xlsx, &entries);
    let started = std::time::Instant::now();
    let doc = office::extract(&xlsx).unwrap();
    assert!(started.elapsed() < limit, "took {:?}", started.elapsed());
    let text = &doc.pages[0].text;
    assert_eq!(
        text.matches("\n# ").count() + text.starts_with("# ") as usize,
        1
    );
    assert!(text.starts_with("# S0\n"));
    assert!(text.ends_with("\n… more rows"));

    // Five thousand slides, every one the same slide part.
    let mut entries = pptx_entries();
    let ids: String = (0..5000)
        .map(|i| format!(r#"<p:sldId id="{}" r:id="rId3"/>"#, 256 + i))
        .collect();
    entries[2].1 = format!(
        r#"<?xml version="1.0"?><p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldIdLst>{ids}</p:sldIdLst></p:presentation>"#
    )
    .into_bytes();
    let pptx = dir.path("repeats.pptx");
    write_zip(&pptx, &entries);
    let started = std::time::Instant::now();
    let doc = office::extract(&pptx).unwrap();
    assert!(started.elapsed() < limit, "took {:?}", started.elapsed());
    assert_eq!(doc.pages.len(), 1);
    assert!(doc.pages[0].text.starts_with("# Metrics"));
}

#[test]
fn doctype_entities_are_not_expanded() {
    let dir = Dir::new("doctype");
    // A "billion laughs" document: each entity ten of the one before.
    let mut entities = r#"<!ENTITY a0 "laugh">"#.to_string();
    for i in 1..10 {
        let refs = format!("&a{};", i - 1).repeat(10);
        entities.push_str(&format!(r#"<!ENTITY a{i} "{refs}">"#));
    }
    let mut entries = docx_entries("");
    entries[2].1 = format!(
        r#"<?xml version="1.0"?><!DOCTYPE w:document [{entities}]><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Before &a9; after</w:t></w:r></w:p></w:body></w:document>"#
    )
    .into_bytes();
    let path = dir.path("laughs.docx");
    write_zip(&path, &entries);
    let started = std::time::Instant::now();
    let result = office::extract(&path);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    if let Ok(doc) = result {
        let len: usize = doc.pages.iter().map(|p| p.text.len()).sum();
        assert!(len < 1024, "an entity was expanded to {len} bytes");
    }
}

#[test]
fn dot_dot_targets_stay_inside_the_package() {
    let dir = Dir::new("dotdot");
    // `..` past the package root stays at the root: this names word/document.xml.
    let mut entries = docx_entries(&para("", "Inside the zip."));
    entries[1].1 = rels(&[rel("rId1", OFFICE_DOC, "../../../word/document.xml")]);
    let path = dir.path("climb.docx");
    write_zip(&path, &entries);
    let doc = office::extract(&path).unwrap();
    assert_eq!(doc.pages[0].text, "Inside the zip.");

    // A target that only exists outside the package is a missing part, not a file read.
    let outside = dir.path("outside.xml");
    std::fs::write(&outside, b"<w:document/>").unwrap();
    let mut entries = docx_entries(&para("", "x"));
    entries[1].1 = rels(&[rel("rId1", OFFICE_DOC, "../outside.xml")]);
    entries.remove(2);
    let path = dir.path("escape.docx");
    write_zip(&path, &entries);
    assert!(extract_err(&path).contains("no body"));
}
