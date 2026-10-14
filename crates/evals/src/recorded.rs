//! Recorded embeddings: the real EmbeddingGemma vectors of every corpus chunk, corpus
//! picture and golden query, recorded once on the service Mac by `margin-eval
//! record-embeddings` and replayed in CI so recall@k there is real, not bag of words
//! (`docs/agent-rag-architecture.md` §3.1).
//!
//! # File format (`evals/recorded/embeddings.bin`)
//!
//! ```text
//! b"MRGNVEC1"                       8 bytes, magic
//! header_len: u32 LE                then header_len bytes of JSON:
//!   {"embedder_version": "...", "dim": 768, "count": N}
//! N entries, sorted by key:
//!   key: [u8; 32]                   sha256(embedder_version ‖ 0x00 ‖ input)
//!   vector: [f16 LE; dim]           IEEE half precision, L2-normalised before rounding
//! ```
//!
//! `input` is exactly what the model is given: for a text the task prefix plus the text
//! (`title: {title} | text: {chunk}`, `title: none | text: …` without a title, `task:
//! search result | query: {query}`), for a picture `picture:{document}#{page}:{tokens}`
//! (`page` empty for an image file; `tokens` the image encoder's soft-token count).
//! A text or picture that wasn't recorded is an error naming it, never a made-up
//! vector, so a chunker change that cuts new chunk texts needs a deliberate re-record.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, bail};
use margin_embed::fake::FakeEmbedder;
use margin_embed::{DIM, Embedder, ImageTokens, Picture, Purpose};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::corpus::CorpusDoc;

/// The embedder and prompt-prefix version the eval index is built with: EmbeddingGemma
/// 2 with the document title prefix of §2.1. Stamped on every passage row, eval run and
/// recording; `compare` refuses to compare runs across versions unless told to.
pub const EMBEDDER_VERSION: &str = "embeddinggemma-2/title-prefix-v1";

/// `evals/recorded/embeddings.bin`.
pub const EMBEDDINGS_FILE: &str = "recorded/embeddings.bin";

const MAGIC: &[u8; 8] = b"MRGNVEC1";

/// What the model is given for `text` with `purpose`.
pub fn model_input(purpose: &Purpose, text: &str) -> String {
    match purpose {
        Purpose::Document { title } => format!(
            "title: {} | text: {text}",
            title.as_deref().filter(|t| !t.is_empty()).unwrap_or("none")
        ),
        Purpose::Query => format!("task: search result | query: {text}"),
    }
}

/// The input string standing for a picture.
pub fn picture_input(document: &str, page: Option<u32>, tokens: ImageTokens) -> String {
    format!(
        "picture:{document}#{}:{}",
        page.map(|p| p.to_string()).unwrap_or_default(),
        tokens.count()
    )
}

pub type Key = [u8; 32];

pub fn key(embedder_version: &str, input: &str) -> Key {
    let mut h = Sha256::new();
    h.update(embedder_version.as_bytes());
    h.update([0]);
    h.update(input.as_bytes());
    h.finalize().into()
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

/// A set of recorded vectors, to write or to replay.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Recording {
    pub embedder_version: String,
    pub entries: BTreeMap<Key, Vec<f32>>,
}

#[derive(Serialize, Deserialize)]
struct Header {
    embedder_version: String,
    dim: usize,
    count: usize,
}

impl Recording {
    pub fn new(embedder_version: &str) -> Self {
        Self {
            embedder_version: embedder_version.to_string(),
            entries: BTreeMap::new(),
        }
    }

    pub fn insert(&mut self, input: &str, vector: Vec<f32>) {
        self.entries
            .insert(key(&self.embedder_version, input), vector);
    }

    pub fn get(&self, input: &str) -> Option<&Vec<f32>> {
        self.entries.get(&key(&self.embedder_version, input))
    }

