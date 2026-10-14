//! Recorded embeddings: the real EmbeddingGemma vectors of every corpus chunk, corpus
//! picture and golden query, recorded once on the service Mac by `margin-eval
//! record-embeddings` and replayed in CI so recall@k there is real, not bag of words
//! (`docs/agent-rag-architecture.md` §3.1).
//!
//! The file is `margin_embed::fake::Recording`'s (one format and one reader for the
//! service's `RecordedEmbedder` and the runner): f16 vectors keyed by `sha256(input)`,
//! the embedder version in the header. `input` is exactly what the model is given: for
//! a text the task prefix plus the text (`margin_embed::prompt`: `title: {title} |
//! text: {chunk}`, `title: none | text: …` without a title, `task: search result |
//! query: {query}`), the key `RecordedEmbedder::embed` looks up; for a picture
//! `picture:{document}#{page}:{tokens}` (`page` empty for an image file; `tokens` the
//! image encoder's soft-token count), so CI needn't render pages to find them.
//! A text or picture that wasn't recorded is an error naming it, never a made-up
//! vector, so a chunker change that cuts new chunk texts needs a deliberate re-record.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, bail};
use margin_embed::fake::FakeEmbedder;
use margin_embed::{Embedder, ImageTokens, Picture, Purpose};

use crate::corpus::CorpusDoc;

/// The embedder and prompt-prefix version the eval index is built with: EmbeddingGemma
/// 2 on Core ML with the document title prefix of §2.1 (the baseline recording's; the
/// ONNX backends have their own, `margin_embed::Backend::embedder_version`).
/// Stamped on every passage row, eval run and recording; `compare` refuses to compare
/// runs across versions unless told to.
pub fn embedder_version() -> &'static str {
    margin_embed::embedder_version(margin_embed::Backend::CoreMl)
}

/// `evals/recorded/embeddings.bin`.
pub const EMBEDDINGS_FILE: &str = "recorded/embeddings.bin";

pub use margin_embed::fake::Recording;

/// What the model is given for `text` with `purpose` (the task prefix, then the text).
pub fn model_input(purpose: &Purpose, text: &str) -> String {
    format!("{}{text}", margin_embed::prompt(purpose))
}

/// The input string standing for a picture.
pub fn picture_input(document: &str, page: Option<u32>, tokens: ImageTokens) -> String {
    format!(
        "picture:{document}#{}:{}",
        page.map(|p| p.to_string()).unwrap_or_default(),
        tokens.count()
    )
}

/// Where the eval index gets its vectors: the recording (CI), the real model (the
/// runner), or bag of words (the harness's own tests).
pub trait Vectors: Send + Sync {
    /// Stamped on passage rows and runs.
    fn version(&self) -> &str;
    fn texts(&self, texts: &[String], purpose: &Purpose) -> anyhow::Result<Vec<Vec<f32>>>;
    /// The picture `pic` of `doc`; `text` is what was read from it (OCR, page text).
    fn picture(&self, doc: &CorpusDoc, pic: &Picture, text: &str) -> anyhow::Result<Vec<f32>>;
}

/// Reads a recording, saying how to make one when it is missing.
pub fn load(path: &Path) -> anyhow::Result<Recording> {
    Recording::load(path).with_context(|| {
        format!(
            "couldn't read {} (run `margin-eval record-embeddings` on the service Mac)",
            path.display()
        )
    })
}

fn short(input: &str) -> String {
    let s: String = input.chars().take(90).collect();
    if s.len() < input.len() {
        format!("{s}…")
    } else {
        s
    }
}

/// Replays a recording; anything not recorded is an error.
pub struct RecordedVectors {
    recording: Recording,
    /// Inputs asked for and missing, for one error that lists them all.
    misses: Mutex<Vec<String>>,
}

impl RecordedVectors {
    pub fn new(recording: Recording) -> Self {
        Self {
            recording,
            misses: Mutex::new(Vec::new()),
        }
    }

    pub fn open(path: &Path) -> anyhow::Result<Self> {
        Ok(Self::new(load(path)?))
    }

    fn lookup(&self, input: &str) -> anyhow::Result<Vec<f32>> {
        match self.recording.get_input(input) {
            Some(v) => Ok(v.clone()),
            None => {
                self.misses.lock().unwrap().push(short(input));
                bail!(
                    "no recorded vector for \"{}\" (embedder {}): the chunker or the corpus \
                     changed since the recording; re-run `margin-eval record-embeddings`",
                    short(input),
                    self.recording.version
                )
            }
        }
    }
}

