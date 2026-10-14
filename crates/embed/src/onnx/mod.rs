//! EmbeddingGemma 2 on ONNX Runtime (`onnx-community/embeddinggemma-2-ONNX`), CPU
//! execution provider: the Linux service's embedder, also buildable on macOS (the `onnx`
//! feature) for the eval A/B (`docs/plan-linux-service.md` §2.1).
//!
//! The text graph does what `coreml.rs` does by hand (token table lookup, scaling,
//! pooling, projection) and returns `sentence_embedding` `[batch, 768]`, normalised; its
//! media inputs (`image_features`, `video_features`, `audio_features`, `[n, 512]`) are
//! empty for text. Text inputs are batched for real (`text.rs`). The vision and audio
//! encoders (`onnx/vision_encoder*`, `onnx/audio_encoder*`) are the media area's: they
//! load on first use and unload after `ENCODER_IDLE` (`IdleSlot`), and their feature rows
//! go in at the placeholder tokens below.
//!
//! The encoders' input names were not dumped yet (the spike loaded the text graph only).
//! `describe_graph` prints any graph's inputs; the real-model test
//! `real_encoder_graphs_are_described` (`tests/onnx_text.rs`) runs it on both encoders.

pub(crate) mod session;
pub(crate) mod text;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokenizers::Tokenizer;

use crate::download::ONNX_TEXT_GRAPH;
use crate::{Backend, DIM, EmbedError, Embedder, ImageTokens, ModelFiles, OnnxOptions, Purpose};
use session::{OrtTextGraph, TextGraph};
use text::Tokens;

pub use session::{GraphPort, describe as describe_graph};

/// Width of the feature rows the encoders produce and the text graph takes.
pub(crate) const FEATURE_WIDTH: usize = 512;
/// An encoder unused this long is dropped (its session and arena freed).
pub const ENCODER_IDLE: Duration = Duration::from_secs(10 * 60);

/// Special tokens of the media wrapping (`config.json`; `<bos> <|image> … <image|> <eos>`).
pub const BOI_TOKEN_ID: u32 = 255_999;
pub const IMAGE_TOKEN_ID: u32 = 258_880;
pub const EOI_TOKEN_ID: u32 = 258_882;
pub const BOA_TOKEN_ID: u32 = 256_000;
pub const AUDIO_TOKEN_ID: u32 = 258_881;
pub const EOA_TOKEN_ID: u32 = 258_883;

/// The fields of the export's `config.json` the text path reads. The ids sit at the top
/// level or under `text_config` depending on the export; Gemma's are the defaults.
#[derive(Debug, Default, Deserialize)]
struct Config {
    bos_token_id: Option<u32>,
    eos_token_id: Option<serde_json::Value>,
    pad_token_id: Option<u32>,
    #[serde(default)]
    text_config: Option<Box<Config>>,
}

/// `eos_token_id` may be a list; the first is the one appended.
fn first_id(v: &Option<serde_json::Value>) -> Option<u32> {
    match v.as_ref()? {
        serde_json::Value::Number(n) => n.as_u64().map(|n| n as u32),
        serde_json::Value::Array(a) => a.first()?.as_u64().map(|n| n as u32),
        _ => None,
    }
}

impl Config {
    fn ids(&self) -> (u32, u32, u32) {
        let inner = self.text_config.as_deref();
        let bos = self.bos_token_id.or(inner.and_then(|c| c.bos_token_id));
        let eos = first_id(&self.eos_token_id).or(inner.and_then(|c| first_id(&c.eos_token_id)));
        let pad = self.pad_token_id.or(inner.and_then(|c| c.pad_token_id));
        (bos.unwrap_or(2), eos.unwrap_or(1), pad.unwrap_or(0))
    }
}

/// A value loaded on first use and dropped once unused for `idle`: the encoders' slot.
pub(crate) struct IdleSlot<T> {
    idle: Duration,
    value: Mutex<Option<(Arc<T>, Instant)>>,
}

#[cfg_attr(not(test), allow(dead_code))] // The media area wires the encoders to it.
impl<T> IdleSlot<T> {
    pub fn new(idle: Duration) -> Self {
        Self {
            idle,
            value: Mutex::new(None),
        }
    }

    /// The value, loading it with `load` if it isn't loaded; marks it used at `now`.
    pub fn get(
        &self,
        now: Instant,
        load: impl FnOnce() -> Result<T, EmbedError>,
    ) -> Result<Arc<T>, EmbedError> {
        let mut slot = self.value.lock().unwrap();
        if let Some((value, used)) = slot.as_mut() {
            *used = now;
            return Ok(value.clone());
        }
        let value = Arc::new(load()?);
        *slot = Some((value.clone(), now));
        Ok(value)
    }

