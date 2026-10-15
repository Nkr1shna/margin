//! Pictures off macOS (`docs/plan-linux-service.md` §2.4), in place of ImageIO:
//!
//! - decoding with `image` 0.25 (PNG, JPEG, WebP, GIF, TIFF, BMP and the rest of its
//!   default formats), turned upright by the EXIF orientation the decoder reports
//!   (`ImageDecoder::orientation`), which is what ImageIO applied implicitly;
//! - HEIC/HEIF (and AVIF, when libheif has a decoder for it) through libheif, with the
//!   `heic` feature on Linux; libheif applies the file's own rotation and mirroring;
//! - shrinking (never enlarging) with a triangle filter, and JPEG/PNG out.
// On macOS these build (the `linux-readers` feature) for their tests; Linux reads with them.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::io::Cursor;
use std::path::Path;

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};

use crate::EmbedError;

/// Longest side an image is read at for OCR (as `image.rs`).
pub(crate) const OCR_MAX_SIDE: u32 = 4096;
/// JPEG quality of `jpeg_for_model` (ImageIO's 0.85).
pub(crate) const MODEL_JPEG_QUALITY: u8 = 85;
/// JPEG quality of video frames (ImageIO's 0.8).
pub(crate) const FRAME_JPEG_QUALITY: u8 = 80;

fn extract_error(what: &str, e: impl std::fmt::Display) -> EmbedError {
    EmbedError::Extract(format!("{what}: {e}"))
}

/// Whether the file is an ISO base media file of the HEIF family (`ftyp` with a HEIF or
/// AVIF brand), which `image` doesn't decode.
pub(crate) fn is_heif(head: &[u8]) -> bool {
    const BRANDS: &[&[u8; 4]] = &[
        b"heic", b"heix", b"hevc", b"hevx", b"heim", b"heis", b"hevm", b"hevs", b"mif1", b"msf1",
        b"avif", b"avis",
    ];
    if head.len() < 12 || &head[4..8] != b"ftyp" {
        return false;
    }
    let size = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
    let end = size.clamp(12, head.len());
    // The major brand, then the compatible brands after the minor version.
    std::iter::once(&head[8..12])
        .chain(
            head[16.min(end)..end]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| &b[..]),
        )
        .any(|brand| BRANDS.iter().any(|b| &b[..] == brand))
}

/// The image at `path`, upright.
pub(crate) fn load(path: &Path) -> Result<DynamicImage, EmbedError> {
    let bytes = std::fs::read(path).map_err(|e| extract_error("couldn't read the image", e))?;
    decode(&bytes)
}

/// Decodes an image file's bytes, upright.
pub(crate) fn decode(bytes: &[u8]) -> Result<DynamicImage, EmbedError> {
    if is_heif(&bytes[..bytes.len().min(64)]) {
        return decode_heif(bytes);
    }
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| extract_error("couldn't read the image", e))?;
    if reader.format().is_none() {
        return Err(EmbedError::Extract("not an image this build reads".into()));
    }
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| extract_error("couldn't read the image", e))?;
    // A broken EXIF block isn't worth refusing the picture over.
    let orientation = decoder.orientation().ok();
    let mut image = DynamicImage::from_decoder(decoder)
        .map_err(|e| extract_error("couldn't read the image", e))?;
    if let Some(orientation) = orientation {
        image.apply_orientation(orientation);
    }
    if image.width() == 0 || image.height() == 0 {
        return Err(EmbedError::Extract("the image is empty".into()));
    }
    Ok(image)
}

#[cfg(all(target_os = "linux", feature = "heic"))]
fn decode_heif(bytes: &[u8]) -> Result<DynamicImage, EmbedError> {
    use libheif_rs::{ColorSpace, HeifContext, LibHeif, RgbChroma};
    let fail = |e: libheif_rs::HeifError| extract_error("couldn't read the HEIF image", e);
    let lib = LibHeif::new();
    let context = HeifContext::read_from_bytes(bytes).map_err(fail)?;
    let handle = context.primary_image_handle().map_err(fail)?;
    // Decoding applies the file's rotation, mirroring and crop.
    let decoded = lib
        .decode(&handle, ColorSpace::Rgb(RgbChroma::Rgb), None)
        .map_err(fail)?;
    let planes = decoded.planes();
    let plane = planes
        .interleaved
        .ok_or_else(|| EmbedError::Extract("the HEIF image has no RGB plane".into()))?;
    let (width, height) = (plane.width, plane.height);
    let row = width as usize * 3;
    let mut pixels = Vec::with_capacity(row * height as usize);
    for y in 0..height as usize {
        let start = y * plane.stride;
        pixels.extend_from_slice(&plane.data[start..start + row]);
    }
    let image = image::RgbImage::from_raw(width, height, pixels)
        .ok_or_else(|| EmbedError::Extract("the HEIF image is malformed".into()))?;
    if width == 0 || height == 0 {
        return Err(EmbedError::Extract("the image is empty".into()));
    }
    Ok(DynamicImage::ImageRgb8(image))
}

