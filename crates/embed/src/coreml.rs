//! EmbeddingGemma 2 on Core ML, following the model card
//! (huggingface.co/FluidInference/embeddinggemma-2-coreml) and FluidUse's
//! `EmbeddingGemma2Manager.swift`:
//!
//! 1. Prefix the text with the task prompt from `config.json` (`prompts.document` or
//!    `prompts.search_query`).
//! 2. Tokenize with `tokenizer.json` (no special tokens), keep the first 510 tokens, and
//!    wrap them in `<bos>` (2) … `<eos>` (1).
//! 3. Look up each id's row of `embeddings.bf16` (262,144 × 512 bf16, memory-mapped),
//!    widen bf16 to f32 (the 16 bits shifted left by 16), multiply by `embed_scale`
//!    (√512) and store as fp16.
//! 4. Run the smallest `embed_S` function (S = 32, 48, 64, 128, 256, 512) that fits, with
//!    `inputs_embeds` [1, S, 512] (rows past the text are the `<pad>` row) and
//!    `attention_mask` [1, S] (1 for the text's tokens, 0 for padding).
//! 5. Read `embedding` [1, 768] and L2-normalize it.
//!
//! Images and audio (`vision.rs`, `audio.rs`) go through their encoder first, which
//! turns them into rows in the same 512-wide space; those rows are wrapped in
//! `<bos> <|image> … <image|> <eos>` (or `<|audio>` … `<audio|>`), the special tokens
//! looked up as in step 3, and embedded by the same `embed_S` functions
//! (FluidUse's `EmbeddingGemma2Manager.embed(media:count:start:end:)`).
//!
//! Each `.mlpackage` is compiled once into a `.mlmodelc` beside it; each function is
//! loaded from it by name (`functionName`, macOS 15+). The text functions use all
//! compute units; the encoders load on first use, on the CPU and GPU (as the model card
//! and FluidUse recommend: faster than the Neural Engine for them).

use std::collections::BTreeMap;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use half::f16;
use objc2::AnyThread;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2_core_ml::{
    MLComputeUnits, MLDictionaryFeatureProvider, MLFeatureProvider, MLFeatureValue, MLModel,
    MLModelConfiguration, MLMultiArray, MLMultiArrayDataType,
};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSInteger, NSNumber, NSString, NSURL};
use serde::Deserialize;
use tokenizers::Tokenizer;

use crate::download::{TEXT_PACKAGE, compiled_name};
use crate::{DIM, EmbedError, Embedder, ImageTokens, MAX_TOKENS, ModelFiles, Purpose};

/// The fields of `config.json` the model uses.
#[derive(Debug, Deserialize)]
struct Config {
    hidden_size: usize,
    embedding_dim: usize,
    embed_scale: f32,
    bos_token_id: u32,
    eos_token_id: u32,
    pad_token_id: u32,
    functions: BTreeMap<String, Function>,
    prompts: BTreeMap<String, String>,
    vision: crate::vision::Config,
    audio: crate::audio::Config,
}

#[derive(Debug, Deserialize)]
struct Function {
    tokens: usize,
}

/// One loaded `embed_S` function.
struct EmbedFunction {
    tokens: usize,
    model: Retained<MLModel>,
}

/// The loaded functions, smallest first.
struct Functions(Vec<EmbedFunction>);

// SAFETY: `MLModel` is an immutable, reference-counted Objective-C object that may be used
// from any thread; `CoreMlEmbedder` only reaches it through a `Mutex`, so predictions are
// never made concurrently.
unsafe impl Send for Functions {}

pub(crate) struct Model {
    files: ModelFiles,
    tokenizer: Tokenizer,
    /// `embeddings.bf16`, rows of `hidden` little-endian bf16 values.
    table: memmap2::Mmap,
    rows: usize,
    pub(crate) hidden: usize,
    scale: f32,
    bos: u32,
    eos: u32,
    pad: u32,
    document_prompt: String,
    query_prompt: String,
    functions: Mutex<Functions>,
    vision_config: crate::vision::Config,
    audio_config: crate::audio::Config,
    /// The encoders, loaded on first use.
    vision: Mutex<Option<Arc<crate::vision::Vision>>>,
    audio: Mutex<Option<Arc<crate::audio::Audio>>>,
}

impl Model {
    pub(crate) fn load(files: &ModelFiles) -> Result<Self, EmbedError> {
        if !files.is_complete() {
            return Err(EmbedError::NotDownloaded);
        }
        if !objc2::available!(macos = 15.0) {
            return Err(EmbedError::Load(
                "the embedding model needs macOS 15 or later".into(),
            ));
        }
        let dir = &files.dir;
        let config: Config = serde_json::from_slice(&std::fs::read(dir.join("config.json"))?)
            .map_err(|e| EmbedError::Load(format!("config.json: {e}")))?;
        if config.embedding_dim != DIM {
            return Err(EmbedError::Load(format!(
                "the model makes {}-d vectors, expected {DIM}",
                config.embedding_dim
            )));
        }
        let prompt = |name: &str| {
            config
                .prompts
                .get(name)
                .cloned()
                .ok_or_else(|| EmbedError::Load(format!("config.json has no {name} prompt")))
        };
        let document_prompt = prompt("document")?;
        let query_prompt = prompt("search_query")?;

        let tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| EmbedError::Load(format!("tokenizer.json: {e}")))?;

