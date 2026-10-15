//! The ONNX image encoder (`onnx/vision_encoder*.onnx`) and its preprocessing, as
//! `Gemma4ImageProcessor` (Transformers.js `models/gemma4/image_processing_gemma4.js`;
//! transformers `models/gemma4`) prepares pictures:
//!
//! 1. Decode upright, RGB (alpha dropped), and resize with bicubic filtering to the
//!    largest aspect-preserving size whose sides are multiples of 48 px (patch 16 ×
//!    pooling 3) with at most `9 × tokens` patches (`target_size`; the same rule
//!    `vision.rs` checked against Hugging Face for the Core ML encoder).
//! 2. Rescale to [0, 1] (no mean/std normalisation) and cut into 16 × 16 patches, row by
//!    row, each patch's values in (row, column, channel) order: `pixel_values`
//!    `[max_patches, 768]`, zero past the real patches.
//! 3. `pixel_position_ids` `[max_patches, 2]`: (column, row) per patch, (−1, −1) for
//!    padding.
//! 4. The encoder pools 3 × 3 patch groups into soft tokens; the first `patches / 9`
//!    rows of its `image_features` are the picture's, which go into the text graph at
//!    `<|image>` + that many image tokens + `<image|>` (`mod.rs`).
//!
//! The encoder's input and output names are read from the graph when it loads
//! (Transformers.js passes `pixel_values` and `pixel_position_ids` and reads
//! `image_features`); a batch axis is added when the graph has one. The real-model test
//! `real_model_embeds_corpus_pictures_and_speech` (`tests/corpus.rs`) checks them on the real export.
//!
//! On Linux (and macOS with `linux-readers`) the picture is decoded with `image` and
//! resized with Catmull-Rom (bicubic, as PIL's `BICUBIC`); on macOS otherwise with
//! ImageIO, drawn at the target size by Core Graphics.
//!
//! Validation against the Hugging Face processor, as far as it went here (2026-10-14):
//! the resize rule, the patch order, the (column, row) positions with −1 padding, the
//! 1/255 rescale without normalisation and the `patches / 9` token count were read from
//! Transformers.js' `image_processing_gemma4.js` and `modeling_gemma4.js` and are pinned
//! by the tests below (`target_size` on FluidUse's Hugging Face cases). Not yet done: a
//! pixel-for-pixel comparison of `pixel_values` with the Python processor on three
//! images (no PyTorch/transformers with Gemma 4 on this Mac) and the real encoder's
//! input names (no model download in this area); `real_model_embeds_corpus_pictures_and_speech`
//! prints the graphs and checks picture–query ranking on the corpus, and the eval's
//! cross-modal gate (`docs/plan-linux-service.md` §4) is the end-to-end check. `image`'s
//! bicubic resampling rounds differently from PIL's, so `pixel_values` aren't expected
//! to match the Python processor bit for bit (unmeasured).

use std::path::Path;

use half::f16;
use ort::session::Session;
use ort::value::{Tensor, TensorElementType, ValueType};

use super::FEATURE_WIDTH;
use crate::{EmbedError, ImageTokens};

/// Pixels per patch side.
pub(crate) const PATCH: usize = 16;
/// Patches per soft token side (3 × 3 patches pool into one token).
pub(crate) const POOLING: usize = 3;
/// Values per patch: 16 × 16 × RGB.
pub(crate) const PATCH_VALUES: usize = PATCH * PATCH * 3;

