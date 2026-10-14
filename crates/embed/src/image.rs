//! Images with ImageIO and Core Graphics: decoding any format ImageIO reads (HEIC, PNG,
//! JPEG, WebP, GIF's first frame, TIFF, BMP…) upright (its EXIF/TIFF orientation
//! applied), drawing into RGBA pixels at a given size, and writing PNG or JPEG.

use std::path::Path;

use objc2::AnyThread;
use objc2_core_foundation::{
    CFBoolean, CFDictionary, CFMutableData, CFNumber, CFRetained, CFString, CFType, CFURL, CGPoint,
    CGRect, CGSize,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGContext, CGImage,
    CGImageAlphaInfo, CGInterpolationQuality, kCGColorSpaceSRGB,
};
use objc2_foundation::{NSArray, NSDictionary};
use objc2_image_io::{
    CGImageDestination, CGImageSource, kCGImageDestinationLossyCompressionQuality,
    kCGImageSourceCreateThumbnailFromImageAlways, kCGImageSourceCreateThumbnailWithTransform,
    kCGImageSourceThumbnailMaxPixelSize,
};
use objc2_vision::{
    VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel,
};

use crate::EmbedError;

/// Decodes the first image in `path`, rotated and flipped as its orientation says. With
/// `max_side`, an image larger than that is scaled down (keeping its aspect ratio) while
/// decoding.
pub(crate) fn load(
    path: &Path,
    max_side: Option<usize>,
) -> Result<CFRetained<CGImage>, EmbedError> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let url = CFURL::from_file_path(path)
        .ok_or_else(|| EmbedError::Extract(format!("bad path {}", path.display())))?;
    let source = unsafe { CGImageSource::with_url(&url, None) }
        .ok_or_else(|| EmbedError::Extract(format!("couldn't open {name}")))?;
    if unsafe { source.count() } == 0 {
        return Err(EmbedError::Extract(format!(
            "{name} isn't an image ImageIO reads"
        )));
    }
    // A "thumbnail" made from the full image with the orientation transform applied is
    // the upright image; without a size limit it is as big as the image itself.
    let yes: &CFType = CFBoolean::new(true).as_ref();
    let mut keys: Vec<&CFString> = vec![
        unsafe { kCGImageSourceCreateThumbnailFromImageAlways },
        unsafe { kCGImageSourceCreateThumbnailWithTransform },
    ];
    let mut values: Vec<&CFType> = vec![yes, yes];
    let limit = max_side.map(|m| CFNumber::new_isize(m as isize));
    if let Some(limit) = &limit {
        keys.push(unsafe { kCGImageSourceThumbnailMaxPixelSize });
        values.push(limit.as_ref());
    }
    let options = CFDictionary::from_slices(&keys, &values);
    unsafe { source.thumbnail_at_index(0, Some(options.as_opaque())) }
        .or_else(|| unsafe { source.image_at_index(0, None) })
        .ok_or_else(|| EmbedError::Extract(format!("couldn't decode {name}")))
}

/// Size of an image in pixels.
pub(crate) fn size(image: &CGImage) -> (usize, usize) {
    (CGImage::width(Some(image)), CGImage::height(Some(image)))
}

/// The image drawn at `width` × `height` (stretched if the aspect differs) into sRGB,
/// with high-quality interpolation, as RGBA8 rows from the top (alpha byte unused).
pub(crate) fn rgba(image: &CGImage, width: usize, height: usize) -> Result<Vec<u8>, EmbedError> {
    let mut pixels = vec![0u8; width * height * 4];
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
        .ok_or_else(|| EmbedError::Embed("no sRGB color space".into()))?;
    // SAFETY: `pixels` holds `height` rows of `width * 4` bytes and outlives the context.
    let context = unsafe {
        CGBitmapContextCreate(
            pixels.as_mut_ptr().cast(),
            width,
            height,
            8,
            width * 4,
            Some(&space),
            CGImageAlphaInfo::NoneSkipLast.0,
        )
    }
    .ok_or_else(|| EmbedError::Embed(format!("can't draw a {width}x{height} image")))?;
    CGContext::set_interpolation_quality(Some(&context), CGInterpolationQuality::High);
    CGContext::draw_image(
        Some(&context),
        CGRect::new(
            CGPoint::new(0.0, 0.0),
            CGSize::new(width as f64, height as f64),
        ),
        Some(image),
    );
    // Done drawing into `pixels`.
    drop(context);
    Ok(pixels)
}

