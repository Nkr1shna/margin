//! A cross-encoder reranker on ONNX Runtime, CPU execution provider (R2,
//! `docs/r2-brief.md` D5): `ms-marco-MiniLM-L6-v2` (default) or `bge-reranker-base`,
//! int8 (`RerankerModel`, the Xenova ONNX exports pinned by revision in `download.rs`).
//!
//! `score` tokenises each (query, passage) pair with the model's own tokenizer and its
//! pair template (`[CLS] query [SEP] passage [SEP]` for MiniLM, `<s> query </s></s>
//! passage </s>` for bge), truncated to `routes::RERANK_PAIR_TOKENS` from the passage
//! only (the query kept whole), runs them shortest first in batches of `RERANK_BATCH`
//! and returns the logit per pair in the input's order (higher = more relevant; both
//! models emit one logit).
//!
//! The tokenizer loads with `OnnxReranker::load`; the graph loads on the first `score`
//! into an `IdleSlot` like the vision encoder and is dropped once unused for
//! `ENCODER_IDLE` (a background sweeper checks every minute; `sweep` does it now), so an
//! idle service holds only the tokenizer.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use margin_shared::routes::RERANK_PAIR_TOKENS;
use ort::session::Session;
use ort::value::{TensorRef, ValueType};
use tokenizers::{Encoding, Tokenizer, TruncationParams, TruncationStrategy};

use super::session::{ATTENTION_MASK, INPUT_IDS, load as load_session, read_floats};
use super::{ENCODER_IDLE, IdleSlot, Loader};
use crate::{EmbedError, OnnxOptions, Reranker, RerankerFiles, RerankerModel};

/// Pairs per run of the graph.
pub const RERANK_BATCH: usize = 8;

/// How often the background sweeper looks for an idle graph.
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// The cross-encoder graph: `rows × cols` token ids, their mask (0 on padding) and their
/// segment ids in, one logit per row out.
pub(crate) trait PairGraph: Send {
    fn run(
        &mut self,
        ids: &[i64],
        mask: &[i64],
        types: &[i64],
        rows: usize,
        cols: usize,
    ) -> Result<Vec<f32>, EmbedError>;
}

/// The segment-id input BERT-style graphs take (bge's XLM-R graph has none).
const TOKEN_TYPE_IDS: &str = "token_type_ids";

/// A cross-encoder export (`onnx/model_quantized.onnx`) on ONNX Runtime.
struct OrtPairGraph {
    session: Session,
    /// Whether the graph takes `token_type_ids`.
    types: bool,
    /// Its one output, `logits` `[rows, 1]`.
    output: String,
}

impl OrtPairGraph {
    fn load(path: &Path, threads: usize) -> Result<Self, EmbedError> {
        let session = load_session(path, threads)?;
        let mut has = (false, false, false);
        for input in session.inputs() {
            match input.name() {
                INPUT_IDS => has.0 = true,
                ATTENTION_MASK => has.1 = true,
                TOKEN_TYPE_IDS => has.2 = true,
                other => {
                    return Err(EmbedError::Load(format!(
                        "the reranker graph takes an unexpected input {other}"
                    )));
                }
            }
        }
        if !(has.0 && has.1) {
            return Err(EmbedError::Load(format!(
                "the reranker graph has no {INPUT_IDS}/{ATTENTION_MASK} inputs"
            )));
        }
        let output = match session.outputs() {
            [only] if matches!(only.dtype(), ValueType::Tensor { .. }) => only.name().to_string(),
            outputs => {
                return Err(EmbedError::Load(format!(
                    "the reranker graph has {} outputs, expected one tensor of logits",
                    outputs.len()
                )));
            }
        };
        Ok(Self {
            session,
            types: has.2,
            output,
        })
    }
}

fn run_error(e: impl std::fmt::Display) -> EmbedError {
    EmbedError::Embed(format!("reranking: {e}"))
}

