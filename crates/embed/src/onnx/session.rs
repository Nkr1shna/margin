//! ONNX Runtime sessions (`ort` 2.0.0-rc.14) on the CPU execution provider, and the text
//! graph behind the small `TextGraph` trait the embedder runs (tests put a stub there).
//!
//! The builder always asks for `AutoDevicePolicy::PreferCPU`: rc.14 otherwise lets ONNX
//! Runtime pick devices by its *MaxEfficiency* policy, which on macOS hands parts of the
//! graph to the CoreML EP, and that EP refuses the empty `[0, 512]` media inputs a text
//! run passes ("has zero elements. This is not supported by the CoreML EP"). Listing the
//! CPU EP alone does not override it (the spike, `docs/plan-linux-service.md` §8).

use std::path::Path;
use std::sync::OnceLock;

use half::f16;
use ort::environment::Environment;
use ort::session::Session;
use ort::session::builder::{AutoDevicePolicy, GraphOptimizationLevel, SessionBuilder};
use ort::value::{DynValue, Tensor, TensorElementType, TensorRef, ValueType};

use crate::EmbedError;

/// The text graph: token ids and their mask in, one embedding per row out.
pub(crate) trait TextGraph: Send {
    /// `ids` and `mask` are `rows × cols`, row-major, right-padded (mask 0 on padding).
    /// Returns `rows` vectors.
    fn run(
        &mut self,
        ids: &[i64],
        mask: &[i64],
        rows: usize,
        cols: usize,
    ) -> Result<Vec<Vec<f32>>, EmbedError>;

    /// One sequence (`ids`, all attended) whose placeholder tokens take `media`'s feature
    /// rows (`vision.rs`, `audio.rs`). Returns its embedding. Graphs without media inputs
    /// refuse.
    fn run_media(&mut self, ids: &[i64], media: MediaRows<'_>) -> Result<Vec<f32>, EmbedError> {
        let _ = (ids, media);
        Err(EmbedError::Embed("this graph takes no media".into()))
    }
}

/// Encoder rows for one of the text graph's media inputs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MediaRows<'a> {
    /// `IMAGE_FEATURES` or `AUDIO_FEATURES`.
    pub input: &'a str,
    /// `count × FEATURE_WIDTH`, row-major.
    pub rows: &'a [f32],
    pub count: usize,
}

/// The text graph's media inputs.
pub(crate) const IMAGE_FEATURES: &str = "image_features";
pub(crate) const AUDIO_FEATURES: &str = "audio_features";

/// One input or output of a graph, as `describe` reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphPort {
    pub name: String,
    /// The element type (`Float32`, `Int64`, …), or the value kind for non-tensors.
    pub dtype: String,
    /// The shape, -1 for a dynamic dimension.
    pub shape: Vec<i64>,
}

/// The process's ONNX Runtime environment: `ort` allows one per process, so it is made
/// once and kept.
pub(crate) fn environment() -> Result<&'static Environment, EmbedError> {
    static ENV: OnceLock<Result<Environment, String>> = OnceLock::new();
    ENV.get_or_init(|| {
        ort::init()
            .with_name("margin-embed")
            .build()
            .map_err(|e| e.to_string())
    })
    .as_ref()
    .map_err(|e| EmbedError::Load(format!("ONNX Runtime: {e}")))
}

fn load_error(e: impl std::fmt::Display) -> EmbedError {
    EmbedError::Load(e.to_string())
}

fn run_error(e: impl std::fmt::Display) -> EmbedError {
    EmbedError::Embed(e.to_string())
}

/// A builder on the CPU EP with `threads` intra-op threads and every graph optimization.
fn builder(threads: usize) -> Result<SessionBuilder, EmbedError> {
    Session::builder(environment()?)
        .map_err(load_error)?
        .with_auto_device(AutoDevicePolicy::PreferCPU)
        .map_err(load_error)?
        .with_execution_providers([ort::ep::CPU::default().build()])
        .map_err(load_error)?
        .with_intra_threads(threads.max(1))
        .map_err(load_error)?
        .with_optimization_level(GraphOptimizationLevel::All)
        .map_err(load_error)
}

