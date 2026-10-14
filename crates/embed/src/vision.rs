//! The image encoder, `EmbeddingGemma2Vision.mlpackage`, as FluidUse's
//! `EmbeddingGemma2Vision.swift` runs it (FluidInference/FluidUse, branch
//! `feat/embeddinggemma2-vision`):
//!
//! 1. Decode the image upright (ImageIO) and resize it, keeping its aspect ratio, so both
//!    sides are multiples of 48 px and it has at most 9 × tokens patches of 16 px (Hugging
//!    Face `get_aspect_ratio_preserving_size`). Drawn into sRGB with Core Graphics'
//!    high-quality interpolation.
//! 2. Cut it into 16 × 16 patches row by row, each patch `[16 rows][16 cols][RGB]` scaled
//!    to [0, 1] (the model maps them to [−1, 1]): `patches` [1, N, 768], padded with zeros
//!    to the function's patch count N.
//! 3. `positions` [1, N, 2]: (column, row) of each patch, −1 for padding;
//!    `position_embeddings` [1, N, 768]: `table[0][column] + table[1][row]` from
//!    `position_embeddings.f16` (zero for padding); `valid` [1, N]: 1 for real patches;
//!    `pool` [T, N]: 1/9 for each patch in its 3 × 3 group (groups row-major).
//! 4. Run `vision_T` (T = 70, 140 or 280 tokens) on the CPU and GPU. Its `image_tokens`
//!    [1, T, 512] — the first patches/9 of them — go to the text model wrapped in
//!    `<|image>` … `<image|>` (`coreml::Model::embed_media`).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use half::f16;
use objc2::rc::{Retained, autoreleasepool};
use objc2_core_ml::{MLComputeUnits, MLModel, MLMultiArrayDataType};
use serde::Deserialize;

use crate::coreml::{load_package, multi_array, output_array, predict, read_rows, to_f16};
use crate::{EmbedError, ImageTokens};

/// `config.json`'s `vision` section.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Config {
    package: String,
    functions: BTreeMap<String, FunctionConfig>,
    patch_size: usize,
    pooling_kernel: usize,
    position_table: TableConfig,
    pub(crate) boi_token_id: u32,
    pub(crate) eoi_token_id: u32,
}

#[derive(Debug, Clone, Deserialize)]
struct FunctionConfig {
    tokens: usize,
    patches: usize,
}

#[derive(Debug, Clone, Deserialize)]
struct TableConfig {
    file: String,
    /// [2, rows, hidden].
    shape: [usize; 3],
}

/// Images larger than this are scaled down while decoding; every budget's resized image
/// is far smaller (at most 2,520 patches, e.g. 1,008 × 640 px).
const DECODE_MAX_SIDE: usize = 4096;

struct Function {
    tokens: usize,
    patches: usize,
    model: Retained<MLModel>,
}

pub(crate) struct Vision {
    functions: Mutex<Vec<Function>>,
    /// `position_embeddings.f16`: [2][rows][hidden] fp16.
    table: Vec<f16>,
    table_rows: usize,
    /// Width of a patch row and of a position embedding: 16 × 16 × 3 = 768.
    width: usize,
    patch: usize,
    pooling: usize,
}

// SAFETY: as for the text functions: `MLModel` may be used from any thread, and the
// functions are only reached through the `Mutex`.
unsafe impl Send for Vision {}
unsafe impl Sync for Vision {}

