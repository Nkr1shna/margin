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
//! The `.mlpackage` is compiled once into `EmbeddingGemma2Text.mlmodelc` beside it; each
//! function is loaded from it with all compute units (`functionName`, macOS 15+).

use std::collections::BTreeMap;
use std::ptr::NonNull;
use std::sync::Mutex;
use std::sync::mpsc;

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

use crate::download::{COMPILED, TEXT_PACKAGE};
use crate::{DIM, EmbedError, Embedder, MAX_TOKENS, ModelFiles, Purpose};

/// The fields of `config.json` the text path uses.
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
    tokenizer: Tokenizer,
    /// `embeddings.bf16`, rows of `hidden` little-endian bf16 values.
    table: memmap2::Mmap,
    rows: usize,
    hidden: usize,
    scale: f32,
    bos: u32,
    eos: u32,
    pad: u32,
    document_prompt: String,
    query_prompt: String,
    functions: Mutex<Functions>,
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

        let compiled = dir.join(COMPILED);
        let functions = match load_functions(&compiled, &sizes) {
            Ok(functions) => functions,
            Err(first) => {
                // A missing or stale compile (e.g. from another macOS version): compile again.
                if compiled.exists() {
                    tracing::info!("recompiling the embedding model: {first}");
                    std::fs::remove_dir_all(&compiled)?;
                }
                compile(&dir.join(TEXT_PACKAGE), &compiled)?;
                load_functions(&compiled, &sizes)?
            }
        };

        Ok(Model {
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

    fn embed_one(&self, text: &str, purpose: Purpose) -> Result<Vec<f32>, EmbedError> {
        let ids = self.token_ids(text, purpose)?;
        let functions = self.functions.lock().unwrap_or_else(|e| e.into_inner());
        let function = functions
            .0
            .iter()
            .find(|f| f.tokens >= ids.len())
            .ok_or_else(|| EmbedError::Embed(format!("{} tokens don't fit", ids.len())))?;
        let length = function.tokens;

        let mut embeds = vec![f16::ZERO; length * self.hidden];
        for (position, row) in embeds.chunks_exact_mut(self.hidden).enumerate() {
            self.write_row(ids.get(position).copied().unwrap_or(self.pad), row);
        }
        let mask: Vec<f16> = (0..length)
            .map(|i| if i < ids.len() { f16::ONE } else { f16::ZERO })
            .collect();

        autoreleasepool(|_| {
            let inputs_embeds = multi_array(&[1, length, self.hidden], &embeds)?;
            let attention_mask = multi_array(&[1, length], &mask)?;
            let keys = [
                &*NSString::from_str("inputs_embeds"),
                &*NSString::from_str("attention_mask"),
            ];
            let values = [
                unsafe { MLFeatureValue::featureValueWithMultiArray(&inputs_embeds) },
                unsafe { MLFeatureValue::featureValueWithMultiArray(&attention_mask) },
            ];
            let values: [&AnyObject; 2] = [&values[0], &values[1]];
            let dictionary = NSDictionary::from_slices(&keys, &values);
            let input = unsafe {
                MLDictionaryFeatureProvider::initWithDictionary_error(
                    MLDictionaryFeatureProvider::alloc(),
                    &dictionary,
                )
            }
            .map_err(|e| EmbedError::Embed(describe(&e)))?;
            let input = ProtocolObject::<dyn MLFeatureProvider>::from_ref(&*input);
            let output = unsafe { function.model.predictionFromFeatures_error(input) }
                .map_err(|e| EmbedError::Embed(describe(&e)))?;
            let embedding = unsafe { output.featureValueForName(&NSString::from_str("embedding")) }
                .and_then(|value| unsafe { value.multiArrayValue() })
                .ok_or_else(|| EmbedError::Embed("the model returned no embedding".into()))?;
            let mut vector = read_vector(&embedding)?;
            normalize(&mut vector);
            Ok(vector)
        })
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
}

/// Loads every `embed_S` function from the compiled model.
fn load_functions(
    compiled: &std::path::Path,
    sizes: &[(usize, &str)],
) -> Result<Vec<EmbedFunction>, EmbedError> {
    if !compiled.exists() {
        return Err(EmbedError::Load("not compiled yet".into()));
    }
    let url = NSURL::from_directory_path(compiled)
        .ok_or_else(|| EmbedError::Load(format!("bad path {}", compiled.display())))?;
    sizes
        .iter()
        .map(|&(tokens, name)| {
            autoreleasepool(|_| {
                let configuration = unsafe { MLModelConfiguration::new() };
                unsafe {
                    configuration.setComputeUnits(MLComputeUnits::All);
                    configuration.setFunctionName(Some(&NSString::from_str(name)));
                }
                let model = unsafe {
                    MLModel::modelWithContentsOfURL_configuration_error(&url, &configuration)
                }
                .map_err(|e| EmbedError::Load(format!("{name}: {}", describe(&e))))?;
                Ok(EmbedFunction { tokens, model })
            })
        })
        .collect()
}

/// Compiles `package` for this Mac and moves the result to `to`.
fn compile(package: &std::path::Path, to: &std::path::Path) -> Result<(), EmbedError> {
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

fn copy_dir(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
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

/// An fp16 `MLMultiArray` of `shape` holding `values` in row-major order.
fn multi_array(shape: &[usize], values: &[f16]) -> Result<Retained<MLMultiArray>, EmbedError> {
    let numbers: Vec<Retained<NSNumber>> = shape
        .iter()
        .map(|&n| NSNumber::new_isize(n as isize))
        .collect();
    let array = unsafe {
        MLMultiArray::initWithShape_dataType_error(
            MLMultiArray::alloc(),
            &NSArray::from_retained_slice(&numbers),
            MLMultiArrayDataType::Float16,
        )
    }
    .map_err(|e| EmbedError::Embed(describe(&e)))?;
    let handler = RcBlock::new(
        |bytes: NonNull<std::ffi::c_void>,
         _size: NSInteger,
         strides: NonNull<NSArray<NSNumber>>| {
            // SAFETY: Core ML passes the array's buffer, of `shape` with these strides
            // (in elements), valid for the call.
            let strides: Vec<usize> = unsafe { strides.as_ref() }
                .iter()
                .map(|n| n.as_isize() as usize)
                .collect();
            let base = bytes.as_ptr().cast::<f16>();
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
    let strides = unsafe { array.strides() };
    let stride = strides.lastObject().map_or(1, |n| n.as_isize() as usize);
    let data_type = unsafe { array.dataType() };
    let out = std::cell::RefCell::new(Err(EmbedError::Embed(format!(
        "unexpected embedding type {data_type:?}"
    ))));
    let handler = RcBlock::new(|bytes: NonNull<std::ffi::c_void>, _size: NSInteger| {
        // SAFETY: the buffer holds `DIM` values of `data_type` at `stride`, valid for the call.
        let values: Option<Vec<f32>> = if data_type == MLMultiArrayDataType::Float16 {
            let base = bytes.as_ptr().cast::<f16>();
            Some(
                (0..DIM)
                    .map(|i| unsafe { *base.add(i * stride) }.to_f32())
                    .collect(),
            )
        } else if data_type == MLMultiArrayDataType::Float32 {
            let base = bytes.as_ptr().cast::<f32>();
            Some((0..DIM).map(|i| unsafe { *base.add(i * stride) }).collect())
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

fn normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        vector.iter_mut().for_each(|x| *x /= norm);
    }
}

fn describe(error: &NSError) -> String {
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
}