impl Vectors for RecordedVectors {
    fn version(&self) -> &str {
        &self.recording.version
    }
    fn texts(&self, texts: &[String], purpose: &Purpose) -> anyhow::Result<Vec<Vec<f32>>> {
        texts
            .iter()
            .map(|t| self.lookup(&model_input(purpose, t)))
            .collect()
    }
    fn picture(&self, doc: &CorpusDoc, pic: &Picture, _text: &str) -> anyhow::Result<Vec<f32>> {
        self.lookup(&picture_input(&doc.path, pic.page, pic.tokens))
    }
}

/// Wraps a real source of vectors and keeps everything it returns, for
/// `record-embeddings`. Inputs already in `seed` are served from it (re-recording only
/// embeds what changed).
pub struct RecordingVectors<'a> {
    inner: &'a dyn Vectors,
    recording: Mutex<Recording>,
    /// Inputs `inner` was asked for.
    embedded: Mutex<usize>,
    /// Every input asked for, recorded or not.
    used: Mutex<std::collections::HashSet<String>>,
}

impl<'a> RecordingVectors<'a> {
    pub fn new(inner: &'a dyn Vectors, seed: Recording) -> anyhow::Result<Self> {
        anyhow::ensure!(
            seed.version == inner.version(),
            "the existing recording is of {}, the embedder is {}",
            seed.version,
            inner.version()
        );
        Ok(Self {
            inner,
            recording: Mutex::new(seed),
            embedded: Mutex::new(0),
            used: Mutex::new(Default::default()),
        })
    }

    /// The recording, keeping only the inputs asked for (so stale vectors don't pile
    /// up).
    pub fn finish(self) -> Recording {
        let mut rec = self.recording.into_inner().unwrap();
        let used = self.used.into_inner().unwrap();
        rec.retain_inputs(used.iter().map(String::as_str));
        rec
    }

    pub fn embedded(&self) -> usize {
        *self.embedded.lock().unwrap()
    }
}

impl Vectors for RecordingVectors<'_> {
    fn version(&self) -> &str {
        self.inner.version()
    }
    fn texts(&self, texts: &[String], purpose: &Purpose) -> anyhow::Result<Vec<Vec<f32>>> {
        let inputs: Vec<String> = texts.iter().map(|t| model_input(purpose, t)).collect();
        self.used.lock().unwrap().extend(inputs.iter().cloned());
        let missing: Vec<usize> = {
            let rec = self.recording.lock().unwrap();
            (0..texts.len())
                .filter(|i| rec.get_input(&inputs[*i]).is_none())
                .collect()
        };
        if !missing.is_empty() {
            let todo: Vec<String> = missing.iter().map(|i| texts[*i].clone()).collect();
            let vecs = self.inner.texts(&todo, purpose)?;
            let mut rec = self.recording.lock().unwrap();
            for (i, v) in missing.iter().zip(vecs) {
                rec.insert_input(&inputs[*i], v);
            }
            *self.embedded.lock().unwrap() += missing.len();
        }
        let rec = self.recording.lock().unwrap();
        Ok(inputs
            .iter()
            .map(|i| rec.get_input(i).unwrap().clone())
            .collect())
    }
    fn picture(&self, doc: &CorpusDoc, pic: &Picture, text: &str) -> anyhow::Result<Vec<f32>> {
        let input = picture_input(&doc.path, pic.page, pic.tokens);
        self.used.lock().unwrap().insert(input.clone());
        if let Some(v) = self.recording.lock().unwrap().get_input(&input) {
            return Ok(v.clone());
        }
        let v = self.inner.picture(doc, pic, text)?;
        self.recording
            .lock()
            .unwrap()
            .insert_input(&input, v.clone());
        *self.embedded.lock().unwrap() += 1;
        Ok(v)
    }
}

/// Bag-of-words vectors (`margin_embed::fake::FakeEmbedder`) for the harness's own
/// tests: a picture is embedded as the text read from it, or as its path when it has
/// none. Never used for metrics.
#[derive(Debug, Default)]
pub struct FakeVectors;

impl Vectors for FakeVectors {
    fn version(&self) -> &str {
        "fake-bag-of-words"
    }
    fn texts(&self, texts: &[String], purpose: &Purpose) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(FakeEmbedder.embed(texts, purpose.clone())?)
    }
    fn picture(&self, doc: &CorpusDoc, _pic: &Picture, text: &str) -> anyhow::Result<Vec<f32>> {
        let words = if text.trim().is_empty() {
            doc.path.replace(['/', '-', '.'], " ")
        } else {
            text.to_string()
        };
        Ok(FakeEmbedder.embed(&[words], Purpose::DOCUMENT)?.remove(0))
    }
}