    pub fn to_bytes(&self) -> anyhow::Result<Vec<u8>> {
        let header = serde_json::to_vec(&Header {
            embedder_version: self.embedder_version.clone(),
            dim: DIM,
            count: self.entries.len(),
        })?;
        let mut out = Vec::with_capacity(12 + header.len() + self.entries.len() * (32 + DIM * 2));
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(header.len() as u32).to_le_bytes());
        out.extend_from_slice(&header);
        for (k, v) in &self.entries {
            anyhow::ensure!(
                v.len() == DIM,
                "a vector has {} dimensions, not {DIM}",
                v.len()
            );
            out.extend_from_slice(k);
            for x in v {
                out.extend_from_slice(&half::f16::from_f32(*x).to_le_bytes());
            }
        }
        Ok(out)
    }

    pub fn from_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
        if bytes.len() < 12 || &bytes[..8] != MAGIC {
            bail!("not a Margin embeddings recording");
        }
        let hlen = u32::from_le_bytes(bytes[8..12].try_into()?) as usize;
        let header: Header = serde_json::from_slice(
            bytes
                .get(12..12 + hlen)
                .context("the recording's header is cut off")?,
        )?;
        let mut pos = 12 + hlen;
        let entry = 32 + header.dim * 2;
        anyhow::ensure!(
            bytes.len() == pos + header.count * entry,
            "the recording holds {} bytes of entries, not {} × {entry}",
            bytes.len() - pos,
            header.count
        );
        let mut entries = BTreeMap::new();
        for _ in 0..header.count {
            let k: Key = bytes[pos..pos + 32].try_into()?;
            let v = bytes[pos + 32..pos + entry]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| half::f16::from_le_bytes(*b).to_f32())
                .collect();
            entries.insert(k, v);
            pos += entry;
        }
        Ok(Self {
            embedder_version: header.embedder_version,
            entries,
        })
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let bytes = std::fs::read(path).with_context(|| {
            format!(
                "couldn't read {} (run `margin-eval record-embeddings` on the service Mac)",
                path.display()
            )
        })?;
        Self::from_bytes(&bytes).with_context(|| format!("bad {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, self.to_bytes()?)
            .with_context(|| format!("couldn't write {}", path.display()))
    }
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
        Ok(Self::new(Recording::load(path)?))
    }

    fn lookup(&self, input: &str) -> anyhow::Result<Vec<f32>> {
        match self.recording.get(input) {
            Some(v) => Ok(v.clone()),
            None => {
                self.misses.lock().unwrap().push(short(input));
                bail!(
                    "no recorded vector for \"{}\" (embedder {}): the chunker or the corpus \
                     changed since the recording; re-run `margin-eval record-embeddings`",
                    short(input),
                    self.recording.embedder_version
                )
            }
        }
    }
}

impl Vectors for RecordedVectors {
    fn version(&self) -> &str {
        &self.recording.embedder_version
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
            seed.embedder_version == inner.version(),
            "the existing recording is of {}, the embedder is {}",
            seed.embedder_version,
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
        let keep: std::collections::HashSet<Key> =
            used.iter().map(|i| key(&rec.embedder_version, i)).collect();
        rec.entries.retain(|k, _| keep.contains(k));
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
                .filter(|i| rec.get(&inputs[*i]).is_none())
                .collect()
        };
        if !missing.is_empty() {
            let todo: Vec<String> = missing.iter().map(|i| texts[*i].clone()).collect();
            let vecs = self.inner.texts(&todo, purpose)?;
            let mut rec = self.recording.lock().unwrap();
            for (i, v) in missing.iter().zip(vecs) {
                rec.insert(&inputs[*i], v);
            }
            *self.embedded.lock().unwrap() += missing.len();
        }
        let rec = self.recording.lock().unwrap();
        Ok(inputs.iter().map(|i| rec.get(i).unwrap().clone()).collect())
    }
    fn picture(&self, doc: &CorpusDoc, pic: &Picture, text: &str) -> anyhow::Result<Vec<f32>> {
        let input = picture_input(&doc.path, pic.page, pic.tokens);
        self.used.lock().unwrap().insert(input.clone());
        if let Some(v) = self.recording.lock().unwrap().get(&input) {
            return Ok(v.clone());
        }
        let v = self.inner.picture(doc, pic, text)?;
        self.recording.lock().unwrap().insert(&input, v.clone());
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
        let mut v = vec![0f32; DIM];
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
        assert_ne!(key("v1", "a"), key("v2", "a"));
    }

    #[test]
    fn a_recording_round_trips_through_f16() {
        let mut rec = Recording::new("v1");
        let mut v = unit(3);
        v[4] = 0.333;
        rec.insert("title: none | text: a", v.clone());
        rec.insert("task: search result | query: b", unit(7));
        let bytes = rec.to_bytes().unwrap();
        assert_eq!(&bytes[..8], MAGIC);
        let back = Recording::from_bytes(&bytes).unwrap();
        assert_eq!(back.embedder_version, "v1");
        let got = back.get("title: none | text: a").unwrap();
        assert!((got[4] - 0.333).abs() < 1e-3);
        assert_eq!(got[3], 1.0);
        assert!(Recording::from_bytes(&bytes[..bytes.len() - 1]).is_err());
        assert!(Recording::from_bytes(b"not a file").is_err());
        let mut wrong = rec.clone();
        wrong.insert("x", vec![0.0; 3]);
        assert!(wrong.to_bytes().is_err());
    }

    #[test]
    fn replay_fails_loudly_on_a_miss() {
        let mut rec = Recording::new("v1");
        rec.insert(&model_input(&Purpose::Query, "known"), unit(1));
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
        rec.insert(&picture_input(&doc.path, None, ImageTokens::High), unit(2));
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
        let mut seed = Recording::new(inner.version());
        seed.insert(&model_input(&Purpose::Query, "old"), unit(5));
        seed.insert(&model_input(&Purpose::Query, "stale"), unit(6));
        let rv = RecordingVectors::new(&inner, seed).unwrap();
        let got = rv
            .texts(&["old".into(), "new words".into()], &Purpose::Query)
            .unwrap();
        assert_eq!(got[0], unit(5), "served from the seed");
        assert_eq!(rv.embedded(), 1);
        let rec = rv.finish();
        assert_eq!(rec.entries.len(), 2);
        assert!(rec.get(&model_input(&Purpose::Query, "stale")).is_none());
        assert!(RecordingVectors::new(&inner, Recording::new("other")).is_err());
    }
}