        let table_file = std::fs::File::open(dir.join("embeddings.bf16"))?;
        // SAFETY: the file is only replaced by a new download, which never happens while
        // the service has the model loaded (it drops the embedder first).
        let table = unsafe { memmap2::Mmap::map(&table_file) }?;
        let row_bytes = config.hidden_size * 2;
        if config.hidden_size == 0 || table.len() % row_bytes != 0 {
            return Err(EmbedError::Load(
                "embeddings.bf16 has an unexpected size".into(),
            ));
        }
        let rows = table.len() / row_bytes;

        let mut sizes: Vec<(usize, &str)> = config
            .functions
            .iter()
            .filter(|(name, _)| name.starts_with("embed_"))
            .map(|(name, f)| (f.tokens, name.as_str()))
            .collect();
        sizes.sort();
        if sizes.last().is_none_or(|(tokens, _)| *tokens < MAX_TOKENS) {
            return Err(EmbedError::Load(format!(
                "config.json has no embed_{MAX_TOKENS} function"
            )));
        }
        let names: Vec<&str> = sizes.iter().map(|(_, name)| *name).collect();
        let models = load_package(dir, TEXT_PACKAGE, &names, MLComputeUnits::All)?;
        let functions = sizes
            .iter()
            .zip(models)
            .map(|(&(tokens, _), model)| EmbedFunction { tokens, model })
            .collect();

        Ok(Model {
            files: files.clone(),
            tokenizer,
            table,
            rows,
            hidden: config.hidden_size,
            scale: config.embed_scale,
            bos: config.bos_token_id,
            eos: config.eos_token_id,
            pad: config.pad_token_id,
            document_prompt,
            query_prompt,
            functions: Mutex::new(Functions(functions)),
            vision_config: config.vision,
            audio_config: config.audio,
            vision: Mutex::new(None),
            audio: Mutex::new(None),
        })
    }

    /// `<bos>`, the prefixed text's tokens (at most `MAX_TOKENS - 2`), `<eos>`.
    fn token_ids(&self, text: &str, purpose: Purpose) -> Result<Vec<u32>, EmbedError> {
        let prompt = match purpose {
            Purpose::Document => &self.document_prompt,
            Purpose::Query => &self.query_prompt,
        };
        let encoding = self
            .tokenizer
            .encode(format!("{prompt}{text}"), false)
            .map_err(|e| EmbedError::Embed(format!("tokenizing: {e}")))?;
        let tokens = encoding.get_ids();
        let mut ids = Vec::with_capacity(tokens.len().min(MAX_TOKENS - 2) + 2);
        ids.push(self.bos);
        ids.extend_from_slice(&tokens[..tokens.len().min(MAX_TOKENS - 2)]);
        ids.push(self.eos);
        Ok(ids)
    }

    /// Row `id` of the token table × `scale`, as fp16, into `out`.
    fn write_row(&self, id: u32, out: &mut [f16]) {
        let row = (id as usize).min(self.rows - 1) * self.hidden * 2;
        let bytes = &self.table[row..row + self.hidden * 2];
        for (value, pair) in out.iter_mut().zip(bytes.as_chunks::<2>().0) {
            let bits = u16::from_le_bytes(*pair);
            *value = f16::from_f32(f32::from_bits((bits as u32) << 16) * self.scale);
        }
    }

    /// The table rows of `ids`, one after another.
    fn rows_of(&self, ids: &[u32]) -> Vec<f16> {
        let mut rows = vec![f16::ZERO; ids.len() * self.hidden];
        for (&id, row) in ids.iter().zip(rows.chunks_exact_mut(self.hidden)) {
            self.write_row(id, row);
        }
        rows
    }

    fn embed_one(&self, text: &str, purpose: Purpose) -> Result<Vec<f32>, EmbedError> {
        let ids = self.token_ids(text, purpose)?;
        self.embed_rows(self.rows_of(&ids))
    }

    /// The embedding of a media item's encoder rows (`count` × `hidden`, fp16):
    /// `<bos> start rows end <eos>` through the smallest `embed_S` that fits.
    pub(crate) fn embed_media(
        &self,
        media: &[f16],
        start: u32,
        end: u32,
    ) -> Result<Vec<f32>, EmbedError> {
        let mut rows = self.rows_of(&[self.bos, start]);
        rows.extend_from_slice(media);
        rows.extend(self.rows_of(&[end, self.eos]));
        self.embed_rows(rows)
    }

    /// Runs the smallest `embed_S` that holds `rows` (input rows of `hidden` values, all
    /// attended), padding with the `<pad>` row.
    fn embed_rows(&self, mut rows: Vec<f16>) -> Result<Vec<f32>, EmbedError> {
        let count = rows.len() / self.hidden;
        let functions = self.functions.lock().unwrap_or_else(|e| e.into_inner());
        let function = functions
            .0
            .iter()
            .find(|f| f.tokens >= count)
            .ok_or_else(|| EmbedError::Embed(format!("{count} tokens don't fit")))?;
        let length = function.tokens;
        let pad = self.rows_of(&[self.pad]);
        for _ in count..length {
            rows.extend_from_slice(&pad);
        }
        let mask: Vec<f16> = (0..length)
            .map(|i| if i < count { f16::ONE } else { f16::ZERO })
            .collect();

        autoreleasepool(|_| {
            let inputs_embeds = multi_array(
                &[1, length, self.hidden],
                &rows,
                MLMultiArrayDataType::Float16,
            )?;
            let attention_mask = multi_array(&[1, length], &mask, MLMultiArrayDataType::Float16)?;
            let output = predict(
                &function.model,
                &[
                    ("inputs_embeds", &inputs_embeds),
                    ("attention_mask", &attention_mask),
                ],
            )?;
            let embedding = output_array(&output, "embedding")?;
            let mut vector = read_vector(&embedding)?;
            normalize(&mut vector);
            Ok(vector)
        })
    }

    fn vision(&self) -> Result<Arc<crate::vision::Vision>, EmbedError> {
        let mut slot = self.vision.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(vision) = slot.as_ref() {
            return Ok(vision.clone());
        }
        let vision = Arc::new(crate::vision::Vision::load(
            &self.files.dir,
            &self.vision_config,
        )?);
        *slot = Some(vision.clone());
        Ok(vision)
    }

    fn audio(&self) -> Result<Arc<crate::audio::Audio>, EmbedError> {
        let mut slot = self.audio.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(audio) = slot.as_ref() {
            return Ok(audio.clone());
        }
        let audio = Arc::new(crate::audio::Audio::load(
            &self.files.dir,
            &self.audio_config,
        )?);
        *slot = Some(audio.clone());
        Ok(audio)
    }
}

