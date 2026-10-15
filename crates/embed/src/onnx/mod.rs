//! EmbeddingGemma 2 on ONNX Runtime (`onnx-community/embeddinggemma-2-ONNX`), CPU
//! execution provider: the service's embedder (`docs/plan-linux-service.md` §2.1).
//!
//! The text graph does the whole model (token table lookup, scaling, pooling,
//! projection) and returns `sentence_embedding` `[batch, 768]`, normalised; its
//! media inputs (`image_features`, `video_features`, `audio_features`, `[n, 512]`) are
//! empty for text. Text inputs are batched for real (`text.rs`). Pictures and sound go
//! through the vision and audio encoders first (`vision.rs`, `audio.rs`, `mel.rs`;
//! `onnx/vision_encoder*`, `onnx/audio_encoder*`), which load on first use and unload
//! after `ENCODER_IDLE` (`IdleSlot`, swept on every call); their feature rows go into the
//! text graph's `image_features`/`audio_features` input at the placeholder tokens of
//! `<bos> <|image> <image>… <image|> <eos>` (or the audio tokens), one item per run.
//!
//! The encoders' input and output names are read from the graphs when they load;
//! `describe_graph` prints any graph's inputs, and the real-model tests in
//! `tests/corpus.rs` (`real_model_embeds_corpus_pictures_and_speech`) runs both encoders
//! end to end.

pub(crate) mod audio;
pub(crate) mod mel;
pub(crate) mod session;
pub(crate) mod text;
pub(crate) mod vision;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokenizers::Tokenizer;

use crate::download::{ONNX_AUDIO_GRAPH, ONNX_TEXT_GRAPH, ONNX_VISION_GRAPH};
use crate::{Backend, DIM, EmbedError, Embedder, ImageTokens, ModelFiles, OnnxOptions, Purpose};
use audio::{AudioGraph, OrtAudioGraph};
use session::{AUDIO_FEATURES, IMAGE_FEATURES, MediaRows, OrtTextGraph, TextGraph};
use text::Tokens;
use vision::{OrtVisionGraph, VisionGraph};

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

    /// Drops the value whenever it was last used (a model being deleted). Returns
    /// whether one was loaded.
    pub fn clear(&self) -> bool {
        self.value.lock().unwrap().take().is_some()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_loaded(&self) -> bool {
        self.value.lock().unwrap().is_some()
    }
}

/// Makes an encoder when it is first needed (again after it was unloaded).
pub(crate) type Loader<T> = Box<dyn Fn() -> Result<T, EmbedError> + Send + Sync>;

type Encoder<T> = IdleSlot<Mutex<Box<T>>>;

pub(crate) struct Model {
    backend: Backend,
    tokens: Tokens,
    graph: Mutex<Box<dyn TextGraph>>,
    batch_tokens: usize,
    vision: Encoder<dyn VisionGraph>,
    audio: Encoder<dyn AudioGraph>,
    load_vision: Loader<Box<dyn VisionGraph>>,
    load_audio: Loader<Box<dyn AudioGraph>>,
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
        if !files.is_complete() {
            return Err(EmbedError::NotDownloaded);
        }
        let tokens = load_tokens(&files.dir)?;
        let path = files.graph_path(ONNX_TEXT_GRAPH);
        let graph = OrtTextGraph::load(&path, opts.threads())?;
        let threads = opts.threads();
        let vision_path = files.graph_path(ONNX_VISION_GRAPH);
        let audio_path = files.graph_path(ONNX_AUDIO_GRAPH);
        Ok(
            Self::from_parts(files.backend, tokens, Box::new(graph), opts).with_encoders(
                Box::new(move || {
                    Ok(Box::new(OrtVisionGraph::load(&vision_path, threads)?)
                        as Box<dyn VisionGraph>)
                }),
                Box::new(move || {
                    Ok(Box::new(OrtAudioGraph::load(&audio_path, threads)?) as Box<dyn AudioGraph>)
                }),
            ),
        )
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
            vision: IdleSlot::new(ENCODER_IDLE),
            audio: IdleSlot::new(ENCODER_IDLE),
            load_vision: Box::new(|| Err(EmbedError::Load("no image encoder".into()))),
            load_audio: Box::new(|| Err(EmbedError::Load("no audio encoder".into()))),
        }
    }

    /// The model with these encoder loaders (the real graphs, or stubs in tests).
    pub(crate) fn with_encoders(
        mut self,
        vision: Loader<Box<dyn VisionGraph>>,
        audio: Loader<Box<dyn AudioGraph>>,
    ) -> Self {
        self.load_vision = vision;
        self.load_audio = audio;
        self
    }

    /// Unloads encoders unused for `ENCODER_IDLE`.
    fn sweep(&self, now: Instant) {
        self.vision.sweep(now);
        self.audio.sweep(now);
    }

    /// The embedding of one media item's encoder rows (`count` of them): `<bos> start`,
    /// `count` placeholder tokens, `end <eos>`, the rows going in at the placeholders.
    fn embed_media(
        &self,
        rows: &[f32],
        count: usize,
        (start, placeholder, end): (u32, u32, u32),
        input: &str,
    ) -> Result<Vec<f32>, EmbedError> {
        let mut ids = Vec::with_capacity(count + 4);
        ids.extend([self.tokens.bos as i64, start as i64]);
        ids.extend(std::iter::repeat_n(placeholder as i64, count));
        ids.extend([end as i64, self.tokens.eos as i64]);
        let mut graph = self
            .graph
            .lock()
            .map_err(|_| EmbedError::Embed("the model failed earlier".into()))?;
        let mut v = graph.run_media(&ids, MediaRows { input, rows, count })?;
        if v.len() != DIM || v.iter().any(|x| !x.is_finite()) {
            return Err(EmbedError::Embed(
                "the model returned a malformed vector".into(),
            ));
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            v.iter_mut().for_each(|x| *x /= norm);
        }
        Ok(v)
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
}