/// Images are decoded at most this large for text recognition.
pub(crate) const OCR_MAX_SIDE: usize = 4096;

/// The text Vision's recognizer (accurate, with language correction) reads in `image`,
/// one recognized line per line, in its reading order. The image is first drawn on
/// white, so dark text on a transparent background is still read.
pub(crate) fn recognize_text(image: &CGImage) -> Result<String, EmbedError> {
    let (width, height) = size(image);
    if width == 0 || height == 0 {
        return Ok(String::new());
    }
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
        .ok_or_else(|| EmbedError::Extract("no sRGB color space".into()))?;
    let context = unsafe {
        CGBitmapContextCreate(
            std::ptr::null_mut(),
            width,
            height,
            8,
            0,
            Some(&space),
            CGImageAlphaInfo::NoneSkipLast.0,
        )
    }
    .ok_or_else(|| EmbedError::Extract(format!("can't draw a {width}x{height} image")))?;
    let rect = CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(width as f64, height as f64),
    );
    CGContext::set_rgb_fill_color(Some(&context), 1.0, 1.0, 1.0, 1.0);
    CGContext::fill_rect(Some(&context), rect);
    CGContext::draw_image(Some(&context), rect, Some(image));
    let flat = CGBitmapContextCreateImage(Some(&context))
        .ok_or_else(|| EmbedError::Extract("couldn't draw the image".into()))?;
    recognize(&flat)
}