/// Loads the graph at `path` (its external data beside it) on the CPU. Blocking.
pub(crate) fn load(path: &Path, threads: usize) -> Result<Session, EmbedError> {
    if !path.is_file() {
        return Err(EmbedError::NotDownloaded);
    }
    builder(threads)?
        .commit_from_file(path)
        .map_err(|e| EmbedError::Load(format!("{}: {e}", path.display())))
}

fn port(name: &str, dtype: &ValueType) -> GraphPort {
    match dtype {
        ValueType::Tensor { ty, shape, .. } => GraphPort {
            name: name.to_string(),
            dtype: format!("{ty:?}"),
            shape: shape.to_vec(),
        },
        other => GraphPort {
            name: name.to_string(),
            dtype: format!("{other}"),
            shape: Vec::new(),
        },
    }
}

/// The inputs and outputs of the graph at `path`, for checking a graph's names and
/// shapes (the encoders' are read this way before their code is written). Loads the
/// graph; blocking.
pub fn describe(path: &Path) -> Result<(Vec<GraphPort>, Vec<GraphPort>), EmbedError> {
    let session = load(path, 1)?;
    let inputs = session
        .inputs()
        .iter()
        .map(|o| port(o.name(), o.dtype()))
        .collect();
    let outputs = session
        .outputs()
        .iter()
        .map(|o| port(o.name(), o.dtype()))
        .collect();
    Ok((inputs, outputs))
}

/// The text graph's token inputs and its pooled, normalised output.
pub(crate) const INPUT_IDS: &str = "input_ids";
pub(crate) const ATTENTION_MASK: &str = "attention_mask";
pub(crate) const SENTENCE_EMBEDDING: &str = "sentence_embedding";

/// An input a text-only run leaves empty: the media feature rows (`image_features`,
/// `video_features`, `audio_features`), `[0, width]` of their element type.
#[derive(Debug, Clone)]
struct EmptyInput {
    name: String,
    ty: TensorElementType,
    width: usize,
}

/// EmbeddingGemma 2's text graph (`onnx/model*.onnx`).
pub(crate) struct OrtTextGraph {
    session: Session,
    empty: Vec<EmptyInput>,
}

impl OrtTextGraph {
    pub(crate) fn load(path: &Path, threads: usize) -> Result<Self, EmbedError> {
        let session = load(path, threads)?;
        let mut empty = Vec::new();
        let mut has = (false, false);
        for input in session.inputs() {
            match input.name() {
                INPUT_IDS => has.0 = true,
                ATTENTION_MASK => has.1 = true,
                name => {
                    let ValueType::Tensor { ty, shape, .. } = input.dtype() else {
                        return Err(EmbedError::Load(format!(
                            "the text graph's input {name} isn't a tensor"
                        )));
                    };
                    if !matches!(ty, TensorElementType::Float32 | TensorElementType::Float16) {
                        return Err(EmbedError::Load(format!(
                            "the text graph's input {name} is {ty:?}, expected floats"
                        )));
                    }
                    let width = shape
                        .last()
                        .copied()
                        .filter(|w| *w > 0)
                        .map_or(super::FEATURE_WIDTH, |w| w as usize);
                    empty.push(EmptyInput {
                        name: name.to_string(),
                        ty: *ty,
                        width,
                    });
                }
            }
        }
        if !(has.0 && has.1) {
            return Err(EmbedError::Load(format!(
                "the text graph has no {INPUT_IDS}/{ATTENTION_MASK} inputs"
            )));
        }
        if !session
            .outputs()
            .iter()
            .any(|o| o.name() == SENTENCE_EMBEDDING)
        {
            return Err(EmbedError::Load(format!(
                "the text graph has no {SENTENCE_EMBEDDING} output"
            )));
        }
        Ok(Self { session, empty })
    }
}