/// Text vectors by input, for tests that need specific geometry.
pub struct TableVectors {
    pub version: String,
    pub table: HashMap<String, Vec<f32>>,
}

impl Vectors for TableVectors {
    fn version(&self) -> &str {
        &self.version
    }
    fn texts(&self, texts: &[String], purpose: &Purpose) -> anyhow::Result<Vec<Vec<f32>>> {
        texts
            .iter()
            .map(|t| {
                let input = model_input(purpose, t);
                self.table
                    .get(&input)
                    .cloned()
                    .with_context(|| format!("no vector for {input}"))
            })
            .collect()
    }
    fn picture(&self, doc: &CorpusDoc, pic: &Picture, _text: &str) -> anyhow::Result<Vec<f32>> {
        let input = picture_input(&doc.path, pic.page, pic.tokens);
        self.table
            .get(&input)
            .cloned()
            .with_context(|| format!("no vector for {input}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use margin_shared::SourceKind;

    fn unit(i: usize) -> Vec<f32> {
        let mut v = vec![0f32; margin_embed::DIM];
        v[i] = 1.0;
        v
    }

    #[test]
    fn model_inputs_carry_the_task_prefix() {
        assert_eq!(
            model_input(&Purpose::DOCUMENT, "x"),
            "title: none | text: x"
        );
        assert_eq!(
            model_input(
                &Purpose::Document {
                    title: Some("Fourier > Gibbs".into())
                },
                "x"
            ),
            "title: Fourier > Gibbs | text: x"
        );
        assert_eq!(
            model_input(
                &Purpose::Document {
                    title: Some(String::new())
                },
                "x"
            ),
            "title: none | text: x"
        );
        assert_eq!(
            model_input(&Purpose::Query, "why"),
            "task: search result | query: why"
        );
        assert_eq!(
            picture_input("pdf/a.pdf", Some(2), ImageTokens::High),
            "picture:pdf/a.pdf#2:280"
        );
    }

    #[test]
    fn recorded_texts_are_what_the_services_recorded_embedder_reads() {
        use margin_embed::fake::RecordedEmbedder;
        let mut rec = Recording::new();
        let mut v = unit(3);
        v[4] = 0.333;
        rec.insert_input(&model_input(&Purpose::document(Some("Gibbs")), "a"), v);
        let back = Recording::from_bytes(&rec.to_bytes()).unwrap();
        let got = RecordedEmbedder::new(back)
            .unwrap()
            .embed(&["a".into()], Purpose::document(Some("Gibbs")))
            .unwrap()
            .remove(0);
        // f16, and unit length again after it.
        assert!((got[4] / got[3] - 0.333).abs() < 2e-3, "{got:?}");
    }

    #[test]
    fn replay_fails_loudly_on_a_miss() {
        let mut rec = Recording::for_version("v1");
        rec.insert_input(&model_input(&Purpose::Query, "known"), unit(1));
        let doc = CorpusDoc {
            path: "images/a.png".into(),
            kind: SourceKind::Image,
            in_folder: false,
        };
        let pic = Picture {
            path: "a.png".into(),
            page: None,
            at_ms: None,
            tokens: ImageTokens::High,
        };
        rec.insert_input(&picture_input(&doc.path, None, ImageTokens::High), unit(2));
        let r = RecordedVectors::new(rec);
        assert_eq!(
            r.texts(&["known".into()], &Purpose::Query).unwrap()[0],
            unit(1)
        );
        assert_eq!(r.picture(&doc, &pic, "").unwrap(), unit(2));
        let err = r
            .texts(&["unknown".into()], &Purpose::Query)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("re-run `margin-eval record-embeddings`"),
            "{err}"
        );
        // The same text as a document is a different input.
        assert!(r.texts(&["known".into()], &Purpose::DOCUMENT).is_err());
    }

    #[test]
    fn recording_embeds_only_what_is_new_and_drops_what_is_unused() {
        let inner = FakeVectors;
        let mut seed = Recording::for_version(inner.version());
        seed.insert_input(&model_input(&Purpose::Query, "old"), unit(5));
        seed.insert_input(&model_input(&Purpose::Query, "stale"), unit(6));
        let rv = RecordingVectors::new(&inner, seed).unwrap();
        let got = rv
            .texts(&["old".into(), "new words".into()], &Purpose::Query)
            .unwrap();
        assert_eq!(got[0], unit(5), "served from the seed");
        assert_eq!(rv.embedded(), 1);
        let rec = rv.finish();
        assert_eq!(rec.len().0, 2);
        assert!(
            rec.get_input(&model_input(&Purpose::Query, "stale"))
                .is_none()
        );
        assert!(RecordingVectors::new(&inner, Recording::for_version("other")).is_err());
    }
}
