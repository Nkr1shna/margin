//! PDFs with pdfium (`pdfium-render` 0.9.4, MIT/Apache-2.0, binding `libpdfium` from
//! `bblanchon/pdfium-binaries`, BSD-3-Clause, at run time; `docs/plan-linux-service.md`
//! §2.2). MuPDF is AGPL and isn't used.
//!
//! - Text: each page's text layer; a page with fewer than `MIN_TEXT_CHARS` non-space
//!   characters (a scan) is rendered in gray at 200 dpi and read with tesseract
//!   (`ocr_tesseract.rs`), and the longer of the two is kept.
//! - Pictures: each page rendered in color at 144 dpi, its longer side at most 2048 px
//!   written as PNG.
//!
//! The library is looked for at `$MARGIN_PDFIUM` (the file or its folder), in the tools
//! folder the service sets (`set_tools_dir`, `data_dir/tools`, so a user install needs no
//! root), then in the system library folders, then wherever the loader finds it. It is
//! bound once per process; a missing library is looked for again on the next call.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use image::DynamicImage;
use pdfium_render::prelude::*;

use crate::{EmbedError, Extracted, Page};

/// Overrides where the pdfium library is looked for (the file, or a folder holding it).
pub const PDFIUM_ENV: &str = "MARGIN_PDFIUM";

/// Pages with fewer non-space characters than this in their text layer are OCR'd.
const MIN_TEXT_CHARS: usize = 16;
/// Pictures of pages: 144 dpi (2 px per point)…
const PICTURE_SCALE: f32 = 2.0;
/// …with the longer side at most this many pixels.
const PICTURE_MAX_SIDE: f32 = 2048.0;
/// OCR renders: 200 dpi…
const OCR_SCALE: f32 = 200.0 / 72.0;
/// …at most this many pixels on the longer side.
const OCR_MAX_SIDE: f32 = 4096.0;

static TOOLS_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Where the service keeps tools a user installed (`data_dir/tools`): searched for the
/// pdfium library before the system folders.
pub fn set_tools_dir(dir: impl Into<PathBuf>) {
    *TOOLS_DIR.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir.into());
}

/// The library's file name on this platform (`libpdfium.so`, `libpdfium.dylib`).
pub fn pdfium_library_name() -> String {
    Pdfium::pdfium_platform_library_name()
        .to_string_lossy()
        .into_owned()
}

/// The places the library is looked for, in order.
fn candidates() -> Vec<PathBuf> {
    let name = pdfium_library_name();
    let mut out = Vec::new();
    if let Some(env) = std::env::var_os(PDFIUM_ENV) {
        let path = PathBuf::from(env);
        if path.is_dir() {
            out.push(path.join(&name));
        } else {
            out.push(path);
        }
    }
    if let Some(dir) = TOOLS_DIR.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        out.push(dir.join(&name));
        out.push(dir.join("pdfium/lib").join(&name));
    }
    for dir in [
        "/usr/local/lib",
        "/usr/lib",
        "/usr/lib/aarch64-linux-gnu",
        "/usr/lib/x86_64-linux-gnu",
        "/opt/homebrew/lib",
    ] {
        out.push(Path::new(dir).join(&name));
    }
    out
}

/// The pdfium library file `pdfium` would bind, when one is found in the known places.
pub fn find_pdfium() -> Option<PathBuf> {
    candidates().into_iter().find(|p| p.is_file())
}

/// The process's pdfium, bound on first success. `Pdfium` is `Send + Sync` with
/// `pdfium-render`'s `thread_safe` feature (on by default): calls are serialized.
fn pdfium() -> Result<&'static Pdfium, EmbedError> {
    static PDFIUM: Mutex<Option<&'static Pdfium>> = Mutex::new(None);
    let mut slot = PDFIUM.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(pdfium) = *slot {
        return Ok(pdfium);
    }
    let bindings = candidates()
        .into_iter()
        .filter(|p| p.is_file())
        .find_map(|p| Pdfium::bind_to_library(p).ok())
        .map_or_else(Pdfium::bind_to_system_library, Ok)
        .map_err(|e| {
            EmbedError::Extract(format!(
                "reading PDFs needs the pdfium library ({}): {e}",
                pdfium_library_name()
            ))
        })?;
    let pdfium: &'static Pdfium = Box::leak(Box::new(Pdfium::new(bindings)));
    *slot = Some(pdfium);
    Ok(pdfium)
}

fn pdf_error(e: PdfiumError) -> EmbedError {
    match e {
        PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError) => {
            EmbedError::Extract("the PDF is password-protected".into())
        }
        PdfiumError::PdfiumLibraryInternalError(
            PdfiumInternalError::FormatError | PdfiumInternalError::FileError,
        ) => EmbedError::Extract("not a PDF, or it couldn't be opened".into()),
        e => EmbedError::Extract(format!("couldn't read the PDF: {e}")),
    }
}