/// Reads a `[rows, DIM]` float output (f32 or f16) into rows.
fn read_rows(value: &DynValue, rows: usize) -> Result<Vec<Vec<f32>>, EmbedError> {
    let (shape, data): (Vec<i64>, Vec<f32>) = match value.try_extract_tensor::<f32>() {
        Ok((shape, data)) => (shape.to_vec(), data.to_vec()),
        Err(_) => {
            let (shape, data) = value.try_extract_tensor::<f16>().map_err(run_error)?;
            (shape.to_vec(), data.iter().map(|x| x.to_f32()).collect())
        }
    };
    let width = shape.last().copied().unwrap_or(0) as usize;
    if shape.len() != 2 || shape[0] as usize != rows || width != crate::DIM {
        return Err(EmbedError::Embed(format!(
            "{SENTENCE_EMBEDDING} is {shape:?}, expected [{rows}, {}]",
            crate::DIM
        )));
    }
    Ok(data.chunks_exact(width).map(<[f32]>::to_vec).collect())
}

/// Reads any float output (f32 or f16) as its shape and values.
pub(crate) fn read_floats(value: &DynValue) -> Result<(Vec<i64>, Vec<f32>), EmbedError> {
    match value.try_extract_tensor::<f32>() {
        Ok((shape, data)) => Ok((shape.to_vec(), data.to_vec())),
        Err(_) => {
            let (shape, data) = value.try_extract_tensor::<f16>().map_err(run_error)?;
            Ok((shape.to_vec(), data.iter().map(|x| x.to_f32()).collect()))
        }
    }
}

impl OrtTextGraph {
    /// Runs `rows × cols` ids with their mask; the media inputs are empty except the one
    /// `media` fills.
    fn run_inputs(
        &mut self,
        ids: &[i64],
        mask: &[i64],
        rows: usize,
        cols: usize,
        media: Option<MediaRows<'_>>,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        if let Some(m) = media {
            let Some(input) = self.empty.iter().find(|e| e.name == m.input) else {
                return Err(EmbedError::Embed(format!(
                    "the text graph has no {} input",
                    m.input
                )));
            };
            if m.rows.len() != m.count * input.width {
                return Err(EmbedError::Embed(format!(
                    "{} has {} values for {} rows of {}",
                    m.input,
                    m.rows.len(),
                    m.count,
                    input.width
                )));
            }
        }
        let mut inputs: Vec<(String, ort::session::SessionInputValue<'_>)> = vec![
            (
                INPUT_IDS.into(),
                TensorRef::from_array_view(([rows, cols], ids))
                    .map_err(run_error)?
                    .into(),
            ),
            (
                ATTENTION_MASK.into(),
                TensorRef::from_array_view(([rows, cols], mask))
                    .map_err(run_error)?
                    .into(),
            ),
        ];
        for e in &self.empty {
            let (count, data): (usize, &[f32]) = match media {
                Some(m) if m.input == e.name => (m.count, m.rows),
                _ => (0, &[]),
            };
            let value = match e.ty {
                TensorElementType::Float16 => Tensor::from_array((
                    [count, e.width],
                    data.iter().map(|x| f16::from_f32(*x)).collect::<Vec<f16>>(),
                ))
                .map_err(run_error)?
                .into_dyn(),
                _ => Tensor::from_array(([count, e.width], data.to_vec()))
                    .map_err(run_error)?
                    .into_dyn(),
            };
            inputs.push((e.name.clone(), value.into()));
        }
        let outputs = self.session.run(inputs).map_err(run_error)?;
        let value = outputs
            .get(SENTENCE_EMBEDDING)
            .ok_or_else(|| EmbedError::Embed(format!("no {SENTENCE_EMBEDDING} output")))?;
        read_rows(value, rows)
    }
}

impl TextGraph for OrtTextGraph {
    fn run(
        &mut self,
        ids: &[i64],
        mask: &[i64],
        rows: usize,
        cols: usize,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.run_inputs(ids, mask, rows, cols, None)
    }

    fn run_media(&mut self, ids: &[i64], media: MediaRows<'_>) -> Result<Vec<f32>, EmbedError> {
        let mask = vec![1i64; ids.len()];
        let mut rows = self.run_inputs(ids, &mask, 1, ids.len(), Some(media))?;
        rows.pop()
            .ok_or_else(|| EmbedError::Embed("the model returned no vector".into()))
    }
}
