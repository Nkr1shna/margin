//! PDF text with PDFKit, page by page. A page whose text layer is empty or nearly so
//! (a scan) is rendered to a grayscale bitmap with Core Graphics and read with Vision's
//! text recognition (accurate, with language correction).

use std::path::Path;
use std::ptr;

use objc2::AnyThread;
use objc2::rc::autoreleasepool;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGContext, CGImage,
    CGImageAlphaInfo, CGInterpolationQuality, CGPDFBox, CGPDFPage,
};
use objc2_foundation::{NSArray, NSDictionary, NSURL};
use objc2_pdf_kit::{PDFDocument, PDFPage};
use objc2_vision::{
    VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel,
};

use crate::{EmbedError, Extracted, Page};

/// Pages with fewer non-space characters than this in their text layer are OCR'd.
const MIN_TEXT_CHARS: usize = 16;
/// Render scale for OCR: 144 dpi.
const OCR_SCALE: f64 = 2.0;
/// Longest side of an OCR render, in pixels.
const OCR_MAX_SIDE: f64 = 4096.0;

pub(crate) fn extract(path: &Path) -> Result<Extracted, EmbedError> {
    let url = NSURL::from_file_path(path)
        .ok_or_else(|| EmbedError::Extract(format!("bad path {}", path.display())))?;
    let doc = unsafe { PDFDocument::initWithURL(PDFDocument::alloc(), &url) }
        .ok_or_else(|| EmbedError::Extract("not a PDF, or it couldn't be opened".into()))?;
    if unsafe { doc.isLocked() } {
        return Err(EmbedError::Extract("the PDF is password-protected".into()));
    }
    let count = unsafe { doc.pageCount() };
    let mut pages = Vec::with_capacity(count);
    for index in 0..count {
        let text = autoreleasepool(|_| -> Result<String, EmbedError> {
            let Some(page) = (unsafe { doc.pageAtIndex(index) }) else {
                return Ok(String::new());
            };
            let layer = unsafe { page.string() }
                .map(|s| s.to_string())
                .unwrap_or_default();
            let layer = layer.replace("\r\n", "\n").replace('\r', "\n");
            if non_space_chars(&layer) >= MIN_TEXT_CHARS {
                return Ok(layer);
            }
            let ocr = ocr_page(&page)?;
            Ok(if non_space_chars(&ocr) > non_space_chars(&layer) {
                ocr
            } else {
                layer
            })
        })?;
        pages.push(Page {
            number: Some(index as u32 + 1),
            text,
        });
    }
    Ok(Extracted { pages })
}

fn non_space_chars(text: &str) -> usize {
    text.chars().filter(|c| !c.is_whitespace()).count()
}

/// Recognized lines of the rendered page, in Vision's reading order.
fn ocr_page(page: &PDFPage) -> Result<String, EmbedError> {
    let Some(image) = render(page) else {
        return Ok(String::new());
    };
    let request = VNRecognizeTextRequest::new();
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
    request.setUsesLanguageCorrection(true);
    request.setAutomaticallyDetectsLanguage(true);
    let handler = unsafe {
        VNImageRequestHandler::initWithCGImage_options(
            VNImageRequestHandler::alloc(),
            &image,
            &NSDictionary::new(),
        )
    };
    let generic: &VNRequest = &request;
    handler
        .performRequests_error(&NSArray::from_slice(&[generic]))
        .map_err(|e| EmbedError::Extract(format!("text recognition failed: {e}")))?;
    let lines: Vec<String> = request
        .results()
        .map(|observations| {
            observations
                .iter()
                .filter_map(|o| o.topCandidates(1).firstObject())
                .map(|text| text.string().to_string())
                .collect()
        })
        .unwrap_or_default();
    Ok(lines.join("\n"))
}