impl Embedder for Model {
    fn embed(&self, texts: &[String], purpose: Purpose) -> Result<Vec<Vec<f32>>, EmbedError> {
        texts
            .iter()
            .map(|text| self.embed_one(text, purpose))
            .collect()
    }

    fn count_tokens(&self, text: &str) -> usize {
        self.tokenizer
            .encode(text, false)
            .map(|e| e.get_ids().len())
            .unwrap_or(0)
    }

    fn embed_images(
        &self,
        images: &[std::path::PathBuf],
        tokens: ImageTokens,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        if images.is_empty() {
            return Ok(Vec::new());
        }
        let vision = self.vision()?;
        images
            .iter()
            .map(|path| {
                let rows = vision.encode(path, tokens)?;
                self.embed_media(
                    &rows,
                    self.vision_config.boi_token_id,
                    self.vision_config.eoi_token_id,
                )
            })
            .collect()
    }

    fn embed_audio(&self, windows: &[Vec<f32>]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if windows.is_empty() {
            return Ok(Vec::new());
        }
        let audio = self.audio()?;
        windows
            .iter()
            .map(|window| {
                let rows = audio.encode(window)?;
                self.embed_media(
                    &rows,
                    self.audio_config.boa_token_id,
                    self.audio_config.eoa_token_id,
                )
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Core ML helpers, shared with the encoders
// ---------------------------------------------------------------------------

/// Loads `functions` (or the package's only function, for an empty list) from `package`
/// in `dir`, compiling it first when there's no usable compiled copy.
pub(crate) fn load_package(
    dir: &Path,
    package: &str,
    functions: &[&str],
    units: MLComputeUnits,
) -> Result<Vec<Retained<MLModel>>, EmbedError> {
    let compiled = dir.join(compiled_name(package));
    match load_functions(&compiled, functions, units) {
        Ok(models) => Ok(models),
        Err(first) => {
            // A missing or stale compile (e.g. from another macOS version): compile again.
            if compiled.exists() {
                tracing::info!("recompiling {package}: {first}");
                std::fs::remove_dir_all(&compiled)?;
            }
            compile(&dir.join(package), &compiled)?;
            load_functions(&compiled, functions, units)
        }
    }
}

fn load_functions(
    compiled: &Path,
    functions: &[&str],
    units: MLComputeUnits,
) -> Result<Vec<Retained<MLModel>>, EmbedError> {
    if !compiled.exists() {
        return Err(EmbedError::Load("not compiled yet".into()));
    }
    let url = NSURL::from_directory_path(compiled)
        .ok_or_else(|| EmbedError::Load(format!("bad path {}", compiled.display())))?;
    let load = |name: Option<&str>| {
        autoreleasepool(|_| {
            let configuration = unsafe { MLModelConfiguration::new() };
            unsafe {
                configuration.setComputeUnits(units);
                if let Some(name) = name {
                    configuration.setFunctionName(Some(&NSString::from_str(name)));
                }
            }
            unsafe { MLModel::modelWithContentsOfURL_configuration_error(&url, &configuration) }
                .map_err(|e| {
                    EmbedError::Load(format!("{}: {}", name.unwrap_or("model"), describe(&e)))
                })
        })
    };
    if functions.is_empty() {
        return Ok(vec![load(None)?]);
    }
    functions.iter().map(|&name| load(Some(name))).collect()
}

/// Compiles `package` for this Mac and moves the result to `to`.
fn compile(package: &Path, to: &Path) -> Result<(), EmbedError> {
    let url = NSURL::from_directory_path(package)
        .ok_or_else(|| EmbedError::Load(format!("bad path {}", package.display())))?;
    let (send, receive) = mpsc::channel();
    let handler = RcBlock::new(move |compiled: *mut NSURL, error: *mut NSError| {
        // SAFETY: Core ML passes either a valid URL or a valid error, alive for the call.
        let result = match (unsafe { compiled.as_ref() }, unsafe { error.as_ref() }) {
            (Some(url), _) => url
                .to_file_path()
                .ok_or_else(|| "no compiled path".to_string()),
            (None, Some(error)) => Err(describe(error)),
            (None, None) => Err("unknown error".to_string()),
        };
        let _ = send.send(result);
    });
    unsafe { MLModel::compileModelAtURL_completionHandler(&url, &handler) };
    let temporary = receive
        .recv()
        .map_err(|_| EmbedError::Load("the compiler never answered".into()))?
        .map_err(|e| EmbedError::Load(format!("compiling: {e}")))?;

    // The compiler writes to a temporary folder, possibly on another volume.
    let staged = to.with_extension("mlmodelc.part");
    if staged.exists() {
        std::fs::remove_dir_all(&staged)?;
    }
    if std::fs::rename(&temporary, &staged).is_err() {
        copy_dir(&temporary, &staged)?;
        let _ = std::fs::remove_dir_all(&temporary);
    }
    std::fs::rename(&staged, to)?;
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// One prediction with the named multi-array inputs.
pub(crate) fn predict(
    model: &MLModel,
    inputs: &[(&str, &MLMultiArray)],
) -> Result<Retained<ProtocolObject<dyn MLFeatureProvider>>, EmbedError> {
    let keys: Vec<Retained<NSString>> = inputs.iter().map(|(k, _)| NSString::from_str(k)).collect();
    let keys: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let values: Vec<Retained<MLFeatureValue>> = inputs
        .iter()
        .map(|(_, v)| unsafe { MLFeatureValue::featureValueWithMultiArray(v) })
        .collect();
    let values: Vec<&AnyObject> = values
        .iter()
        .map(|v| AsRef::<AnyObject>::as_ref(&**v))
        .collect();
    let dictionary = NSDictionary::from_slices(&keys, &values);
    let input = unsafe {
        MLDictionaryFeatureProvider::initWithDictionary_error(
            MLDictionaryFeatureProvider::alloc(),
            &dictionary,
        )
    }
    .map_err(|e| EmbedError::Embed(describe(&e)))?;
    let input = ProtocolObject::<dyn MLFeatureProvider>::from_ref(&*input);
    unsafe { model.predictionFromFeatures_error(input) }
        .map_err(|e| EmbedError::Embed(describe(&e)))
}

/// The multi-array output called `name`.
pub(crate) fn output_array(
    output: &ProtocolObject<dyn MLFeatureProvider>,
    name: &str,
) -> Result<Retained<MLMultiArray>, EmbedError> {
    unsafe { output.featureValueForName(&NSString::from_str(name)) }
        .and_then(|value| unsafe { value.multiArrayValue() })
        .ok_or_else(|| EmbedError::Embed(format!("the model returned no {name}")))
}

/// An `MLMultiArray` of `shape` and `data_type` (Float16 for `f16` values, Float32 for
/// `f32`) holding `values` in row-major order.
pub(crate) fn multi_array<T: Copy>(
    shape: &[usize],
    values: &[T],
    data_type: MLMultiArrayDataType,
) -> Result<Retained<MLMultiArray>, EmbedError> {
    debug_assert_eq!(values.len(), shape.iter().product::<usize>());
    let numbers: Vec<Retained<NSNumber>> = shape
        .iter()
        .map(|&n| NSNumber::new_isize(n as isize))
        .collect();
    let array = unsafe {
        MLMultiArray::initWithShape_dataType_error(
            MLMultiArray::alloc(),
            &NSArray::from_retained_slice(&numbers),
            data_type,
        )
    }
    .map_err(|e| EmbedError::Embed(describe(&e)))?;
    let handler = RcBlock::new(
        |bytes: NonNull<std::ffi::c_void>,
         _size: NSInteger,
         strides: NonNull<NSArray<NSNumber>>| {
            // SAFETY: Core ML passes the array's buffer, of `shape` with these strides
            // (in elements of `data_type`, which matches `T`), valid for the call.
            let strides: Vec<usize> = unsafe { strides.as_ref() }
                .iter()
                .map(|n| n.as_isize() as usize)
                .collect();
            let base = bytes.as_ptr().cast::<T>();
            let contiguous = strides.last() == Some(&1)
                && (0..shape.len().saturating_sub(1))
                    .all(|axis| strides[axis] == strides[axis + 1] * shape[axis + 1]);
            if contiguous {
                unsafe { std::ptr::copy_nonoverlapping(values.as_ptr(), base, values.len()) };
                return;
            }
            let mut index = vec![0usize; shape.len()];
            for &value in values {
                let offset: usize = index.iter().zip(&strides).map(|(i, s)| i * s).sum();
                unsafe { base.add(offset).write(value) };
                for axis in (0..shape.len()).rev() {
                    index[axis] += 1;
                    if index[axis] < shape[axis] {
                        break;
                    }
                    index[axis] = 0;
                }
            }
        },
    );
    unsafe { array.getMutableBytesWithHandler(&handler) };
    Ok(array)
}

/// The model's output (`[1, 768]` or `[768]`, fp16 or fp32) as f32.
fn read_vector(array: &MLMultiArray) -> Result<Vec<f32>, EmbedError> {
    let count = unsafe { array.count() } as usize;
    if count != DIM {
        return Err(EmbedError::Embed(format!(
            "the embedding has {count} values, expected {DIM}"
        )));
    }
    let mut rows = read_rows(array, 1, DIM)?;
    Ok(rows.pop().unwrap_or_default())
}

/// The first `rows` × `cols` values of an output whose last two axes are rows and
/// columns (`[…, R, C]`, fp16 or fp32), as f32 rows.
pub(crate) fn read_rows(
    array: &MLMultiArray,
    rows: usize,
    cols: usize,
) -> Result<Vec<Vec<f32>>, EmbedError> {
    let shape: Vec<usize> = unsafe { array.shape() }
        .iter()
        .map(|n| n.as_isize() as usize)
        .collect();
    let strides: Vec<usize> = unsafe { array.strides() }
        .iter()
        .map(|n| n.as_isize() as usize)
        .collect();
    let (shape_rows, shape_cols) = match shape.as_slice() {
        [.., r, c] => (*r, *c),
        [c] => (1, *c),
        _ => (0, 0),
    };
    if shape_rows < rows || shape_cols < cols {
        return Err(EmbedError::Embed(format!(
            "the model returned {shape:?}, expected at least {rows} x {cols}"
        )));
    }
    let col_stride = strides.last().copied().unwrap_or(1);
    let row_stride = if strides.len() >= 2 {
        strides[strides.len() - 2]
    } else {
        0
    };
    let data_type = unsafe { array.dataType() };
    let out = std::cell::RefCell::new(Err(EmbedError::Embed(format!(
        "unexpected output type {data_type:?}"
    ))));
    let handler = RcBlock::new(|bytes: NonNull<std::ffi::c_void>, _size: NSInteger| {
        // SAFETY: the buffer holds the array's values of `data_type` at these strides,
        // valid for the call; `rows` and `cols` are within its shape.
        let at = |r: usize, c: usize| r * row_stride + c * col_stride;
        let values: Option<Vec<Vec<f32>>> = if data_type == MLMultiArrayDataType::Float16 {
            let base = bytes.as_ptr().cast::<f16>();
            Some(
                (0..rows)
                    .map(|r| {
                        (0..cols)
                            .map(|c| unsafe { *base.add(at(r, c)) }.to_f32())
                            .collect()
                    })
                    .collect(),
            )
        } else if data_type == MLMultiArrayDataType::Float32 {
            let base = bytes.as_ptr().cast::<f32>();
            Some(
                (0..rows)
                    .map(|r| (0..cols).map(|c| unsafe { *base.add(at(r, c)) }).collect())
                    .collect(),
            )
        } else {
            None
        };
        if let Some(values) = values {
            *out.borrow_mut() = Ok(values);
        }
    });
    unsafe { array.getBytesWithHandler(&handler) };
    drop(handler);
    out.into_inner()
}

/// Rows of f32 as one fp16 buffer.
pub(crate) fn to_f16(rows: &[Vec<f32>]) -> Vec<f16> {
    rows.iter().flatten().map(|&x| f16::from_f32(x)).collect()
}

fn normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        vector.iter_mut().for_each(|x| *x /= norm);
    }
}

pub(crate) fn describe(error: &NSError) -> String {
    error.localizedDescription().to_string()
}

#[cfg(test)]
mod tests {
    //! Against the real model, when it is on this Mac: `MARGIN_EMBED_MODEL_DIR`, or
    //! `~/Library/Caches/margin-embed-test/model`. Skipped (with a message) otherwise.

    use std::path::PathBuf;

    use super::*;
    use crate::cosine;

    fn model_dir() -> Option<PathBuf> {
        let dir = match std::env::var_os("MARGIN_EMBED_MODEL_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => PathBuf::from(std::env::var_os("HOME")?)
                .join("Library/Caches/margin-embed-test/model"),
        };
        if ModelFiles::new(&dir).is_complete() {
            Some(dir)
        } else {
            eprintln!(
                "skipping: no downloaded model in {} (set MARGIN_EMBED_MODEL_DIR)",
                dir.display()
            );
            None
        }
    }

    /// One model per test binary: loading compiles for the Neural Engine.
    fn model() -> Option<&'static crate::CoreMlEmbedder> {
        static MODEL: std::sync::OnceLock<Option<crate::CoreMlEmbedder>> =
            std::sync::OnceLock::new();
        MODEL
            .get_or_init(|| {
                let dir = model_dir()?;
                Some(crate::CoreMlEmbedder::load(&ModelFiles::new(dir)).unwrap())
            })
            .as_ref()
    }

    fn embed(model: &crate::CoreMlEmbedder, texts: &[&str], purpose: Purpose) -> Vec<Vec<f32>> {
        let texts: Vec<String> = texts.iter().map(|t| t.to_string()).collect();
        model.embed(&texts, purpose).unwrap()
    }

    #[test]
    fn real_model_vectors_are_unit_length_and_repeatable() {
        let Some(model) = model() else { return };
        let texts = ["Hello, world.", "", "Ünïcödé ﬁ ligature and ① circled 東京"];
        let first = embed(model, &texts, Purpose::Document);
        let again = embed(model, &texts, Purpose::Document);
        for (a, b) in first.iter().zip(&again) {
            assert_eq!(a.len(), DIM);
            let norm = a.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-4, "norm {norm}");
            assert!(a.iter().all(|x| x.is_finite()));
            assert_eq!(a, b);
        }
        // The prefix matters: the same text as a query is a different vector.
        let query = embed(model, &texts[..1], Purpose::Query);
        assert!(cosine(&first[0], &query[0]) < 0.999);
    }

    #[test]
    fn real_model_ranks_the_relevant_passage_first() {
        let Some(model) = model() else { return };
        let passages = [
            "The stock market fell sharply on Tuesday as investors worried about interest rates.",
            "Photosynthesis is the process by which green plants use sunlight, water and carbon \
             dioxide to make glucose and release oxygen.",
            "To bake sourdough bread, feed the starter the night before and proof the dough slowly.",
            "The Treaty of Westphalia ended the Thirty Years' War in 1648.",
        ];
        let docs = embed(model, &passages, Purpose::Document);
        let query = &embed(
            model,
            &["How do plants turn light into food?"],
            Purpose::Query,
        )[0];
        let scores: Vec<f32> = docs.iter().map(|d| cosine(query, d)).collect();
        eprintln!("scores: {scores:?}");
        let best = (0..scores.len())
            .max_by(|&a, &b| scores[a].total_cmp(&scores[b]))
            .unwrap();
        assert_eq!(best, 1, "{scores:?}");
        assert!(
            scores[1]
                > scores
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != 1)
                    .map(|(_, s)| *s)
                    .fold(f32::MIN, f32::max)
                    + 0.05
        );
    }