impl PairGraph for OrtPairGraph {
    fn run(
        &mut self,
        ids: &[i64],
        mask: &[i64],
        types: &[i64],
        rows: usize,
        cols: usize,
    ) -> Result<Vec<f32>, EmbedError> {
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
        if self.types {
            inputs.push((
                TOKEN_TYPE_IDS.into(),
                TensorRef::from_array_view(([rows, cols], types))
                    .map_err(run_error)?
                    .into(),
            ));
        }
        let outputs = self.session.run(inputs).map_err(run_error)?;
        let value = outputs
            .get(&self.output)
            .ok_or_else(|| run_error(format!("no {} output", self.output)))?;
        let (shape, logits) = read_floats(value)?;
        let one_per_row = match shape.as_slice() {
            [r] => *r as usize == rows,
            [r, 1] => *r as usize == rows,
            _ => false,
        };
        if !one_per_row {
            return Err(run_error(format!(
                "{} is {shape:?}, expected [{rows}, 1]",
                self.output
            )));
        }
        Ok(logits)
    }
}

/// The fields of the export's `config.json` the reranker reads.
#[derive(Debug, Default, serde::Deserialize)]
struct Config {
    pad_token_id: Option<u32>,
}

/// The pair tokenizer: the model's `tokenizer.json` with its pair template, truncating
/// the passage to `RERANK_PAIR_TOKENS`.
pub(crate) struct PairTokens {
    tokenizer: Tokenizer,
    pad: u32,
}

impl PairTokens {
    pub(crate) fn new(mut tokenizer: Tokenizer, pad: u32) -> Result<Self, EmbedError> {
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: RERANK_PAIR_TOKENS,
                strategy: TruncationStrategy::OnlySecond,
                ..Default::default()
            }))
            .map_err(|e| EmbedError::Load(format!("tokenizer.json: {e}")))?;
        tokenizer.with_padding(None);
        Ok(Self { tokenizer, pad })
    }

    fn load(dir: &Path) -> Result<Self, EmbedError> {
        let config: Config = match std::fs::read(dir.join("config.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| EmbedError::Load(format!("config.json: {e}")))?,
            Err(e) => return Err(EmbedError::Load(format!("config.json: {e}"))),
        };
        let tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| EmbedError::Load(format!("tokenizer.json: {e}")))?;
        Self::new(tokenizer, config.pad_token_id.unwrap_or(0))
    }

    /// One pair's ids, mask and segment ids, at most `RERANK_PAIR_TOKENS` long. The
    /// passage is cut first; a query too long to leave the passage any room (longer than
    /// `routes::RERANK_QUERY_CHARS` allows) is cut too rather than failing the call.
    fn pair(&self, query: &str, passage: &str) -> Result<Encoding, EmbedError> {
        match self.tokenizer.encode((query, passage), true) {
            Ok(encoding) => Ok(encoding),
            Err(_) => {
                let mut longest = self.tokenizer.clone();
                longest
                    .with_truncation(Some(TruncationParams {
                        max_length: RERANK_PAIR_TOKENS,
                        strategy: TruncationStrategy::LongestFirst,
                        ..Default::default()
                    }))
                    .map_err(|e| EmbedError::Embed(format!("tokenizing: {e}")))?;
                longest
                    .encode((query, passage), true)
                    .map_err(|e| EmbedError::Embed(format!("tokenizing: {e}")))
            }
        }
    }
}