impl Embedder for Model {
    fn embed(&self, texts: &[String], purpose: Purpose) -> Result<Vec<Vec<f32>>, EmbedError> {
        let ids: Vec<Vec<u32>> = texts
            .iter()
            .map(|t| self.tokens.ids(t, &purpose))
            .collect::<Result<_, _>>()?;
        self.sweep(Instant::now());
        self.run(&ids)
    }

    fn count_tokens(&self, text: &str) -> usize {
        self.tokens.count(text)
    }

    fn embed_images(
        &self,
        images: &[PathBuf],
        tokens: ImageTokens,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        if images.is_empty() {
            return Ok(Vec::new());
        }
        let now = Instant::now();
        self.sweep(now);
        let encoder = self
            .vision
            .get(now, || (self.load_vision)().map(Mutex::new))?;
        images
            .iter()
            .map(|path| {
                let (rows, count) = {
                    let mut graph = encoder.lock().map_err(|_| {
                        EmbedError::Embed("the image encoder failed earlier".into())
                    })?;
                    vision::image_rows(graph.as_mut(), path, tokens)?
                };
                self.embed_media(
                    &rows,
                    count,
                    (BOI_TOKEN_ID, IMAGE_TOKEN_ID, EOI_TOKEN_ID),
                    IMAGE_FEATURES,
                )
            })
            .collect()
    }

    fn embed_audio(&self, windows: &[Vec<f32>]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if windows.is_empty() {
            return Ok(Vec::new());
        }
        let now = Instant::now();
        self.sweep(now);
        let encoder = self
            .audio
            .get(now, || (self.load_audio)().map(Mutex::new))?;
        windows
            .iter()
            .map(|window| {
                let (rows, count) = {
                    let mut graph = encoder.lock().map_err(|_| {
                        EmbedError::Embed("the audio encoder failed earlier".into())
                    })?;
                    audio::window_rows(graph.as_mut(), window)?
                };
                self.embed_media(
                    &rows,
                    count,
                    (BOA_TOKEN_ID, AUDIO_TOKEN_ID, EOA_TOKEN_ID),
                    AUDIO_FEATURES,
                )
            })
            .collect()
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
    }

    /// A text graph that takes media: records each media run's ids and rows, and returns
    /// a vector made from them (the first column of each row, then the placeholder
    /// count), so tests can tell what went in.
    /// One media run: its ids, the input its rows went to, and how many rows.
    type MediaRun = (Vec<i64>, String, usize);

    #[derive(Clone, Default)]
    struct MediaGraph {
        runs: Arc<Mutex<Vec<MediaRun>>>,
    }

    impl TextGraph for MediaGraph {
        fn run(
            &mut self,
            _: &[i64],
            _: &[i64],
            rows: usize,
            _: usize,
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(vec![vec![1.0; DIM]; rows])
        }

        fn run_media(&mut self, ids: &[i64], media: MediaRows<'_>) -> Result<Vec<f32>, EmbedError> {
            assert_eq!(media.rows.len(), media.count * FEATURE_WIDTH);
            self.runs
                .lock()
                .unwrap()
                .push((ids.to_vec(), media.input.to_string(), media.count));
            let mut v = vec![0f32; DIM];
            for (i, row) in media.rows.as_chunks::<FEATURE_WIDTH>().0.iter().enumerate() {
                v[i % (DIM - 1)] += row[0] + 1.0;
            }
            v[DIM - 1] = media.count as f32;
            Ok(v)
        }
    }