impl Vision {
    pub(crate) fn load(dir: &Path, config: &Config) -> Result<Self, EmbedError> {
        let [axes, table_rows, width] = config.position_table.shape;
        let bytes = std::fs::read(dir.join(&config.position_table.file))?;
        if axes != 2 || bytes.len() != axes * table_rows * width * 2 {
            return Err(EmbedError::Load(format!(
                "{} has an unexpected size",
                config.position_table.file
            )));
        }
        let table: Vec<f16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| f16::from_le_bytes(*pair))
            .collect();
        let patch = config.patch_size;
        if width != patch * patch * 3 {
            return Err(EmbedError::Load(format!(
                "{patch} px patches don't match {width}-wide position embeddings"
            )));
        }
        let mut sizes: Vec<(&str, &FunctionConfig)> = config
            .functions
            .iter()
            .map(|(name, f)| (name.as_str(), f))
            .collect();
        sizes.sort_by_key(|(_, f)| f.tokens);
        let names: Vec<&str> = sizes.iter().map(|(name, _)| *name).collect();
        let models = load_package(dir, &config.package, &names, MLComputeUnits::CPUAndGPU)?;
        let functions = sizes
            .iter()
            .zip(models)
            .map(|((_, f), model)| Function {
                tokens: f.tokens,
                patches: f.patches,
                model,
            })
            .collect();
        Ok(Vision {
            functions: Mutex::new(functions),
            table,
            table_rows,
            width,
            patch,
            pooling: config.pooling_kernel,
        })
    }

    /// The encoder's rows for the image at `path` (patches / 9 rows of 512, fp16).
    pub(crate) fn encode(&self, path: &Path, tokens: ImageTokens) -> Result<Vec<f16>, EmbedError> {
        let image = crate::image::load(path, Some(DECODE_MAX_SIDE))?;
        let (width, height) = crate::image::size(&image);
        if width == 0 || height == 0 {
            return Err(EmbedError::Extract("the image is empty".into()));
        }
        let functions = self.functions.lock().unwrap_or_else(|e| e.into_inner());
        let function = functions
            .iter()
            .find(|f| f.tokens == tokens.count())
            .ok_or_else(|| EmbedError::Load(format!("no vision_{} function", tokens.count())))?;
        let (target_w, target_h) =
            target_size(width, height, function.patches, self.patch, self.pooling);
        let pixels = crate::image::rgba(&image, target_w, target_h)?;
        let inputs = self.inputs(
            &pixels,
            target_w,
            target_h,
            function.tokens,
            function.patches,
        )?;
        let count = inputs.count;
        autoreleasepool(|_| {
            let n = function.patches;
            let patches = multi_array(
                &[1, n, self.width],
                &inputs.patches,
                MLMultiArrayDataType::Float16,
            )?;
            let positions =
                multi_array(&[1, n, 2], &inputs.positions, MLMultiArrayDataType::Float16)?;
            let embeddings = multi_array(
                &[1, n, self.width],
                &inputs.position_embeddings,
                MLMultiArrayDataType::Float16,
            )?;
            let valid = multi_array(&[1, n], &inputs.valid, MLMultiArrayDataType::Float16)?;
            let pool = multi_array(
                &[function.tokens, n],
                &inputs.pool,
                MLMultiArrayDataType::Float16,
            )?;
            let output = predict(
                &function.model,
                &[
                    ("patches", &patches),
                    ("positions", &positions),
                    ("position_embeddings", &embeddings),
                    ("valid", &valid),
                    ("pool", &pool),
                ],
            )?;
            let tokens = output_array(&output, "image_tokens")?;
            let rows = count / (self.pooling * self.pooling);
            let hidden = unsafe { tokens.shape() }
                .lastObject()
                .map_or(0, |n| n.as_isize() as usize);
            Ok(to_f16(&read_rows(&tokens, rows, hidden)?))
        })
    }

    /// The five inputs for an image already drawn at `width` × `height`.
    fn inputs(
        &self,
        pixels: &[u8],
        width: usize,
        height: usize,
        tokens: usize,
        total: usize,
    ) -> Result<Inputs, EmbedError> {
        let p = self.patch;
        let columns = width / p;
        let rows = height / p;
        let count = columns * rows;
        if count > total || columns > self.table_rows || rows > self.table_rows {
            return Err(EmbedError::Embed(format!(
                "the image needs {count} patches; the budget has {total}"
            )));
        }
        let w = self.width;
        let mut inputs = Inputs {
            patches: vec![f16::ZERO; total * w],
            positions: vec![f16::from_f32(-1.0); total * 2],
            position_embeddings: vec![f16::ZERO; total * w],
            valid: vec![f16::ZERO; total],
            pool: vec![f16::ZERO; tokens * total],
            count,
        };
        let ninth = f16::from_f32(1.0 / (self.pooling * self.pooling) as f32);
        for row in 0..rows {
            for column in 0..columns {
                let index = row * columns + column;
                let base = index * w;
                for y in 0..p {
                    for x in 0..p {
                        let pixel = ((row * p + y) * width + column * p + x) * 4;
                        let offset = base + (y * p + x) * 3;
                        for channel in 0..3 {
                            inputs.patches[offset + channel] =
                                f16::from_f32(pixels[pixel + channel] as f32 / 255.0);
                        }
                    }
                }
                inputs.positions[index * 2] = f16::from_f32(column as f32);
                inputs.positions[index * 2 + 1] = f16::from_f32(row as f32);
                inputs.valid[index] = f16::ONE;
                let x_row = column * w;
                let y_row = (self.table_rows + row) * w;
                for channel in 0..w {
                    inputs.position_embeddings[base + channel] = f16::from_f32(
                        self.table[x_row + channel].to_f32() + self.table[y_row + channel].to_f32(),
                    );
                }
                let group = column / self.pooling + (columns / self.pooling) * (row / self.pooling);
                inputs.pool[group * total + index] = ninth;
            }
        }
        Ok(inputs)
    }
}

struct Inputs {
    patches: Vec<f16>,
    positions: Vec<f16>,
    position_embeddings: Vec<f16>,
    valid: Vec<f16>,
    pool: Vec<f16>,
    /// Real patches.
    count: usize,
}

/// The largest aspect-preserving size whose sides are multiples of `patch × pooling`
/// (48 px) and which has at most `patches` patches (FluidUse's `targetSize`, after HF
/// `get_aspect_ratio_preserving_size`).
pub(crate) fn target_size(
    width: usize,
    height: usize,
    patches: usize,
    patch: usize,
    pooling: usize,
) -> (usize, usize) {
    let side = patch * pooling;
    let factor = ((patches * patch * patch) as f64 / (width * height) as f64).sqrt();
    let mut target_w = (factor * width as f64 / side as f64).floor() as usize * side;
    let mut target_h = (factor * height as f64 / side as f64).floor() as usize * side;
    let max_side = (patches / (pooling * pooling)) * side;
    if target_h == 0 {
        target_h = side;
        target_w = ((width / height) * side).min(max_side);
    } else if target_w == 0 {
        target_w = side;
        target_h = ((height / width) * side).min(max_side);
    }
    (target_w, target_h)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FluidUse's `testTargetSizesMatchHuggingFace`.
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
            let size = target_size(w, h, tokens * 9, 16, 3);
            assert_eq!(size, expected, "{w}x{h} at {tokens}");
            assert!((size.0 / 16) * (size.1 / 16) <= tokens * 9);
        }
    }
}