/// The largest aspect-preserving size whose sides are multiples of `patch × pooling`
/// and which has at most `patches` patches (`get_aspect_ratio_preserving_size`).
pub(crate) fn target_size(
    width: usize,
    height: usize,
    patches: usize,
    patch: usize,
    pooling: usize,
) -> Result<(usize, usize), EmbedError> {
    if width == 0 || height == 0 {
        return Err(EmbedError::Extract("the image is empty".into()));
    }
    let side = patch * pooling;
    let factor = ((patches * patch * patch) as f64 / (width * height) as f64).sqrt();
    let mut target_w = (factor * width as f64 / side as f64).floor() as usize * side;
    let mut target_h = (factor * height as f64 / side as f64).floor() as usize * side;
    let max_side = (patches / (pooling * pooling)) * side;
    match (target_w, target_h) {
        (0, 0) => {
            return Err(EmbedError::Extract(format!(
                "the image ({width} × {height}) is too small to embed"
            )));
        }
        (_, 0) => {
            target_h = side;
            target_w = ((width / height) * side).min(max_side);
        }
        (0, _) => {
            target_w = side;
            target_h = ((height / width) * side).min(max_side);
        }
        _ => {}
    }
    Ok((target_w, target_h))
}

/// The encoder's inputs for one picture.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Patches {
    /// `total × PATCH_VALUES`, in [0, 1].
    pub values: Vec<f32>,
    /// `total × 2`: (column, row), −1 for padding.
    pub positions: Vec<i64>,
    /// Real patches.
    pub count: usize,
    /// Patch slots, real and padding.
    pub total: usize,
}

impl Patches {
    /// Soft tokens the picture becomes.
    pub fn tokens(&self) -> usize {
        self.count / (POOLING * POOLING)
    }
}

/// Cuts an RGB image (`width × height × 3` bytes, both sides multiples of `PATCH`) into
/// `total` patch slots.
pub(crate) fn patchify(
    rgb: &[u8],
    width: usize,
    height: usize,
    total: usize,
) -> Result<Patches, EmbedError> {
    let columns = width / PATCH;
    let rows = height / PATCH;
    let count = columns * rows;
    if rgb.len() != width * height * 3 {
        return Err(EmbedError::Embed(
            "the picture's pixels don't match its size".into(),
        ));
    }
    if count > total {
        return Err(EmbedError::Embed(format!(
            "the image needs {count} patches; the encoder takes {total}"
        )));
    }
    let mut values = vec![0f32; total * PATCH_VALUES];
    let mut positions = vec![-1i64; total * 2];
    for row in 0..rows {
        for column in 0..columns {
            let index = row * columns + column;
            let base = index * PATCH_VALUES;
            for y in 0..PATCH {
                let src = ((row * PATCH + y) * width + column * PATCH) * 3;
                let dst = base + y * PATCH * 3;
                for (d, s) in values[dst..dst + PATCH * 3]
                    .iter_mut()
                    .zip(&rgb[src..src + PATCH * 3])
                {
                    *d = *s as f32 / 255.0;
                }
            }
            positions[index * 2] = column as i64;
            positions[index * 2 + 1] = row as i64;
        }
    }
    Ok(Patches {
        values,
        positions,
        count,
        total,
    })
}

/// The picture at `path`, upright, resized for `patches` patches: RGB bytes and the size.
#[cfg(any(target_os = "linux", feature = "linux-readers"))]
pub(crate) fn pixels(path: &Path, patches: usize) -> Result<(Vec<u8>, usize, usize), EmbedError> {
    let image = crate::image_linux::load(path)?;
    let (w, h) = target_size(
        image.width() as usize,
        image.height() as usize,
        patches,
        PATCH,
        POOLING,
    )?;
    let rgb = image.to_rgb8();
    let resized = if (rgb.width() as usize, rgb.height() as usize) == (w, h) {
        rgb
    } else {
        image::imageops::resize(
            &rgb,
            w as u32,
            h as u32,
            image::imageops::FilterType::CatmullRom,
        )
    };
    Ok((resized.into_raw(), w, h))
}