    /// Drops the value if it was last used `idle` or more before `now`. Returns whether
    /// it dropped it. A caller still holding an `Arc` keeps it alive until done.
    pub fn sweep(&self, now: Instant) -> bool {
        let mut slot = self.value.lock().unwrap();
        let stale = slot
            .as_ref()
            .is_some_and(|(_, used)| now.saturating_duration_since(*used) >= self.idle);
        if stale {
            *slot = None;
        }
        stale
    }

    pub fn is_loaded(&self) -> bool {
        self.value.lock().unwrap().is_some()
    }
}

pub(crate) struct Model {
    backend: Backend,
    tokens: Tokens,
    graph: Mutex<Box<dyn TextGraph>>,
    batch_tokens: usize,
}

fn load_tokens(dir: &Path) -> Result<Tokens, EmbedError> {
    let config: Config = match std::fs::read(dir.join("config.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| EmbedError::Load(format!("config.json: {e}")))?,
        Err(e) => return Err(EmbedError::Load(format!("config.json: {e}"))),
    };
    let (bos, eos, pad) = config.ids();
    let tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
        .map_err(|e| EmbedError::Load(format!("tokenizer.json: {e}")))?;
    Ok(Tokens {
        tokenizer,
        bos,
        eos,
        pad,
    })
}

impl Model {
    pub(crate) fn load(files: &ModelFiles, opts: &OnnxOptions) -> Result<Self, EmbedError> {
        let Backend::Onnx(_) = files.backend else {
            return Err(EmbedError::Load(format!(
                "{} isn't an ONNX download",
                files.dir.display()
            )));
        };
        if !files.is_complete() {
            return Err(EmbedError::NotDownloaded);
        }
        let tokens = load_tokens(&files.dir)?;
        let path = files
            .graph_path(ONNX_TEXT_GRAPH)
            .ok_or(EmbedError::NotDownloaded)?;
        let graph = OrtTextGraph::load(&path, opts.threads())?;
        Ok(Self::from_parts(
            files.backend,
            tokens,
            Box::new(graph),
            opts,
        ))
    }

    pub(crate) fn from_parts(
        backend: Backend,
        tokens: Tokens,
        graph: Box<dyn TextGraph>,
        opts: &OnnxOptions,
    ) -> Self {
        Self {
            backend,
            tokens,
            graph: Mutex::new(graph),
            batch_tokens: opts.batch_tokens(),
        }
    }

    fn run(&self, ids: &[Vec<u32>]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut graph = self
            .graph
            .lock()
            .map_err(|_| EmbedError::Embed("the model failed earlier".into()))?;
        text::run_batched(graph.as_mut(), ids, self.batch_tokens, self.tokens.pad)
    }

    fn not_yet(what: &str) -> EmbedError {
        EmbedError::Embed(format!(
            "embedding {what} with the ONNX model isn't available in this build yet"
        ))
    }
}

impl Embedder for Model {
    fn embed(&self, texts: &[String], purpose: Purpose) -> Result<Vec<Vec<f32>>, EmbedError> {
        let ids: Vec<Vec<u32>> = texts
            .iter()
            .map(|t| self.tokens.ids(t, &purpose))
            .collect::<Result<_, _>>()?;
        self.run(&ids)
    }

    fn count_tokens(&self, text: &str) -> usize {
        self.tokens.count(text)
    }

    fn embed_images(
        &self,
        images: &[PathBuf],
        _tokens: ImageTokens,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        if images.is_empty() {
            return Ok(Vec::new());
        }
        Err(Self::not_yet("pictures"))
    }

    fn embed_audio(&self, windows: &[Vec<f32>]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if windows.is_empty() {
            return Ok(Vec::new());
        }
        Err(Self::not_yet("sound"))
    }

    fn embed_pipelined(
        &self,
        inputs: &[(String, Purpose)],
        batch: usize,
        on_batch: &mut dyn FnMut(usize, Vec<Vec<f32>>) -> Result<(), EmbedError>,
    ) -> Result<(), EmbedError> {
        text::pipelined_groups(
            inputs,
            batch,
            &self.tokens,
            &mut |ids| self.run(ids),
            on_batch,
        )
    }

    fn version(&self) -> &str {
        self.backend.embedder_version()
    }
}

/// `DIM` is what the graph's projection makes; checked by `session::read_rows`.
const _: () = assert!(DIM == 768);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::OnnxDtype;
    use text::tests::{StubGraph, tokens};

    fn model(graph: StubGraph) -> Model {
        Model::from_parts(
            Backend::Onnx(OnnxDtype::Q8),
            tokens(),
            Box::new(graph),
            &OnnxOptions {
                threads: 1,
                batch_tokens: 600,
            },
        )
    }

    #[test]
    fn the_embedder_batches_keeps_order_and_names_its_version() {
        let graph = StubGraph::default();
        let m = model(graph.clone());
        let texts: Vec<String> = ["plants light", &"heat engine ".repeat(100), "fourier"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let v = m.embed(&texts, Purpose::DOCUMENT).unwrap();
        assert_eq!(v.len(), 3);
        let alone = m.embed(&texts[2..], Purpose::DOCUMENT).unwrap();
        assert_eq!(v[2], alone[0]);
        let q = m.embed(&texts[2..], Purpose::Query).unwrap();
        assert_ne!(q[0], alone[0]);
        assert!(m.embed(&[], Purpose::Query).unwrap().is_empty());
        assert_eq!(m.count_tokens("plants light sugar"), 3);
        assert_eq!(m.version(), "embeddinggemma-2-onnx-q8/title-prefix-v1");
        assert!(m.embed_images(&[], ImageTokens::High).unwrap().is_empty());
        assert!(
            m.embed_images(&[PathBuf::from("a.png")], ImageTokens::High)
                .is_err()
        );
        assert!(m.embed_audio(&[vec![0.0; 10]]).is_err());

        // Pipelined gives what `embed` gives, `batch` at a time.
        let inputs: Vec<(String, Purpose)> = texts
            .iter()
            .map(|t| (t.clone(), Purpose::DOCUMENT))
            .collect();
        let mut got = Vec::new();
        m.embed_pipelined(&inputs, 2, &mut |first, vs| {
            got.push((first, vs));
            Ok(())
        })
        .unwrap();
        assert_eq!(got.iter().map(|(f, _)| *f).collect::<Vec<_>>(), vec![0, 2]);
        let flat: Vec<Vec<f32>> = got.into_iter().flat_map(|(_, v)| v).collect();
        assert_eq!(flat, v);
    }

    #[test]
    fn config_ids_come_from_the_top_or_the_text_config() {
        let c: Config =
            serde_json::from_str(r#"{"bos_token_id": 5, "eos_token_id": [6, 7]}"#).unwrap();
        assert_eq!(c.ids(), (5, 6, 0));
        let c: Config = serde_json::from_str(
            r#"{"text_config": {"bos_token_id": 2, "eos_token_id": 1, "pad_token_id": 0}}"#,
        )
        .unwrap();
        assert_eq!(c.ids(), (2, 1, 0));
        let c: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(c.ids(), (2, 1, 0));
    }

    #[test]
    fn idle_slots_load_once_and_unload_when_idle() {
        let slot: IdleSlot<u32> = IdleSlot::new(ENCODER_IDLE);
        let t0 = Instant::now();
        let mut loads = 0;
        let a = slot
            .get(t0, || {
                loads += 1;
                Ok(7)
            })
            .unwrap();
        let b = slot
            .get(t0 + Duration::from_secs(60), || unreachable!())
            .unwrap();
        assert_eq!((*a, *b, loads), (7, 7, 1));
        // Used at +60 s: not idle at +10 min, idle at +11 min.
        assert!(!slot.sweep(t0 + ENCODER_IDLE));
        assert!(slot.sweep(t0 + ENCODER_IDLE + Duration::from_secs(60)));
        assert!(!slot.is_loaded());
        assert!(!slot.sweep(t0 + ENCODER_IDLE * 3));
        // A failed load leaves it empty.
        assert!(slot.get(t0, || Err(EmbedError::Load("x".into()))).is_err());
        assert!(!slot.is_loaded());
    }

    #[test]
    fn loading_needs_a_complete_onnx_download() {
        let dir = std::env::temp_dir().join(format!("margin-onnx-empty-{}", std::process::id()));
        let opts = OnnxOptions::default();
        let files = ModelFiles::with_backend(&dir, Backend::Onnx(OnnxDtype::Q8));
        assert!(matches!(
            Model::load(&files, &opts),
            Err(EmbedError::NotDownloaded)
        ));
        let core_ml = ModelFiles::new(&dir);
        assert!(matches!(
            Model::load(&core_ml, &opts),
            Err(EmbedError::Load(_))
        ));
        assert_eq!(core_ml.graph_path("model"), None);
    }
}