/// Runs `pairs` through `graph`, shortest first, `RERANK_BATCH` rows per run,
/// right-padded with `pad` (mask 0); logits in the input's order.
fn run_batched(
    graph: &mut dyn PairGraph,
    pairs: &[Encoding],
    pad: u32,
) -> Result<Vec<f32>, EmbedError> {
    let mut order: Vec<usize> = (0..pairs.len()).collect();
    order.sort_by_key(|&i| pairs[i].get_ids().len());
    let mut out = vec![0f32; pairs.len()];
    for batch in order.chunks(RERANK_BATCH) {
        let rows = batch.len();
        let cols = batch
            .iter()
            .map(|&i| pairs[i].get_ids().len())
            .max()
            .unwrap_or(0)
            .max(1);
        let mut ids = vec![pad as i64; rows * cols];
        let mut mask = vec![0i64; rows * cols];
        let mut types = vec![0i64; rows * cols];
        for (r, &i) in batch.iter().enumerate() {
            let e = &pairs[i];
            let at = r * cols;
            for (c, ((id, m), t)) in e
                .get_ids()
                .iter()
                .zip(e.get_attention_mask())
                .zip(e.get_type_ids())
                .enumerate()
            {
                ids[at + c] = *id as i64;
                mask[at + c] = *m as i64;
                types[at + c] = *t as i64;
            }
        }
        let logits = graph.run(&ids, &mask, &types, rows, cols)?;
        if logits.len() != rows {
            return Err(EmbedError::Embed(format!(
                "the reranker returned {} scores for {rows} pairs",
                logits.len()
            )));
        }
        for (&i, logit) in batch.iter().zip(logits) {
            if !logit.is_finite() {
                return Err(EmbedError::Embed(
                    "the reranker returned a malformed score".into(),
                ));
            }
            out[i] = logit;
        }
    }
    Ok(out)
}

type Graph = IdleSlot<Mutex<Box<dyn PairGraph>>>;

/// A cross-encoder: its tokenizer, and its graph loaded on use.
pub struct OnnxReranker {
    model: RerankerModel,
    version: String,
    tokens: PairTokens,
    graph: Arc<Graph>,
    load_graph: Loader<Box<dyn PairGraph>>,
}

impl OnnxReranker {
    /// Opens the model's tokenizer; the graph (`onnx/model_quantized.onnx`) loads on the
    /// CPU EP (`PreferCPU`, the intra-op threads of `opts`) on the first `score`. A
    /// missing or unfinished download is `EmbedError::NotDownloaded`. Blocking.
    pub fn load(files: &RerankerFiles, opts: &OnnxOptions) -> Result<Self, EmbedError> {
        if !files.is_complete() {
            return Err(EmbedError::NotDownloaded);
        }
        let tokens = PairTokens::load(&files.dir)?;
        let path: PathBuf = files.graph_path();
        let threads = opts.threads();
        Ok(Self::from_parts(
            files.model,
            tokens,
            Box::new(move || {
                Ok(Box::new(OrtPairGraph::load(&path, threads)?) as Box<dyn PairGraph>)
            }),
            ENCODER_IDLE,
        ))
    }

    /// A reranker over `load_graph` (the real graph, or a stub in tests), unloaded after
    /// `idle` unused.
    pub(crate) fn from_parts(
        model: RerankerModel,
        tokens: PairTokens,
        load_graph: Loader<Box<dyn PairGraph>>,
        idle: Duration,
    ) -> Self {
        let graph = Arc::new(IdleSlot::new(idle));
        spawn_sweeper(Arc::downgrade(&graph));
        Self {
            model,
            version: model.version(),
            tokens,
            graph,
            load_graph,
        }
    }

    pub fn model(&self) -> RerankerModel {
        self.model
    }

    /// The model and its format (`RerankerModel::version`), recorded with eval runs and
    /// traces.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Drops the graph if unused for `ENCODER_IDLE`. Returns whether it did.
    pub fn sweep(&self) -> bool {
        self.graph.sweep(Instant::now())
    }

    /// Whether the graph is loaded now.
    pub fn is_loaded(&self) -> bool {
        self.graph.is_loaded()
    }

    /// Drops the graph now (before its files are deleted). Returns whether it was
    /// loaded.
    pub fn unload(&self) -> bool {
        self.graph.clear()
    }
}