#[cfg(not(all(target_os = "linux", feature = "heic")))]
fn decode_heif(_bytes: &[u8]) -> Result<DynamicImage, EmbedError> {
    Err(EmbedError::Extract(
        "reading HEIC/HEIF images needs a build with libheif (the `heic` feature)".into(),
    ))
}

/// `image` shrunk so its longer side is at most `max_side` (never enlarged).
pub(crate) fn shrink(image: DynamicImage, max_side: u32) -> DynamicImage {
    let longest = image.width().max(image.height());
    if longest <= max_side || max_side == 0 {
        return image;
    }
    let scale = max_side as f64 / longest as f64;
    let width = ((image.width() as f64 * scale).round() as u32).max(1);
    let height = ((image.height() as f64 * scale).round() as u32).max(1);
    image.resize_exact(width, height, FilterType::Triangle)
}

/// `image` as JPEG at `quality` (1–100), its alpha dropped.
pub(crate) fn encode_jpeg(image: &DynamicImage, quality: u8) -> Result<Vec<u8>, EmbedError> {
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality)
        .encode_image(&DynamicImage::ImageRgb8(image.to_rgb8()))
        .map_err(|e| extract_error("couldn't write the JPEG", e))?;
    Ok(out)
}

/// `image` as JPEG at `quality` in `path`.
pub(crate) fn write_jpeg(image: &DynamicImage, path: &Path, quality: u8) -> Result<(), EmbedError> {
    std::fs::write(path, encode_jpeg(image, quality)?)?;
    Ok(())
}

/// `image` as PNG in `path` (RGB, or 8-bit gray for gray images).
pub(crate) fn write_png(image: &DynamicImage, path: &Path) -> Result<(), EmbedError> {
    let image = match image {
        DynamicImage::ImageLuma8(_) => image.clone(),
        _ => DynamicImage::ImageRgb8(image.to_rgb8()),
    };
    image
        .save_with_format(path, ImageFormat::Png)
        .map_err(|e| extract_error("couldn't write the PNG", e))
}

/// The image at `path` shrunk to at most `max_side` and upright, as JPEG
/// (`jpeg_for_model`).
pub(crate) fn jpeg(path: &Path, max_side: usize, quality: u8) -> Result<Vec<u8>, EmbedError> {
    let max_side = u32::try_from(max_side).unwrap_or(u32::MAX);
    encode_jpeg(&shrink(load(path)?, max_side), quality)
}

#[cfg(test)]
pub(crate) mod tests {
    use image::{Rgb, RgbImage};

    use super::*;