/// Vision text recognition on an image as it is.
pub(crate) fn recognize(image: &CGImage) -> Result<String, EmbedError> {
    let request = VNRecognizeTextRequest::new();
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
    request.setUsesLanguageCorrection(true);
    request.setAutomaticallyDetectsLanguage(true);
    let handler = unsafe {
        VNImageRequestHandler::initWithCGImage_options(
            VNImageRequestHandler::alloc(),
            image,
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Format {
    Png,
    /// With a quality from 0 to 1.
    Jpeg(f64),
}

impl Format {
    fn uti(self) -> &'static str {
        match self {
            Format::Png => "public.png",
            Format::Jpeg(_) => "public.jpeg",
        }
    }
}

fn properties(format: Format) -> Option<CFRetained<CFDictionary<CFString, CFType>>> {
    let Format::Jpeg(quality) = format else {
        return None;
    };
    let quality = CFNumber::new_f64(quality);
    let value: &CFType = quality.as_ref();
    Some(CFDictionary::from_slices(
        &[unsafe { kCGImageDestinationLossyCompressionQuality }],
        &[value],
    ))
}

/// Writes `image` to `path` (replacing it), via a temporary file beside it.
pub(crate) fn write(image: &CGImage, path: &Path, format: Format) -> Result<(), EmbedError> {
    let partial = path.with_extension("part");
    let url = CFURL::from_file_path(&partial)
        .ok_or_else(|| EmbedError::Extract(format!("bad path {}", path.display())))?;
    let uti = CFString::from_static_str(format.uti());
    let destination = unsafe { CGImageDestination::with_url(&url, &uti, 1, None) }
        .ok_or_else(|| EmbedError::Extract(format!("can't write {}", path.display())))?;
    let properties = properties(format);
    unsafe {
        destination.add_image(image, properties.as_deref().map(|p| p.as_opaque()));
    }
    if !unsafe { destination.finalize() } {
        let _ = std::fs::remove_file(&partial);
        return Err(EmbedError::Extract(format!(
            "couldn't write {}",
            path.display()
        )));
    }
    std::fs::rename(&partial, path)?;
    Ok(())
}

/// `image` encoded in memory.
pub(crate) fn encode(image: &CGImage, format: Format) -> Result<Vec<u8>, EmbedError> {
    let data =
        CFMutableData::new(None, 0).ok_or_else(|| EmbedError::Extract("out of memory".into()))?;
    let uti = CFString::from_static_str(format.uti());
    let destination = unsafe { CGImageDestination::with_data(&data, &uti, 1, None) }
        .ok_or_else(|| EmbedError::Extract("can't encode the image".into()))?;
    let properties = properties(format);
    unsafe {
        destination.add_image(image, properties.as_deref().map(|p| p.as_opaque()));
    }
    if !unsafe { destination.finalize() } {
        return Err(EmbedError::Extract("couldn't encode the image".into()));
    }
    Ok(data.to_vec())
}

/// An image file shrunk to at most `max_side` pixels (never enlarged), upright, as JPEG.
pub(crate) fn jpeg(path: &Path, max_side: usize, quality: f64) -> Result<Vec<u8>, EmbedError> {
    let image = load(path, Some(max_side))?;
    encode(&image, Format::Jpeg(quality))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::ptr;

    use super::*;

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("margin-embed-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A `width` × `height` image: left half red, right half blue, drawn with Core
    /// Graphics.
    pub(crate) fn two_colors(width: usize, height: usize) -> CFRetained<CGImage> {
        let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })).unwrap();
        let context = unsafe {
            CGBitmapContextCreate(
                ptr::null_mut(),
                width,
                height,
                8,
                0,
                Some(&space),
                CGImageAlphaInfo::NoneSkipLast.0,
            )
        }
        .unwrap();
        let half = |x: f64| {
            CGRect::new(
                CGPoint::new(x, 0.0),
                CGSize::new(width as f64 / 2.0, height as f64),
            )
        };
        CGContext::set_rgb_fill_color(Some(&context), 1.0, 0.0, 0.0, 1.0);
        CGContext::fill_rect(Some(&context), half(0.0));
        CGContext::set_rgb_fill_color(Some(&context), 0.0, 0.0, 1.0, 1.0);
        CGContext::fill_rect(Some(&context), half(width as f64 / 2.0));
        objc2_core_graphics::CGBitmapContextCreateImage(Some(&context)).unwrap()
    }

    #[test]
    fn writes_reads_and_draws_images() {
        let dir = temp_dir();
        let image = two_colors(64, 32);
        let png = dir.join("two.png");
        write(&image, &png, Format::Png).unwrap();
        let back = load(&png, None).unwrap();
        assert_eq!(size(&back), (64, 32));
        let pixels = rgba(&back, 4, 2).unwrap();
        // Top-left is red, top-right blue.
        assert!(pixels[0] > 200 && pixels[2] < 50, "{:?}", &pixels[..4]);
        assert!(pixels[12] < 50 && pixels[14] > 200, "{:?}", &pixels[12..16]);

        let shrunk = load(&png, Some(16)).unwrap();
        assert_eq!(size(&shrunk), (16, 8));

        let jpeg = jpeg(&png, 32, 0.8).unwrap();
        assert_eq!(&jpeg[..3], &[0xFF, 0xD8, 0xFF]);
        let path = dir.join("two.jpg");
        std::fs::write(&path, &jpeg).unwrap();
        assert_eq!(size(&load(&path, None).unwrap()), (32, 16));
    }

    #[test]
    fn non_images_are_errors() {
        let path = temp_dir().join("not.png");
        std::fs::write(&path, "hello").unwrap();
        assert!(matches!(load(&path, None), Err(EmbedError::Extract(_))));
    }
}