/// Sweeps the slot every `SWEEP_EVERY` until the reranker owning it is dropped.
fn spawn_sweeper(slot: Weak<Graph>) {
    let spawned = std::thread::Builder::new()
        .name("margin-rerank-sweep".into())
        .spawn(move || {
            loop {
                std::thread::sleep(SWEEP_EVERY);
                let Some(slot) = slot.upgrade() else { return };
                if slot.sweep(Instant::now()) {
                    tracing::info!("the reranker model was unloaded after being unused");
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("couldn't start the reranker's idle sweeper: {e}");
    }
}

impl Reranker for OnnxReranker {
    fn name(&self) -> &str {
        &self.version
    }

    fn score(&self, query: &str, passages: &[String]) -> Result<Vec<f32>, EmbedError> {
        if passages.is_empty() {
            return Ok(Vec::new());
        }
        let pairs: Vec<Encoding> = passages
            .iter()
            .map(|p| self.tokens.pair(query, p))
            .collect::<Result<_, _>>()?;
        let now = Instant::now();
        self.graph.sweep(now);
        let graph = self
            .graph
            .get(now, || (self.load_graph)().map(Mutex::new))?;
        let mut graph = graph
            .lock()
            .map_err(|_| EmbedError::Embed("the reranker failed earlier".into()))?;
        run_batched(graph.as_mut(), &pairs, self.tokens.pad)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A BERT-like tokenizer over a word-level vocabulary: `[CLS] a [SEP] b [SEP]`, with
    /// segment ids 0 then 1. Unknown words are `[UNK]`.
    fn pair_tokens() -> PairTokens {
        let mut vocab = serde_json::Map::new();
        for (i, w) in ["[PAD]", "[UNK]", "[CLS]", "[SEP]"].iter().enumerate() {
            vocab.insert(w.to_string(), i.into());
        }
        for w in "gibbs phenomenon overshoot fourier series plants light sugar word the a"
            .split_whitespace()
        {
            let n = vocab.len();
            vocab.entry(w.to_string()).or_insert(n.into());
        }
        let json = serde_json::json!({
            "version": "1.0",
            "truncation": null,
            "padding": null,
            "added_tokens": [],
            "normalizer": {"type": "Lowercase"},
            "pre_tokenizer": {"type": "Whitespace"},
            "post_processor": {"type": "BertProcessing", "sep": ["[SEP]", 3], "cls": ["[CLS]", 2]},
            "decoder": null,
            "model": {"type": "WordLevel", "vocab": vocab, "unk_token": "[UNK]"}
        });
        PairTokens::new(json.to_string().parse().unwrap(), 0).unwrap()
    }

    /// One run of the stub: its shape and its rows' (ids, mask, types), unpadded.
    type Run = (usize, usize, Vec<(Vec<i64>, Vec<i64>, Vec<i64>)>);

    /// A stand-in graph: a row's logit is how many of its passage tokens (segment 1, not
    /// `[SEP]`) also appear in its query (segment 0), so padding must not change it.
    #[derive(Clone, Default)]
    struct StubGraph {
        runs: Arc<Mutex<Vec<Run>>>,
        fail: bool,
    }

    impl PairGraph for StubGraph {
        fn run(
            &mut self,
            ids: &[i64],
            mask: &[i64],
            types: &[i64],
            rows: usize,
            cols: usize,
        ) -> Result<Vec<f32>, EmbedError> {
            assert_eq!(ids.len(), rows * cols);
            assert_eq!(mask.len(), rows * cols);
            assert_eq!(types.len(), rows * cols);
            if self.fail {
                return Err(EmbedError::Embed("boom".into()));
            }
            let mut seen = Vec::new();
            let logits = (0..rows)
                .map(|r| {
                    let row = r * cols..(r + 1) * cols;
                    let kept: Vec<usize> = row.clone().filter(|&i| mask[i] == 1).collect();
                    // Padding only at the end.
                    assert!(kept.iter().all(|&i| i < row.start + kept.len()));
                    seen.push((
                        kept.iter().map(|&i| ids[i]).collect(),
                        kept.iter().map(|&i| mask[i]).collect(),
                        kept.iter().map(|&i| types[i]).collect(),
                    ));
                    let query: Vec<i64> = kept
                        .iter()
                        .filter(|&&i| types[i] == 0 && ids[i] > 3)
                        .map(|&i| ids[i])
                        .collect();
                    kept.iter()
                        .filter(|&&i| types[i] == 1 && ids[i] > 3 && query.contains(&ids[i]))
                        .count() as f32
                })
                .collect();
            self.runs.lock().unwrap().push((rows, cols, seen));
            Ok(logits)
        }
    }

    fn reranker(graph: StubGraph, loads: Arc<AtomicUsize>, idle: Duration) -> OnnxReranker {
        OnnxReranker::from_parts(
            RerankerModel::MsMarcoMiniLmL6,
            pair_tokens(),
            Box::new(move || {
                loads.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(graph.clone()) as Box<dyn PairGraph>)
            }),
            idle,
        )
    }

    fn strings(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn pairs_are_scored_in_batches_of_eight_and_come_back_in_order() {
        let graph = StubGraph::default();
        let loads = Arc::new(AtomicUsize::new(0));
        let r = reranker(graph.clone(), loads.clone(), ENCODER_IDLE);
        assert_eq!(r.name(), "ms-marco-minilm-l6-v2@int8");
        assert_eq!(r.version(), r.name());
        assert_eq!(r.model(), RerankerModel::MsMarcoMiniLmL6);
        assert!(r.score("gibbs", &[]).unwrap().is_empty());
        assert_eq!(
            loads.load(Ordering::SeqCst),
            0,
            "nothing to score loads nothing"
        );

        // 19 passages of varied length: three runs (8, 8, 3).
        let mut passages = Vec::new();
        for i in 0..19 {
            let extra = "word ".repeat(i % 5);
            let p = match i % 3 {
                0 => format!("gibbs phenomenon overshoot {extra}"),
                1 => format!("fourier series {extra}"),
                _ => format!("plants light sugar gibbs {extra}"),
            };
            passages.push(p);
        }
        let scores = r.score("Gibbs phenomenon", &passages).unwrap();
        let expected: Vec<f32> = (0..19)
            .map(|i| match i % 3 {
                0 => 2.0,
                1 => 0.0,
                _ => 1.0,
            })
            .collect();
        assert_eq!(scores, expected);
        let runs = graph.runs.lock().unwrap();
        assert_eq!(
            runs.iter().map(|(rows, _, _)| *rows).collect::<Vec<_>>(),
            vec![8, 8, 3]
        );
        // Shortest first: the runs' widths never shrink.
        let widths: Vec<usize> = runs.iter().map(|(_, cols, _)| *cols).collect();
        assert!(widths.windows(2).all(|w| w[0] <= w[1]), "{widths:?}");
        // Each pair is `[CLS] query [SEP] passage [SEP]` with segments 0 then 1.
        let (ids, mask, types) = &runs[0].2[0];
        assert_eq!(ids[0], 2);
        assert_eq!(&ids[1..4], &[4, 5, 3]);
        assert_eq!(*ids.last().unwrap(), 3);
        assert!(mask.iter().all(|m| *m == 1));
        assert_eq!(&types[..4], &[0, 0, 0, 0]);
        assert!(types[4..].iter().all(|t| *t == 1));
        drop(runs);
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        // Batched or one at a time, a pair scores the same.
        for (i, p) in passages.iter().enumerate() {
            let alone = r
                .score("Gibbs phenomenon", std::slice::from_ref(p))
                .unwrap();
            assert_eq!(alone, vec![scores[i]]);
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1, "the graph stays loaded");
    }

    #[test]
    fn long_passages_are_cut_and_the_query_kept_whole() {
        let graph = StubGraph::default();
        let r = reranker(graph.clone(), Arc::default(), ENCODER_IDLE);
        let query = "gibbs phenomenon overshoot fourier";
        let long = format!("{} gibbs", "word ".repeat(2000));
        let scores = r.score(query, &[long]).unwrap();
        // The passage's one matching word was past the cut.
        assert_eq!(scores, vec![0.0]);
        let runs = graph.runs.lock().unwrap();
        let (ids, _, types) = &runs[0].2[0];
        assert_eq!(ids.len(), RERANK_PAIR_TOKENS);
        assert_eq!(&ids[..6], &[2, 4, 5, 6, 7, 3]);
        assert_eq!(types.iter().filter(|t| **t == 0).count(), 6);
        drop(runs);

        // A query longer than the whole pair is cut too rather than failing.
        let huge = "gibbs ".repeat(RERANK_PAIR_TOKENS * 2);
        let scores = r.score(&huge, &strings(&["gibbs overshoot"])).unwrap();
        assert_eq!(scores, vec![1.0]);
        let runs = graph.runs.lock().unwrap();
        assert_eq!(runs[1].2[0].0.len(), RERANK_PAIR_TOKENS);
    }

    #[test]
    fn the_graph_is_dropped_when_idle_and_loaded_again_on_use() {
        let loads = Arc::new(AtomicUsize::new(0));
        let r = reranker(
            StubGraph::default(),
            loads.clone(),
            Duration::from_millis(30),
        );
        assert!(!r.is_loaded());
        r.score("gibbs", &strings(&["gibbs"])).unwrap();
        assert!(r.is_loaded());
        assert!(!r.sweep(), "just used");
        std::thread::sleep(Duration::from_millis(60));
        assert!(r.sweep());
        assert!(!r.is_loaded());
        r.score("gibbs", &strings(&["gibbs"])).unwrap();
        assert_eq!(loads.load(Ordering::SeqCst), 2);
        assert!(r.unload());
        assert!(!r.unload());
    }

    #[test]
    fn graph_failures_are_errors_not_scores() {
        let failing = StubGraph {
            fail: true,
            ..Default::default()
        };
        let r = reranker(failing, Arc::default(), ENCODER_IDLE);
        assert!(r.score("gibbs", &strings(&["gibbs"])).is_err());

        let r = OnnxReranker::from_parts(
            RerankerModel::BgeRerankerBase,
            pair_tokens(),
            Box::new(|| Err(EmbedError::Load("no graph".into()))),
            ENCODER_IDLE,
        );
        assert_eq!(r.name(), "bge-reranker-base@int8");
        let err = r.score("gibbs", &strings(&["gibbs"])).unwrap_err();
        assert!(err.to_string().contains("no graph"), "{err}");
        assert!(!r.is_loaded());

        /// Answers fewer logits than rows, or a NaN.
        struct Short(bool);
        impl PairGraph for Short {
            fn run(
                &mut self,
                _: &[i64],
                _: &[i64],
                _: &[i64],
                rows: usize,
                _: usize,
            ) -> Result<Vec<f32>, EmbedError> {
                Ok(if self.0 {
                    vec![0.0; rows - 1]
                } else {
                    vec![f32::NAN; rows]
                })
            }
        }
        for short in [true, false] {
            let r = OnnxReranker::from_parts(
                RerankerModel::MsMarcoMiniLmL6,
                pair_tokens(),
                Box::new(move || Ok(Box::new(Short(short)) as Box<dyn PairGraph>)),
                ENCODER_IDLE,
            );
            assert!(r.score("gibbs", &strings(&["a", "b"])).is_err());
        }
    }

    #[test]
    fn loading_needs_a_complete_download() {
        let dir = std::env::temp_dir().join(format!("margin-rerank-none-{}", std::process::id()));
        let files = RerankerFiles::new(&dir, RerankerModel::DEFAULT);
        assert!(matches!(
            OnnxReranker::load(&files, &OnnxOptions::default()),
            Err(EmbedError::NotDownloaded)
        ));
    }
}