    pub(crate) fn temp_dir(name: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "margin-embed-linux-{name}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Red on the left half, blue on the right.
    pub(crate) fn two_colors(width: u32, height: u32) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_fn(width, height, |x, _| {
            if x < width / 2 {
                Rgb([255, 0, 0])
            } else {
                Rgb([0, 0, 255])
            }
        }))
    }

    /// A JPEG of `image` with an EXIF APP1 segment saying `orientation`.
    fn jpeg_with_orientation(image: &DynamicImage, orientation: u16) -> Vec<u8> {
        let jpeg = encode_jpeg(image, 95).unwrap();
        // TIFF header (big-endian), one IFD entry: Orientation (0x0112), SHORT, 1.
        let mut tiff = b"MM\0\x2a\0\0\0\x08".to_vec();
        tiff.extend_from_slice(&1u16.to_be_bytes());
        tiff.extend_from_slice(&0x0112u16.to_be_bytes());
        tiff.extend_from_slice(&3u16.to_be_bytes());
        tiff.extend_from_slice(&1u32.to_be_bytes());
        tiff.extend_from_slice(&orientation.to_be_bytes());
        tiff.extend_from_slice(&[0, 0]);
        tiff.extend_from_slice(&0u32.to_be_bytes());
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend_from_slice(&tiff);
        let mut out = jpeg[..2].to_vec(); // SOI
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&app1);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    fn is_red(p: &Rgb<u8>) -> bool {
        p[0] > 180 && p[2] < 80
    }

    fn is_blue(p: &Rgb<u8>) -> bool {
        p[2] > 180 && p[0] < 80
    }

    #[test]
    fn reads_formats_upright() {
        let dir = temp_dir("formats");
        let image = two_colors(96, 48);
        for (format, ext) in [
            (ImageFormat::Png, "png"),
            (ImageFormat::Gif, "gif"),
            (ImageFormat::Tiff, "tiff"),
            (ImageFormat::Bmp, "bmp"),
            (ImageFormat::WebP, "webp"),
        ] {
            let path = dir.join(format!("two.{ext}"));
            DynamicImage::ImageRgba8(image.to_rgba8())
                .save_with_format(&path, format)
                .unwrap();
            let back = load(&path).unwrap().to_rgb8();
            assert_eq!(back.dimensions(), (96, 48), "{ext}");
            assert!(
                is_red(back.get_pixel(2, 2)) && is_blue(back.get_pixel(93, 2)),
                "{ext}"
            );
        }
        // Stored sideways with EXIF orientation 6 (rotate 90° clockwise to view): it
        // comes back tall, red on top.
        let path = dir.join("sideways.jpg");
        std::fs::write(&path, jpeg_with_orientation(&image, 6)).unwrap();
        let upright = load(&path).unwrap().to_rgb8();
        assert_eq!(upright.dimensions(), (48, 96));
        assert!(
            is_red(upright.get_pixel(24, 2)),
            "{:?}",
            upright.get_pixel(24, 2)
        );
        assert!(is_blue(upright.get_pixel(24, 93)));
        // Orientation 1 leaves it as stored.
        std::fs::write(&path, jpeg_with_orientation(&image, 1)).unwrap();
        assert_eq!(load(&path).unwrap().width(), 96);
    }

    #[test]
    fn non_images_and_heif_without_libheif_are_errors() {
        let dir = temp_dir("bad");
        let text = dir.join("fake.png");
        std::fs::write(&text, "not an image").unwrap();
        assert!(matches!(load(&text), Err(EmbedError::Extract(_))));
        assert!(load(&dir.join("missing.png")).is_err());
        // A HEIC header: detected as HEIF, so `image` isn't asked.
        let mut heic = 24u32.to_be_bytes().to_vec();
        heic.extend_from_slice(b"ftypheic\0\0\0\0mif1heic");
        heic.extend_from_slice(&[0; 32]);
        assert!(is_heif(&heic));
        let mut compatible = 24u32.to_be_bytes().to_vec();
        compatible.extend_from_slice(b"ftypmp42\0\0\0\0isommif1");
        assert!(is_heif(&compatible));
        let mut mp4 = 24u32.to_be_bytes().to_vec();
        mp4.extend_from_slice(b"ftypisom\0\0\0\0isomavc1");
        assert!(!is_heif(&mp4));
        assert!(!is_heif(b"\x89PNG\r\n\x1a\n"));
        let path = dir.join("photo.heic");
        std::fs::write(&path, &heic).unwrap();
        assert!(matches!(load(&path), Err(EmbedError::Extract(_))));
    }

    #[test]
    fn shrinks_and_encodes() {
        let dir = temp_dir("jpeg");
        let path = dir.join("big.png");
        write_png(&two_colors(400, 100), &path).unwrap();
        let bytes = jpeg(&path, 200, MODEL_JPEG_QUALITY).unwrap();
        assert_eq!(&bytes[..2], &[0xFF, 0xD8]);
        let back = image::load_from_memory(&bytes).unwrap();
        assert_eq!((back.width(), back.height()), (200, 50));
        // Never enlarged.
        let small = image::load_from_memory(&jpeg(&path, 4000, 85).unwrap()).unwrap();
        assert_eq!(small.width(), 400);
        // PNG keeps gray as gray.
        let gray = DynamicImage::ImageLuma8(image::GrayImage::from_pixel(4, 4, image::Luma([7])));
        let out = dir.join("gray.png");
        write_png(&gray, &out).unwrap();
        assert!(matches!(load(&out).unwrap(), DynamicImage::ImageLuma8(_)));
        write_jpeg(&two_colors(10, 10), &dir.join("x.jpg"), FRAME_JPEG_QUALITY).unwrap();
        assert!(load(&dir.join("x.jpg")).is_ok());
    }
}