fn open<'a>(pdfium: &'a Pdfium, path: &Path) -> Result<PdfDocument<'a>, EmbedError> {
    pdfium.load_pdf_from_file(path, None).map_err(pdf_error)
}

/// The scale a `width × height` point page is drawn at: `scale`, smaller for huge pages
/// so the longer side is at most `max_side` pixels.
pub(crate) fn fit_scale(width: f32, height: f32, scale: f32, max_side: f32) -> f32 {
    let longest = width.max(height);
    if longest <= 0.0 {
        return scale;
    }
    scale.min(max_side / longest)
}

fn non_space_chars(text: &str) -> usize {
    text.chars().filter(|c| !c.is_whitespace()).count()
}

/// The page's text: its layer when it has enough, else the OCR text when that has more.
pub(crate) fn choose_text(
    layer: String,
    ocr: impl FnOnce() -> Result<String, EmbedError>,
) -> Result<String, EmbedError> {
    if non_space_chars(&layer) >= MIN_TEXT_CHARS {
        return Ok(layer);
    }
    let ocr = ocr()?;
    Ok(if non_space_chars(&ocr) > non_space_chars(&layer) {
        ocr
    } else {
        layer
    })
}

/// The page drawn on white at `scale` (capped to `max_side`), in color or gray.
fn render(
    page: &PdfPage<'_>,
    scale: f32,
    max_side: f32,
    gray: bool,
) -> Result<DynamicImage, EmbedError> {
    let scale = fit_scale(page.width().value, page.height().value, scale, max_side);
    let config = PdfRenderConfig::new()
        .scale_page_by_factor(scale)
        .set_clear_color(PdfColor::WHITE)
        .render_form_data(true)
        .use_grayscale_rendering(gray);
    let bitmap = page.render_with_config(&config).map_err(pdf_error)?;
    let image = bitmap.as_image().map_err(pdf_error)?;
    Ok(if gray {
        DynamicImage::ImageLuma8(image.to_luma8())
    } else {
        DynamicImage::ImageRgb8(image.to_rgb8())
    })
}

/// The text of every page (`ocr` reads a page picture when the layer is empty); with
/// `media_dir`, each page is also drawn there as `page_file(n)` and listed as a picture.
pub(crate) fn extract_with(
    path: &Path,
    media_dir: Option<&Path>,
    ocr: &dyn Fn(&DynamicImage) -> Result<String, EmbedError>,
) -> Result<Extracted, EmbedError> {
    let pdfium = pdfium()?;
    let doc = open(pdfium, path)?;
    if let Some(dir) = media_dir {
        std::fs::create_dir_all(dir)?;
    }
    let mut pages = Vec::new();
    let mut pictures = Vec::new();
    for (index, page) in doc.pages().iter().enumerate() {
        let number = index as u32 + 1;
        if let Some(dir) = media_dir {
            let file = dir.join(crate::page_file(number));
            crate::image_linux::write_png(
                &render(&page, PICTURE_SCALE, PICTURE_MAX_SIDE, false)?,
                &file,
            )?;
            pictures.push(crate::Picture {
                path: file,
                page: Some(number),
                at_ms: None,
                tokens: crate::ImageTokens::High,
            });
        }
        let layer = page
            .text()
            .map(|t| t.all())
            .unwrap_or_default()
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        let text = choose_text(layer, || {
            ocr(&render(&page, OCR_SCALE, OCR_MAX_SIDE, true)?)
        })?;
        pages.push(Page {
            number: Some(number),
            text,
        });
    }
    Ok(Extracted {
        pages,
        pictures,
        ..Default::default()
    })
}

/// `extract_with` tesseract as the OCR.
pub(crate) fn extract(path: &Path, media_dir: Option<&Path>) -> Result<Extracted, EmbedError> {
    extract_with(path, media_dir, &crate::ocr_tesseract::recognize)
}