    #[test]
    fn pictures_and_sound_go_through_their_encoders_into_the_placeholders() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let graph = MediaGraph::default();
        let loads = Arc::new(AtomicUsize::new(0));
        let counted = loads.clone();
        let m = Model::from_parts(
            Backend::Onnx(OnnxDtype::Q8),
            tokens(),
            Box::new(graph.clone()),
            &OnnxOptions::default(),
        )
        .with_encoders(
            Box::new(move || {
                counted.fetch_add(1, Ordering::Relaxed);
                Ok(Box::new(vision::tests::StubVision::default()) as Box<dyn VisionGraph>)
            }),
            Box::new(|| Ok(Box::new(audio::tests::StubAudio::default()) as Box<dyn AudioGraph>)),
        );
        let dir = std::env::temp_dir().join(format!("margin-onnx-media-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (wide, tall) = (dir.join("wide.png"), dir.join("tall.png"));
        vision::tests::write_test_png(&wide, 300, 200);
        vision::tests::write_test_png(&tall, 100, 400);

        let v = m
            .embed_images(&[wide.clone(), tall.clone()], ImageTokens::Low)
            .unwrap();
        assert_eq!(v.len(), 2);
        for x in &v {
            assert!((x.iter().map(|a| a * a).sum::<f32>().sqrt() - 1.0).abs() < 1e-5);
        }
        assert_ne!(v[0], v[1]);
        {
            let runs = graph.runs.lock().unwrap();
            let (ids, input, count) = &runs[0];
            assert_eq!((input.as_str(), *count), (IMAGE_FEATURES, 60));
            assert_eq!(&ids[..2], &[2, BOI_TOKEN_ID as i64]);
            assert!(ids[2..2 + 60].iter().all(|i| *i == IMAGE_TOKEN_ID as i64));
            assert_eq!(&ids[62..], &[EOI_TOKEN_ID as i64, 1]);
        }
        // The encoder stays loaded between calls, and is dropped once idle.
        m.embed_images(std::slice::from_ref(&wide), ImageTokens::Medium)
            .unwrap();
        assert_eq!(loads.load(Ordering::Relaxed), 1);
        assert!(m.vision.is_loaded());
        m.sweep(Instant::now() + ENCODER_IDLE + Duration::from_secs(1));
        assert!(!m.vision.is_loaded());
        m.embed_images(std::slice::from_ref(&wide), ImageTokens::High)
            .unwrap();
        assert_eq!(loads.load(Ordering::Relaxed), 2);
        // A broken picture is an error, not a vector.
        let bad = dir.join("bad.png");
        std::fs::write(&bad, "nope").unwrap();
        assert!(m.embed_images(&[bad], ImageTokens::High).is_err());

        // Sound: one run per window, 25 tokens a second.
        let fixture = mel::tests::fixture();
        let one_second = fixture.samples[..16_000].to_vec();
        let v = m
            .embed_audio(&[fixture.samples.clone(), one_second])
            .unwrap();
        assert_eq!(v.len(), 2);
        let runs = graph.runs.lock().unwrap();
        let audio: Vec<_> = runs.iter().filter(|r| r.1 == AUDIO_FEATURES).collect();
        assert_eq!(audio.iter().map(|r| r.2).collect::<Vec<_>>(), vec![63, 25]);
        let ids = &audio[1].0;
        assert_eq!(&ids[..2], &[2, BOA_TOKEN_ID as i64]);
        assert!(ids[2..27].iter().all(|i| *i == AUDIO_TOKEN_ID as i64));
        assert_eq!(&ids[27..], &[EOA_TOKEN_ID as i64, 1]);
        drop(runs);
        assert!(m.embed_audio(&[vec![0.0; 50]]).is_err());
        assert!(m.embed_audio(&[]).unwrap().is_empty());
    }

    #[test]
    fn a_graph_without_media_inputs_refuses_media() {
        let m = model(StubGraph::default()).with_encoders(
            Box::new(|| Ok(Box::new(vision::tests::StubVision::default()) as Box<dyn VisionGraph>)),
            Box::new(|| Ok(Box::new(audio::tests::StubAudio::default()) as Box<dyn AudioGraph>)),
        );
        let fixture = mel::tests::fixture();
        let err = m.embed_audio(&[fixture.samples]).unwrap_err();
        assert!(err.to_string().contains("takes no media"), "{err}");
    }
}