    /// Text whose ids (prefix, `<bos>` and `<eos>` included) number more than `above`
    /// and at most `limit`.
    fn text_with_tokens(model: &crate::CoreMlEmbedder, above: usize, limit: usize) -> String {
        let words = [
            "mitochondria",
            "make",
            "energy",
            "for",
            "the",
            "cell",
            "and",
            "its",
            "work",
        ];
        let text = |n: usize| {
            words
                .iter()
                .cycle()
                .take(n)
                .copied()
                .collect::<Vec<_>>()
                .join(" ")
        };
        let ids = |n: usize| {
            model
                .0
                .token_ids(&text(n), Purpose::Document)
                .unwrap()
                .len()
        };
        // The most words that fit, by bisection (tokenizing is slow in debug builds).
        let (mut fits, mut over) = (0, 1000);
        while over - fits > 1 {
            let middle = (fits + over) / 2;
            if ids(middle) <= limit {
                fits = middle
            } else {
                over = middle
            }
        }
        assert!(ids(fits) > above);
        text(fits)
    }

    #[test]
    fn real_model_handles_every_length() {
        let Some(model) = model() else { return };
        let sizes = [32, 48, 64, 128, 256, 512];
        let texts: Vec<String> = sizes
            .iter()
            .enumerate()
            .map(|(i, &s)| text_with_tokens(model, if i == 0 { 0 } else { sizes[i - 1] }, s))
            .collect();
        for (text, &size) in texts.iter().zip(&sizes) {
            let ids = model.0.token_ids(text, Purpose::Document).unwrap();
            let functions = model.0.functions.lock().unwrap();
            assert_eq!(
                functions
                    .0
                    .iter()
                    .find(|f| f.tokens >= ids.len())
                    .unwrap()
                    .tokens,
                size
            );
        }
        let vectors = model.embed(&texts, Purpose::Document).unwrap();
        for (v, size) in vectors.iter().zip(sizes) {
            assert_eq!(v.len(), DIM);
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-4, "embed_{size}: norm {norm}");
        }
        // The same words at every size stay close to each other.
        for v in &vectors[1..] {
            assert!(cosine(&vectors[0], v) > 0.6, "{}", cosine(&vectors[0], v));
        }
        // Past 512 tokens only the start counts: a different ending changes nothing.
        let long = "Mitochondria are the powerhouse of the cell and make ATP. ".repeat(60);
        let changed = format!("{long} Completely different ending about volcanoes.");
        let pair = model.embed(&[long, changed], Purpose::Document).unwrap();
        assert_eq!(pair[0], pair[1]);
    }

    #[test]
    fn real_tokenizer_matches_fluiduse_cases() {
        let Some(model) = model() else { return };
        // From FluidUse's Tests/FluidUseTests/Fixtures/embeddinggemma2-tokenizer-cases.json.
        let cases: [(&str, &[u32]); 4] = [
            (
                "title: none | text: Café crème, naïve résumé",
                &[
                    2, 3250, 236787, 7293, 1109, 1816, 236787, 62367, 100479, 236764, 120362,
                    118515, 1,
                ],
            ),
            (
                "  leading and   multiple   spaces  ",
                &[2, 138, 26016, 532, 139, 43819, 139, 35220, 138, 1],
            ),
            (
                "line one\nline two\n\nline four",
                &[2, 1257, 886, 107, 1257, 1156, 108, 1257, 2390, 1],
            ),
            (
                "tabs\tand\ttabs",
                &[2, 39218, 255968, 624, 255968, 39218, 1],
            ),
        ];
        for (text, ids) in cases {
            let encoding = model.0.tokenizer.encode(text, false).unwrap();
            let mut got = vec![2];
            got.extend_from_slice(encoding.get_ids());
            got.push(1);
            assert_eq!(got, ids, "{text:?}");
            assert_eq!(model.count_tokens(text), ids.len() - 2);
        }
        assert_eq!(
            model
                .0
                .token_ids("Café crème, naïve résumé", Purpose::Document)
                .unwrap(),
            cases[0].1
        );
    }

    /// Compares with vectors from `SentenceTransformer("google/embeddinggemma-2")` when
    /// `MARGIN_EMBED_REFERENCE` names a JSON file of `{"text", "purpose", "vector"}`.
    #[test]
    fn real_model_matches_reference_vectors() {
        let Some(model) = model() else { return };
        let Some(path) = std::env::var_os("MARGIN_EMBED_REFERENCE") else {
            eprintln!("skipping: MARGIN_EMBED_REFERENCE is not set");
            return;
        };
        #[derive(Deserialize)]
        struct Reference {
            text: String,
            purpose: String,
            vector: Vec<f32>,
        }
        let references: Vec<Reference> =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert!(!references.is_empty());
        for r in references {
            let purpose = if r.purpose == "query" {
                Purpose::Query
            } else {
                Purpose::Document
            };
            let ours = &model.embed(std::slice::from_ref(&r.text), purpose).unwrap()[0];
            let similarity = cosine(ours, &r.vector);
            eprintln!(
                "{similarity:.5}  {:?}",
                r.text.chars().take(50).collect::<String>()
            );
            assert!(similarity > 0.99, "{similarity} for {:?}", r.text);
        }
    }

    /// Test pictures and speech: `MARGIN_EMBED_MEDIA_DIR`, or
    /// `~/Library/Caches/margin-embed-test/media` (made by its `make.sh`: `diagram.png`, a
    /// red circle and a blue square with a caption; `diagram-wide.png`, a crop of it;
    /// `coast.jpg`, a photo of a rocky coast; `speech.wav`, `say` reading a sentence about
    /// mitochondria, 16 kHz mono).
    fn media_dir() -> Option<PathBuf> {
        let dir = match std::env::var_os("MARGIN_EMBED_MEDIA_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => PathBuf::from(std::env::var_os("HOME")?)
                .join("Library/Caches/margin-embed-test/media"),
        };
        if dir.join("diagram.png").exists() && dir.join("speech.wav").exists() {
            Some(dir)
        } else {
            eprintln!("skipping: no test media in {}", dir.display());
            None
        }
    }

    fn unit(v: &[f32]) -> bool {
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.len() == DIM && (norm - 1.0).abs() < 1e-3 && v.iter().all(|x| x.is_finite())
    }

    #[test]
    fn real_model_embeds_images_into_the_text_space() {
        let Some(model) = model() else { return };
        let Some(media) = media_dir() else { return };
        let images = [media.join("diagram.png"), media.join("coast.jpg")];
        let vectors = model.embed_images(&images, ImageTokens::High).unwrap();
        assert!(vectors.iter().all(|v| unit(v)));
        assert_eq!(
            vectors,
            model.embed_images(&images, ImageTokens::High).unwrap()
        );
        let queries = embed(
            model,
            &[
                "a red circle and a blue square",
                "a rocky coastline by the sea",
            ],
            Purpose::Query,
        );
        let scores: Vec<Vec<f32>> = queries
            .iter()
            .map(|q| vectors.iter().map(|v| cosine(q, v)).collect())
            .collect();
        eprintln!("query x image cosines: {scores:?}");
        assert!(scores[0][0] > scores[0][1], "{scores:?}");
        assert!(scores[1][1] > scores[1][0], "{scores:?}");
        // Every budget works; fewer tokens stay close to the default.
        for tokens in [ImageTokens::Low, ImageTokens::Medium] {
            let v = &model.embed_images(&images[..1], tokens).unwrap()[0];
            assert!(unit(v));
            let similarity = cosine(v, &vectors[0]);
            eprintln!("{tokens:?} vs High: {similarity:.4}");
            assert!(similarity > 0.8, "{tokens:?}: {similarity}");
        }
        assert!(matches!(
            model.embed_images(&[media.join("speech.wav")], ImageTokens::High),
            Err(EmbedError::Extract(_))
        ));
    }

    #[test]
    fn real_model_embeds_audio_into_the_text_space() {
        let Some(model) = model() else { return };
        let Some(media) = media_dir() else { return };
        let samples = crate::av::sound(&media.join("speech.wav"))
            .unwrap()
            .unwrap();
        assert!(
            (samples.len() as i64 - 81_927).abs() < 10,
            "{}",
            samples.len()
        );
        let vectors = model
            .embed_audio(&[samples.clone(), samples[..16_000].to_vec()])
            .unwrap();
        assert!(vectors.iter().all(|v| unit(v)));
        let queries = embed(
            model,
            &[
                "the mitochondria is the powerhouse of the cell",
                "the stock market fell on Tuesday",
            ],
            Purpose::Query,
        );
        let scores: Vec<f32> = queries.iter().map(|q| cosine(q, &vectors[0])).collect();
        eprintln!("query x speech cosines: {scores:?}");
        assert!(scores[0] > scores[1], "{scores:?}");
        // Too short for a single token.
        assert!(model.embed_audio(&[vec![0.0; 100]]).is_err());
    }

    /// Compares images and audio with `SentenceTransformer("google/embeddinggemma-2")`
    /// when `MARGIN_EMBED_MEDIA_REFERENCE` names the JSON `reference_media.py` writes
    /// (`[{"file", "kind", "vector"}]`, files in the media folder).
    #[test]
    fn real_model_matches_reference_media_vectors() {
        let Some(model) = model() else { return };
        let Some(media) = media_dir() else { return };
        let Some(path) = std::env::var_os("MARGIN_EMBED_MEDIA_REFERENCE") else {
            eprintln!("skipping: MARGIN_EMBED_MEDIA_REFERENCE is not set");
            return;
        };
        #[derive(Deserialize)]
        struct Reference {
            file: String,
            kind: String,
            vector: Vec<f32>,
        }
        let references: Vec<Reference> =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let mut compared = 0;
        for r in references {
            let ours = match r.kind.as_str() {
                "image" => model
                    .embed_images(&[media.join(&r.file)], ImageTokens::High)
                    .unwrap()
                    .remove(0),
                "audio" => {
                    let samples = crate::av::sound(&media.join(&r.file)).unwrap().unwrap();
                    model.embed_audio(&[samples]).unwrap().remove(0)
                }
                "query" => embed(model, &[r.file.as_str()], Purpose::Query).remove(0),
                _ => continue,
            };
            let similarity = cosine(&ours, &r.vector);
            eprintln!("{similarity:.5}  {} {}", r.kind, r.file);
            assert!(similarity > 0.99, "{similarity} for {}", r.file);
            compared += 1;
        }
        assert!(compared >= 4);
    }
}