/// Page `number` (from 1) drawn as `extract` draws page pictures, written to `out` as
/// PNG.
pub(crate) fn page_picture(path: &Path, number: u32, out: &Path) -> Result<(), EmbedError> {
    let pdfium = pdfium()?;
    let doc = open(pdfium, path)?;
    let count = doc.pages().len() as u32;
    if number == 0 || number > count {
        return Err(EmbedError::Extract(format!(
            "the PDF has no page {number} (it has {count})"
        )));
    }
    let page = doc
        .pages()
        .get((number - 1) as PdfPageIndex)
        .map_err(pdf_error)?;
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::image_linux::write_png(&render(&page, PICTURE_SCALE, PICTURE_MAX_SIDE, false)?, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_linux::tests::temp_dir;

    #[test]
    fn pages_are_drawn_at_144_dpi_capped_at_2048() {
        // US Letter: 612 × 792 pt → 1224 × 1584 px.
        assert_eq!(
            fit_scale(612.0, 792.0, PICTURE_SCALE, PICTURE_MAX_SIDE),
            2.0
        );
        // A poster is capped on its longer side.
        let s = fit_scale(2000.0, 1000.0, PICTURE_SCALE, PICTURE_MAX_SIDE);
        assert!((2000.0 * s - 2048.0).abs() < 1e-3);
        assert!((fit_scale(612.0, 792.0, OCR_SCALE, OCR_MAX_SIDE) - 200.0 / 72.0).abs() < 1e-6);
        assert_eq!(fit_scale(0.0, 0.0, 2.0, 10.0), 2.0);
    }

    #[test]
    fn a_text_layer_wins_unless_it_is_nearly_empty() {
        let layer = "Photosynthesis turns light into sugar.".to_string();
        let text = choose_text(layer.clone(), || panic!("no OCR for a text page")).unwrap();
        assert_eq!(text, layer);
        let scanned = choose_text(" \n".into(), || Ok("The invoice number is 48213.".into()));
        assert_eq!(scanned.unwrap(), "The invoice number is 48213.");
        // OCR finding less than the layer keeps the layer.
        assert_eq!(
            choose_text("Fig. 3".into(), || Ok("F".into())).unwrap(),
            "Fig. 3"
        );
        assert!(choose_text(String::new(), || Err(EmbedError::Extract("x".into()))).is_err());
    }

    #[test]
    fn the_library_is_looked_for_in_the_tools_folder() {
        let dir = temp_dir("pdfium-tools");
        set_tools_dir(&dir);
        let name = pdfium_library_name();
        assert!(name.contains("pdfium"));
        assert!(candidates().contains(&dir.join(&name)));
    }

    /// With the pdfium library present (the Linux runner installs it; elsewhere this
    /// skips): the corpus PDFs read in full.
    #[test]
    fn real_pdfium_reads_the_corpus_when_installed() {
        if find_pdfium().is_none() {
            eprintln!("skipping: the pdfium library isn't installed");
            return;
        }
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/corpus");
        let pdf = corpus.join("pdf/rc-circuits.pdf");
        let media = temp_dir("pdfium-media");
        let lazy = extract_with(&pdf, None, &|_| Ok(String::new())).unwrap();
        assert!(lazy.pictures.is_empty());
        let sources = std::fs::read_to_string(corpus.join("sources/rc-circuits.txt")).unwrap();
        let sources: Vec<&str> = sources.split("\n\\f\n").collect();
        assert_eq!(lazy.pages.len(), sources.len());
        let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        for (page, source) in lazy.pages.iter().zip(&sources) {
            let first = source.lines().next().unwrap();
            assert!(words(&page.text).contains(&words(first)), "{first:?}");
        }
        let eager = extract_with(&pdf, Some(&media), &|_| Ok(String::new())).unwrap();
        assert_eq!(eager.pictures.len(), eager.pages.len());
        let out = temp_dir("pdfium-page").join("page-2.png");
        page_picture(&pdf, 2, &out).unwrap();
        assert_eq!(
            std::fs::read(&out).unwrap(),
            std::fs::read(&eager.pictures[1].path).unwrap()
        );
        let picture = crate::image_linux::load(&out).unwrap();
        assert!(picture.width().max(picture.height()) <= 2048);
        assert!(page_picture(&pdf, 0, &out).is_err());
        let past = page_picture(&pdf, 99, &out).unwrap_err();
        assert!(past.to_string().contains("no page 99"), "{past}");
        // The scan has no text layer: OCR is asked for its page.
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let scan = extract_with(&corpus.join("pdf/glacier-scan.pdf"), None, &|image| {
            assert!(matches!(image, DynamicImage::ImageLuma8(_)));
            asked.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok("accumulation zone and the equilibrium line".into())
        })
        .unwrap();
        assert!(asked.load(std::sync::atomic::Ordering::Relaxed) >= 1);
        assert!(
            scan.pages
                .iter()
                .any(|p| p.text.contains("equilibrium line"))
        );
        let fake = media.join("fake.pdf");
        std::fs::write(&fake, "not a pdf").unwrap();
        assert!(matches!(
            extract_with(&fake, None, &|_| Ok(String::new())),
            Err(EmbedError::Extract(_))
        ));
    }
}