/// The picture at `path`, upright, drawn for `patches` patches by ImageIO.
#[cfg(all(target_os = "macos", not(feature = "linux-readers")))]
pub(crate) fn pixels(path: &Path, patches: usize) -> Result<(Vec<u8>, usize, usize), EmbedError> {
    /// Larger pictures are scaled down while decoding; every budget is far smaller.
    const DECODE_MAX_SIDE: usize = 4096;
    let image = crate::image::load(path, Some(DECODE_MAX_SIDE))?;
    let (width, height) = crate::image::size(&image);
    let (w, h) = target_size(width, height, patches, PATCH, POOLING)?;
    let rgba = crate::image::rgba(&image, w, h)?;
    let rgb = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    Ok((rgb, w, h))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", feature = "linux-readers")))]
pub(crate) fn pixels(_path: &Path, _patches: usize) -> Result<(Vec<u8>, usize, usize), EmbedError> {
    Err(EmbedError::Extract(
        "reading images needs macOS or Linux".into(),
    ))
}

/// The encoder: patches in, `patches.tokens()` rows of `FEATURE_WIDTH` out (row-major).
pub(crate) trait VisionGraph: Send {
    /// The patch slots the graph takes for a picture of `tokens` soft tokens: fixed by
    /// the graph, or `9 × tokens`.
    fn total_patches(&self, tokens: usize) -> usize {
        tokens * POOLING * POOLING
    }

    fn encode(&mut self, patches: &Patches) -> Result<Vec<f32>, EmbedError>;
}

/// The encoder rows of the picture at `path` at `tokens` soft tokens: `(rows, count)`.
pub(crate) fn image_rows(
    graph: &mut dyn VisionGraph,
    path: &Path,
    tokens: ImageTokens,
) -> Result<(Vec<f32>, usize), EmbedError> {
    let max_patches = tokens.count() * POOLING * POOLING;
    let (rgb, w, h) = pixels(path, max_patches)?;
    let patches = patchify(
        &rgb,
        w,
        h,
        graph.total_patches(tokens.count()).max(max_patches),
    )?;
    let count = patches.tokens();
    let rows = graph.encode(&patches)?;
    let rows = take_rows(rows, count, "image")?;
    Ok((rows, count))
}

/// The first `count` rows of an encoder's output (it may pad), checked.
pub(crate) fn take_rows(
    mut rows: Vec<f32>,
    count: usize,
    what: &str,
) -> Result<Vec<f32>, EmbedError> {
    if rows.len() < count * FEATURE_WIDTH {
        return Err(EmbedError::Embed(format!(
            "the {what} encoder returned {} rows, expected {count}",
            rows.len() / FEATURE_WIDTH
        )));
    }
    rows.truncate(count * FEATURE_WIDTH);
    if rows.iter().any(|x| !x.is_finite()) {
        return Err(EmbedError::Embed(format!(
            "the {what} encoder returned a malformed row"
        )));
    }
    Ok(rows)
}

/// One of a graph's inputs, as the encoders feed it.
#[derive(Debug, Clone)]
pub(crate) struct Port {
    pub name: String,
    pub ty: TensorElementType,
    /// The declared shape (−1 for dynamic dimensions).
    pub shape: Vec<i64>,
}

impl Port {
    pub(crate) fn of(
        session: &Session,
        pick: impl Fn(&str, TensorElementType, usize) -> bool,
    ) -> Option<Port> {
        session
            .inputs()
            .iter()
            .find_map(|input| match input.dtype() {
                ValueType::Tensor { ty, shape, .. } if pick(input.name(), *ty, shape.len()) => {
                    Some(Port {
                        name: input.name().to_string(),
                        ty: *ty,
                        shape: shape.to_vec(),
                    })
                }
                _ => None,
            })
    }

    /// `dims` with a leading batch axis of 1 when the graph declares one more dimension.
    pub(crate) fn shape_for(&self, dims: &[usize]) -> Vec<usize> {
        if self.shape.len() == dims.len() + 1 {
            std::iter::once(1).chain(dims.iter().copied()).collect()
        } else {
            dims.to_vec()
        }
    }

    /// `values` as a tensor of this input's float type in `dims`.
    pub(crate) fn floats(
        &self,
        dims: &[usize],
        values: Vec<f32>,
    ) -> Result<ort::value::DynValue, EmbedError> {
        let shape = self.shape_for(dims);
        Ok(match self.ty {
            TensorElementType::Float16 => Tensor::from_array((
                shape,
                values.into_iter().map(f16::from_f32).collect::<Vec<f16>>(),
            ))
            .map_err(run_error)?
            .into_dyn(),
            _ => Tensor::from_array((shape, values))
                .map_err(run_error)?
                .into_dyn(),
        })
    }

    /// `values` as a tensor of this input's integer (or bool, or float) type in `dims`.
    pub(crate) fn ints(
        &self,
        dims: &[usize],
        values: Vec<i64>,
    ) -> Result<ort::value::DynValue, EmbedError> {
        let shape = self.shape_for(dims);
        Ok(match self.ty {
            TensorElementType::Int32 => Tensor::from_array((
                shape,
                values.into_iter().map(|v| v as i32).collect::<Vec<i32>>(),
            ))
            .map_err(run_error)?
            .into_dyn(),
            TensorElementType::Bool => Tensor::from_array((
                shape,
                values.into_iter().map(|v| v != 0).collect::<Vec<bool>>(),
            ))
            .map_err(run_error)?
            .into_dyn(),
            TensorElementType::Float32 | TensorElementType::Float16 => {
                return self.floats(dims, values.into_iter().map(|v| v as f32).collect());
            }
            _ => Tensor::from_array((shape, values))
                .map_err(run_error)?
                .into_dyn(),
        })
    }
}

pub(crate) fn run_error(e: impl std::fmt::Display) -> EmbedError {
    EmbedError::Embed(e.to_string())
}

fn is_float(ty: TensorElementType) -> bool {
    matches!(ty, TensorElementType::Float32 | TensorElementType::Float16)
}

/// The name of the output to read: `preferred` when the graph has it, else its first.
pub(crate) fn output_name(session: &Session, preferred: &str) -> Result<String, EmbedError> {
    let outputs = session.outputs();
    outputs
        .iter()
        .find(|o| o.name() == preferred)
        .or(outputs.first())
        .map(|o| o.name().to_string())
        .ok_or_else(|| EmbedError::Load("the encoder has no outputs".into()))
}

/// Runs `session` and reads `output` as floats whose last dimension is `FEATURE_WIDTH`.
pub(crate) fn run_rows(
    session: &mut Session,
    inputs: Vec<(String, ort::session::SessionInputValue<'_>)>,
    output: &str,
) -> Result<Vec<f32>, EmbedError> {
    let outputs = session.run(inputs).map_err(run_error)?;
    let value = outputs
        .get(output)
        .ok_or_else(|| EmbedError::Embed(format!("the encoder gave no {output}")))?;
    let (shape, data) = super::session::read_floats(value)?;
    if shape.last().copied() != Some(FEATURE_WIDTH as i64) {
        return Err(EmbedError::Embed(format!(
            "{output} is {shape:?}, expected rows of {FEATURE_WIDTH}"
        )));
    }
    Ok(data)
}

/// EmbeddingGemma 2's image encoder (`onnx/vision_encoder*.onnx`).
pub(crate) struct OrtVisionGraph {
    session: Session,
    values: Port,
    positions: Port,
    output: String,
}

impl OrtVisionGraph {
    pub(crate) fn load(path: &Path, threads: usize) -> Result<Self, EmbedError> {
        let session = super::session::load(path, threads)?;
        let values = Port::of(&session, |n, ty, _| n == "pixel_values" && is_float(ty))
            .or_else(|| Port::of(&session, |_, ty, _| is_float(ty)))
            .ok_or_else(|| EmbedError::Load("the image encoder has no pixel input".into()))?;
        let positions = Port::of(&session, |n, _, _| {
            n != values.name && n.contains("position")
        })
        .or_else(|| Port::of(&session, |n, ty, _| n != values.name && !is_float(ty)))
        .ok_or_else(|| EmbedError::Load("the image encoder has no patch position input".into()))?;
        if values
            .shape
            .last()
            .is_some_and(|w| *w > 0 && *w as usize != PATCH_VALUES)
        {
            return Err(EmbedError::Load(format!(
                "the image encoder takes patches of {:?}, expected {PATCH_VALUES}",
                values.shape.last()
            )));
        }
        let output = output_name(&session, "image_features")?;
        Ok(Self {
            session,
            values,
            positions,
            output,
        })
    }
}

impl VisionGraph for OrtVisionGraph {
    fn total_patches(&self, tokens: usize) -> usize {
        // A graph exported for a fixed patch count says so in its second-to-last axis.
        let n = self.values.shape.len();
        match n.checked_sub(2).map(|i| self.values.shape[i]) {
            Some(fixed) if fixed > 0 => fixed as usize,
            _ => tokens * POOLING * POOLING,
        }
    }

    fn encode(&mut self, patches: &Patches) -> Result<Vec<f32>, EmbedError> {
        let values = self
            .values
            .floats(&[patches.total, PATCH_VALUES], patches.values.clone())?;
        let positions = self
            .positions
            .ints(&[patches.total, 2], patches.positions.clone())?;
        let inputs = vec![
            (self.values.name.clone(), values.into()),
            (self.positions.name.clone(), positions.into()),
        ];
        run_rows(&mut self.session, inputs, &self.output.clone())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A stand-in encoder: each soft token's row is its 3 × 3 group's mean value in the
    /// first column, then the group's (column, row); rows past the real ones are padding.
    #[derive(Default)]
    pub(crate) struct StubVision {
        pub seen: Vec<(usize, usize)>,
    }

    impl VisionGraph for StubVision {
        fn encode(&mut self, patches: &Patches) -> Result<Vec<f32>, EmbedError> {
            self.seen.push((patches.count, patches.total));
            let slots = patches.total / (POOLING * POOLING);
            let mut out = vec![0f32; slots * FEATURE_WIDTH];
            for t in 0..patches.tokens() {
                let p = t * POOLING * POOLING;
                let mean: f32 = patches.values[p * PATCH_VALUES..(p + 1) * PATCH_VALUES]
                    .iter()
                    .sum::<f32>()
                    / PATCH_VALUES as f32;
                out[t * FEATURE_WIDTH] = mean + 0.01;
                out[t * FEATURE_WIDTH + 1] = t as f32;
            }
            Ok(out)
        }
    }

    /// FluidUse's `testTargetSizesMatchHuggingFace`, plus the error cases.
    #[test]
    fn target_sizes_match_hugging_face() {
        let cases = [
            (500, 334, 280, (960, 624)),
            (334, 500, 70, (288, 480)),
            (1920, 1080, 140, (720, 384)),
            (4000, 100, 70, (2496, 48)),
            (64, 64, 280, (768, 768)),
        ];
        for (w, h, tokens, expected) in cases {
            let size = target_size(w, h, tokens * 9, 16, 3).unwrap();
            assert_eq!(size, expected, "{w}x{h} at {tokens}");
            assert!((size.0 / 16) * (size.1 / 16) <= tokens * 9);
        }
        assert!(target_size(0, 10, 2520, 16, 3).is_err());
    }

    #[test]
    fn patches_are_row_major_with_positions_and_padding() {
        // 96 × 48: 6 × 3 patches. Pixel value = patch index, so each patch is uniform.
        let (w, h) = (96, 48);
        let mut rgb = vec![0u8; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                let index = (y / 16) * 6 + x / 16;
                let at = (y * w + x) * 3;
                rgb[at] = (index * 10) as u8;
                rgb[at + 1] = 255;
                rgb[at + 2] = (x % 16) as u8; // varies within the patch
            }
        }
        let p = patchify(&rgb, w, h, 27).unwrap();
        assert_eq!((p.count, p.total, p.tokens()), (18, 27, 2));
        for index in 0..18 {
            let base = index * PATCH_VALUES;
            assert!((p.values[base] - (index * 10) as f32 / 255.0).abs() < 1e-6);
            assert_eq!(p.values[base + 1], 1.0);
            // Within a patch: (row, column, channel), so the 4th pixel of the first row
            // has blue = 3.
            assert!((p.values[base + 3 * 3 + 2] - 3.0 / 255.0).abs() < 1e-6);
            assert_eq!(
                (p.positions[index * 2], p.positions[index * 2 + 1]),
                ((index % 6) as i64, (index / 6) as i64)
            );
        }
        assert!(p.values[18 * PATCH_VALUES..].iter().all(|v| *v == 0.0));
        assert!(p.positions[36..].iter().all(|v| *v == -1));
        assert!(patchify(&rgb, w, h, 10).is_err());
        assert!(patchify(&rgb[1..], w, h, 27).is_err());
    }

    #[test]
    fn take_rows_checks_counts_and_values() {
        let rows = vec![0.5; 3 * FEATURE_WIDTH];
        assert_eq!(
            take_rows(rows.clone(), 2, "x").unwrap().len(),
            2 * FEATURE_WIDTH
        );
        assert!(take_rows(rows, 4, "x").is_err());
        assert!(take_rows(vec![f32::NAN; FEATURE_WIDTH], 1, "x").is_err());
    }

    #[test]
    fn a_picture_becomes_its_soft_token_rows() {
        let dir = std::env::temp_dir().join(format!("margin-onnx-vision-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("square.png");
        write_test_png(&path, 300, 200);
        let mut stub = StubVision::default();
        let (rows, count) = image_rows(&mut stub, &path, ImageTokens::Low).unwrap();
        // 300 × 200 at 70 tokens (630 patches): 480 × 288 → 30 × 18 = 540 patches, 60
        // tokens.
        assert_eq!(stub.seen, vec![(540, 630)]);
        assert_eq!(count, 60);
        assert_eq!(rows.len(), 60 * FEATURE_WIDTH);
        let bad = dir.join("bad.png");
        std::fs::write(&bad, "nope").unwrap();
        assert!(image_rows(&mut stub, &bad, ImageTokens::Low).is_err());
    }

    /// A gray PNG written without the platform's image libraries (stored zlib blocks).
    pub(crate) fn write_test_png(path: &Path, width: u32, height: u32) {
        fn crc(data: &[u8]) -> u32 {
            let mut c = 0xFFFF_FFFFu32;
            for &b in data {
                c ^= b as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 {
                        0xEDB8_8320 ^ (c >> 1)
                    } else {
                        c >> 1
                    };
                }
            }
            !c
        }
        fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let mut body = kind.to_vec();
            body.extend_from_slice(data);
            out.extend_from_slice(&body);
            out.extend_from_slice(&crc(&body).to_be_bytes());
        }
        let mut raw = Vec::new();
        for y in 0..height {
            raw.push(0); // filter: none
            for x in 0..width {
                raw.push(((x + y) % 256) as u8);
            }
        }
        // zlib: stored blocks of at most 65 535 bytes, then Adler-32.
        let mut z = vec![0x78, 0x01];
        let blocks: Vec<&[u8]> = raw.chunks(65_535).collect();
        for (i, block) in blocks.iter().enumerate() {
            z.push((i + 1 == blocks.len()) as u8);
            let len = block.len() as u16;
            z.extend_from_slice(&len.to_le_bytes());
            z.extend_from_slice(&(!len).to_le_bytes());
            z.extend_from_slice(block);
        }
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in &raw {
            a = (a + byte as u32) % 65_521;
            b = (b + a) % 65_521;
        }
        z.extend_from_slice(&((b << 16) | a).to_be_bytes());
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = width.to_be_bytes().to_vec();
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 0, 0, 0, 0]); // 8-bit gray
        chunk(&mut png, b"IHDR", &ihdr);
        chunk(&mut png, b"IDAT", &z);
        chunk(&mut png, b"IEND", &[]);
        std::fs::write(path, png).unwrap();
    }
}