/// The page's crop box drawn on white, grayscale, at `OCR_SCALE` (smaller for huge pages).
fn render(page: &PDFPage) -> Option<objc2_core_foundation::CFRetained<CGImage>> {
    let pdf_page = unsafe { page.pageRef() }?;
    let pdf_page: &CGPDFPage = &pdf_page;
    let rect = CGPDFPage::box_rect(Some(pdf_page), CGPDFBox::CropBox);
    let rotated = CGPDFPage::rotation_angle(Some(pdf_page)).rem_euclid(180) == 90;
    let (width, height) = if rotated {
        (rect.size.height, rect.size.width)
    } else {
        (rect.size.width, rect.size.height)
    };
    if width < 1.0 || height < 1.0 {
        return None;
    }
    let scale = OCR_SCALE.min(OCR_MAX_SIDE / width.max(height));
    let (pixels_w, pixels_h) = (
        (width * scale).round() as usize,
        (height * scale).round() as usize,
    );
    let space = CGColorSpace::new_device_gray()?;
    let context = unsafe {
        CGBitmapContextCreate(
            ptr::null_mut(),
            pixels_w,
            pixels_h,
            8,
            0,
            Some(&space),
            CGImageAlphaInfo::None.0,
        )
    }?;
    let context: &CGContext = &context;
    CGContext::set_gray_fill_color(Some(context), 1.0, 1.0);
    CGContext::fill_rect(
        Some(context),
        CGRect::new(
            CGPoint::new(0.0, 0.0),
            CGSize::new(pixels_w as f64, pixels_h as f64),
        ),
    );
    CGContext::set_interpolation_quality(Some(context), CGInterpolationQuality::High);
    CGContext::scale_ctm(Some(context), scale, scale);
    // Maps the crop box, rotated as the page says, onto the unscaled page rect.
    let transform = CGPDFPage::drawing_transform(
        Some(pdf_page),
        CGPDFBox::CropBox,
        CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(width, height)),
        0,
        true,
    );
    CGContext::concat_ctm(Some(context), transform);
    CGContext::draw_pdf_page(Some(context), Some(pdf_page));
    CGBitmapContextCreateImage(Some(context))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::process::Command;

    use crate::SourceKind;

    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("margin-embed-pdf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Runs a macOS tool, panicking with its output when it fails.
    fn run(cmd: &mut Command) {
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{cmd:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A two-page PDF with a text layer, made by `cupsfilter` from plain text.
    fn text_pdf(dir: &Path) -> PathBuf {
        let txt = dir.join("text.txt");
        let first = "Photosynthesis turns light into chemical energy in chloroplasts.\n";
        std::fs::write(
            &txt,
            format!(
                "{first}{}Second page about mitochondria.\n",
                "\n".repeat(80)
            ),
        )
        .unwrap();
        let pdf = dir.join("text.pdf");
        let out = Command::new("/usr/sbin/cupsfilter")
            .arg(&txt)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "cupsfilter: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::fs::write(&pdf, out.stdout).unwrap();
        pdf
    }

    /// A one-page PDF that is only an image of text: an HTML page rendered to PNG by
    /// Quick Look, then wrapped in a PDF by `sips`.
    fn image_pdf(dir: &Path) -> PathBuf {
        let html = dir.join("scan.html");
        std::fs::write(
            &html,
            "<html><body style='background:white;font:48px Helvetica;padding:40px'>\
             <p>The invoice number is 48213.</p><p>Payment is due in thirty days.</p>\
             </body></html>",
        )
        .unwrap();
        run(Command::new("/usr/bin/qlmanage")
            .args(["-t", "-s", "1200", "-o"])
            .arg(dir)
            .arg(&html));
        let png = dir.join("scan.html.png");
        let pdf = dir.join("scan.pdf");
        run(Command::new("/usr/bin/sips")
            .args(["-s", "format", "pdf"])
            .arg(&png)
            .arg("--out")
            .arg(&pdf));
        pdf
    }

    #[test]
    fn reads_the_text_layer_page_by_page() {
        let pdf = text_pdf(&temp_dir());
        let doc = crate::extract(&pdf, SourceKind::Pdf).unwrap();
        assert!(doc.pages.len() >= 2, "{doc:?}");
        assert_eq!(doc.pages[0].number, Some(1));
        assert!(
            doc.pages[0].text.contains("Photosynthesis turns light"),
            "{doc:?}"
        );
        let rest: String = doc.pages[1..].iter().map(|p| p.text.as_str()).collect();
        assert!(rest.contains("mitochondria"), "{doc:?}");
        assert!(!doc.pages[0].text.contains("mitochondria"));
    }

    #[test]
    fn reads_pages_without_text_with_ocr() {
        let dir = temp_dir();
        let pdf = image_pdf(&dir);
        // The PDF really has no text layer.
        let url = NSURL::from_file_path(&pdf).unwrap();
        let doc = unsafe { PDFDocument::initWithURL(PDFDocument::alloc(), &url) }.unwrap();
        let layer = unsafe { doc.pageAtIndex(0).unwrap().string() }.map(|s| s.to_string());
        assert!(layer.unwrap_or_default().trim().is_empty());

        let doc = crate::extract(&pdf, SourceKind::Pdf).unwrap();
        assert_eq!(doc.pages.len(), 1);
        assert_eq!(doc.pages[0].number, Some(1));
        let text = &doc.pages[0].text;
        assert!(text.contains("invoice number is 48213"), "{text}");
        assert!(text.contains("Payment is due in thirty days"), "{text}");
    }

    #[test]
    fn a_file_that_is_not_a_pdf_is_an_error() {
        let path = temp_dir().join("fake.pdf");
        std::fs::write(&path, "not a pdf").unwrap();
        let err = crate::extract(&path, SourceKind::Pdf).unwrap_err();
        assert!(matches!(err, EmbedError::Extract(_)), "{err:?}");
    }
}
