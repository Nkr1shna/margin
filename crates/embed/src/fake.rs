//! A deterministic stand-in for tests: hashed bag of words, so texts sharing words are
//! similar. Not a real model.
//!
//! Images are read as files: one whose bytes are UTF-8 text (as test "images" are) is
//! embedded like that text, so a test picture saying "Fourier diagram" is found by a
//! query about Fourier; any other file by a hash of its bytes. Audio windows are embedded
//! by a hash of their samples.

use std::path::PathBuf;

use crate::{DIM, EmbedError, Embedder, ImageTokens, Purpose};

#[derive(Debug, Default, Clone)]
pub struct FakeEmbedder;

fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
}

fn hash(bytes: impl IntoIterator<Item = u8>) -> u64 {
    bytes.into_iter().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    })
}

fn normalized(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
    v
}

fn bag_of_words(text: &str) -> Vec<f32> {
    let mut v = vec![0f32; DIM];
    for w in words(text) {
        v[(hash(w.bytes()) % DIM as u64) as usize] += 1.0;
    }
    normalized(v)
}

/// One bucket chosen by `h`.
fn one_hot(h: u64) -> Vec<f32> {
    let mut v = vec![0f32; DIM];
    v[(h % DIM as u64) as usize] = 1.0;
    v
}

impl Embedder for FakeEmbedder {
    fn embed(&self, texts: &[String], _purpose: Purpose) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|t| bag_of_words(t)).collect())
    }

    fn count_tokens(&self, text: &str) -> usize {
        words(text).count()
    }

    fn embed_images(
        &self,
        images: &[PathBuf],
        _tokens: ImageTokens,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        images
            .iter()
            .map(|path| {
                let bytes = std::fs::read(path)
                    .map_err(|e| EmbedError::Extract(format!("{}: {e}", path.display())))?;
                Ok(match std::str::from_utf8(&bytes) {
                    Ok(text) if words(text).next().is_some() => bag_of_words(text),
                    _ => one_hot(hash(bytes)),
                })
            })
            .collect()
    }

    fn embed_audio(&self, windows: &[Vec<f32>]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(windows
            .iter()
            .map(|w| one_hot(hash(w.iter().flat_map(|s| s.to_le_bytes()))))
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Recordings of a real embedder
// ---------------------------------------------------------------------------

/// What the eval runner says to do about a miss.
const RERECORD: &str = "re-record with `margin-eval record-embeddings`";

/// The first bytes of a recording file.
const MAGIC: &[u8; 8] = b"MRGNEMB1";

/// A recording key: SHA-256 of what was asked.
type Key = [u8; 32];

/// What a real embedder answered, recorded by `Recorder` and served by
/// `RecordedEmbedder`, for one `EMBEDDER_VERSION`.
///
/// File layout (little-endian): `MRGNEMB1`; u32 length + the version (UTF-8); u32 `DIM`;
/// u32 vector count, then per vector its 32-byte key and `DIM` f16 values; u32 token-count
/// count, then per entry its 32-byte key and a u32. Entries are sorted by key, so the same
/// recording always writes the same bytes. Keys:
/// - a text: `sha256(prompt(purpose) + text)` (`crate::prompt`: the task prefix, with the
///   title);
/// - an image: `sha256("image" 0 tokens 0 file bytes)`;
/// - an audio window: `sha256("audio" 0 samples as f32 LE)`;
/// - a token count: `sha256("tokens" 0 text)`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Recording {
    pub version: String,
    vectors: std::collections::BTreeMap<Key, Vec<f32>>,
    tokens: std::collections::BTreeMap<Key, u32>,
}

fn text_key(text: &str, purpose: &Purpose) -> Key {
    let mut bytes = crate::prompt(purpose).into_bytes();
    bytes.extend_from_slice(text.as_bytes());
    crate::sha256::digest(&bytes)
}

fn image_key(bytes: &[u8], tokens: ImageTokens) -> Key {
    let mut all = format!("image\0{}\0", tokens.count()).into_bytes();
    all.extend_from_slice(bytes);
    crate::sha256::digest(&all)
}

fn audio_key(window: &[f32]) -> Key {
    let mut all = b"audio\0".to_vec();
    all.extend(window.iter().flat_map(|s| s.to_le_bytes()));
    crate::sha256::digest(&all)
}

fn tokens_key(text: &str) -> Key {
    let mut all = b"tokens\0".to_vec();
    all.extend_from_slice(text.as_bytes());
    crate::sha256::digest(&all)
}

fn read_image(path: &std::path::Path) -> Result<Vec<u8>, EmbedError> {
    std::fs::read(path).map_err(|e| EmbedError::Extract(format!("{}: {e}", path.display())))
}

/// f32 to IEEE half precision, rounding to nearest even (as `half::f16::from_f32`).
pub(crate) fn f32_to_f16(x: f32) -> u16 {
    let bits = x.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32;
    let man = bits & 0x7f_ffff;
    if exp == 0xff {
        // Infinity, or NaN (kept a NaN).
        return sign | 0x7c00 | if man != 0 { 0x200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    if e <= 0 {
        // Subnormal (or zero) in half precision.
        if e < -10 {
            return sign;
        }
        let m = man | 0x80_0000;
        let shift = (14 - e) as u32;
        let mut half = m >> shift;
        let rest = m & ((1 << shift) - 1);
        let halfway = 1 << (shift - 1);
        if rest > halfway || (rest == halfway && half & 1 == 1) {
            half += 1;
        }
        return sign | half as u16;
    }
    let mut half = ((e as u32) << 10) | (man >> 13);
    let rest = man & 0x1fff;
    // Rounding up may carry into the exponent, which is right (up to infinity).
    if rest > 0x1000 || (rest == 0x1000 && half & 1 == 1) {
        half += 1;
    }
    sign | half as u16
}

/// IEEE half precision to f32 (exact).
pub(crate) fn f16_to_f32(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = ((h >> 10) & 0x1f) as i32;
    let man = (h & 0x3ff) as f32;
    match exp {
        0 => sign * man * 2f32.powi(-24),
        0x1f if man == 0.0 => sign * f32::INFINITY,
        0x1f => f32::NAN,
        _ => sign * (1.0 + man / 1024.0) * 2f32.powi(exp - 15),
    }
}

impl Recording {
    /// An empty recording for `EMBEDDER_VERSION`.
    pub fn new() -> Self {
        Self {
            version: crate::EMBEDDER_VERSION.to_string(),
            ..Self::default()
        }
    }

    /// How many vectors and token counts it holds.
    pub fn len(&self) -> (usize, usize) {
        (self.vectors.len(), self.tokens.len())
    }

    pub fn is_empty(&self) -> bool {
        self.vectors.is_empty() && self.tokens.is_empty()
    }

    /// Adds what `other` holds (same version), keeping this one's answer for a key both
    /// have.
    pub fn merge(&mut self, other: Recording) -> Result<(), EmbedError> {
        if other.version != self.version {
            return Err(EmbedError::Load(format!(
                "can't merge a recording of {} into one of {}",
                other.version, self.version
            )));
        }
        for (k, v) in other.vectors {
            self.vectors.entry(k).or_insert(v);
        }
        for (k, v) in other.tokens {
            self.tokens.entry(k).or_insert(v);
        }
        Ok(())
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(32 + self.vectors.len() * (32 + DIM * 2));
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(self.version.len() as u32).to_le_bytes());
        out.extend_from_slice(self.version.as_bytes());
        out.extend_from_slice(&(DIM as u32).to_le_bytes());
        out.extend_from_slice(&(self.vectors.len() as u32).to_le_bytes());
        for (key, vector) in &self.vectors {
            out.extend_from_slice(key);
            for &x in vector {
                out.extend_from_slice(&f32_to_f16(x).to_le_bytes());
            }
        }
        out.extend_from_slice(&(self.tokens.len() as u32).to_le_bytes());
        for (key, count) in &self.tokens {
            out.extend_from_slice(key);
            out.extend_from_slice(&count.to_le_bytes());
        }
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, EmbedError> {
        let bad = |what: &str| EmbedError::Load(format!("not a valid recording: {what}"));
        let mut at = 0usize;
        let mut take = |n: usize| -> Result<&[u8], EmbedError> {
            let slice = bytes.get(at..at + n).ok_or_else(|| bad("it ends early"))?;
            at += n;
            Ok(slice)
        };
        if take(8)? != MAGIC {
            return Err(bad("wrong magic"));
        }
        let u32_at = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        let len = u32_at(take(4)?) as usize;
        let version = String::from_utf8(take(len)?.to_vec()).map_err(|_| bad("version"))?;
        let dim = u32_at(take(4)?) as usize;
        if dim != DIM {
            return Err(bad(&format!("{dim}-d vectors, expected {DIM}")));
        }
        let count = u32_at(take(4)?) as usize;
        let mut vectors = std::collections::BTreeMap::new();
        for _ in 0..count {
            let key: Key = take(32)?.try_into().map_err(|_| bad("key"))?;
            let values = take(DIM * 2)?;
            let vector = values
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| f16_to_f32(u16::from_le_bytes(*p)))
                .collect();
            // Unit length again after the rounding to f16, as `Embedder` promises.
            vectors.insert(key, normalized(vector));
        }
        let count = u32_at(take(4)?) as usize;
        let mut tokens = std::collections::BTreeMap::new();
        for _ in 0..count {
            let key: Key = take(32)?.try_into().map_err(|_| bad("key"))?;
            tokens.insert(key, u32_at(take(4)?));
        }
        if at != bytes.len() {
            return Err(bad("trailing bytes"));
        }
        Ok(Self {
            version,
            vectors,
            tokens,
        })
    }

    pub fn load(path: &std::path::Path) -> Result<Self, EmbedError> {
        let bytes = std::fs::read(path)
            .map_err(|e| EmbedError::Load(format!("{}: {e}", path.display())))?;
        Self::from_bytes(&bytes)
    }

    /// Writes the file (through a temporary name, so a reader never sees half of it).
    pub fn save(&self, path: &std::path::Path) -> Result<(), EmbedError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let part = path.with_extension("part");
        std::fs::write(&part, self.to_bytes())?;
        std::fs::rename(&part, path)?;
        Ok(())
    }
}

/// Wraps a real embedder and records every answer it gives (vectors and token counts),
/// for `margin-eval record-embeddings`: run the chunker and the golden queries through a
/// `Recorder` around the Core ML embedder, then `recording().save(..)`.
pub struct Recorder<E: Embedder> {
    inner: E,
    recording: std::sync::Mutex<Recording>,
}

impl<E: Embedder> Recorder<E> {
    pub fn new(inner: E) -> Self {
        Self {
            inner,
            recording: std::sync::Mutex::new(Recording::new()),
        }
    }

    /// What has been recorded so far.
    pub fn recording(&self) -> Recording {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Recording> {
        self.recording.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl<E: Embedder> Embedder for Recorder<E> {
    fn embed(&self, texts: &[String], purpose: Purpose) -> Result<Vec<Vec<f32>>, EmbedError> {
        let vectors = self.inner.embed(texts, purpose.clone())?;
        let mut recording = self.lock();
        for (text, vector) in texts.iter().zip(&vectors) {
            recording
                .vectors
                .insert(text_key(text, &purpose), vector.clone());
        }
        Ok(vectors)
    }

    fn count_tokens(&self, text: &str) -> usize {
        let count = self.inner.count_tokens(text);
        self.lock().tokens.insert(tokens_key(text), count as u32);
        count
    }

    fn embed_images(
        &self,
        images: &[PathBuf],
        tokens: ImageTokens,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        let vectors = self.inner.embed_images(images, tokens)?;
        let keys = images
            .iter()
            .map(|p| Ok(image_key(&read_image(p)?, tokens)))
            .collect::<Result<Vec<_>, EmbedError>>()?;
        let mut recording = self.lock();
        for (key, vector) in keys.into_iter().zip(&vectors) {
            recording.vectors.insert(key, vector.clone());
        }
        Ok(vectors)
    }

    fn embed_audio(&self, windows: &[Vec<f32>]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let vectors = self.inner.embed_audio(windows)?;
        let mut recording = self.lock();
        for (window, vector) in windows.iter().zip(&vectors) {
            recording.vectors.insert(audio_key(window), vector.clone());
        }
        Ok(vectors)
    }

    fn embed_pipelined(
        &self,
        inputs: &[(String, Purpose)],
        batch: usize,
        on_batch: &mut dyn FnMut(usize, Vec<Vec<f32>>) -> Result<(), EmbedError>,
    ) -> Result<(), EmbedError> {
        self.inner
            .embed_pipelined(inputs, batch, &mut |first, vectors| {
                {
                    let mut recording = self.lock();
                    for ((text, purpose), vector) in inputs[first..].iter().zip(&vectors) {
                        recording
                            .vectors
                            .insert(text_key(text, purpose), vector.clone());
                    }
                }
                on_batch(first, vectors)
            })
    }
}

/// Real EmbeddingGemma vectors recorded once by `margin-eval record-embeddings`
/// (`evals/recorded/embeddings.bin`, f16, keyed by `embedder_version + sha256(prefix +
/// text)`; see `Recording`), served in CI so retrieval metrics are real without the Core
/// ML model. Anything that wasn't recorded is an error (a vector) or a panic (a token
/// count, which has no error to return), never a made-up answer, so a chunker change
/// that produces new chunk texts requires a deliberate re-record.
#[derive(Debug)]
pub struct RecordedEmbedder {
    recording: Recording,
}

impl RecordedEmbedder {
    /// Opens a recording made for this `EMBEDDER_VERSION`.
    pub fn open(path: &std::path::Path) -> Result<Self, EmbedError> {
        Self::new(Recording::load(path)?)
    }

    /// Serves `recording`, which must be of this `EMBEDDER_VERSION`.
    pub fn new(recording: Recording) -> Result<Self, EmbedError> {
        if recording.version != crate::EMBEDDER_VERSION {
            return Err(EmbedError::Load(format!(
                "the recording is of {}, this build embeds with {}: {RERECORD}",
                recording.version,
                crate::EMBEDDER_VERSION
            )));
        }
        Ok(Self { recording })
    }

    fn vector(&self, key: &Key, what: impl FnOnce() -> String) -> Result<Vec<f32>, EmbedError> {
        self.recording
            .vectors
            .get(key)
            .cloned()
            .ok_or_else(|| EmbedError::Embed(format!("{} wasn't recorded: {RERECORD}", what())))
    }
}

/// The start of `text` for a message.
fn preview(text: &str) -> String {
    let short: String = text.chars().take(60).collect();
    if short.len() < text.len() {
        format!("{short:?}…")
    } else {
        format!("{short:?}")
    }
}

impl Embedder for RecordedEmbedder {
    fn embed(&self, texts: &[String], purpose: Purpose) -> Result<Vec<Vec<f32>>, EmbedError> {
        texts
            .iter()
            .map(|text| {
                self.vector(&text_key(text, &purpose), || {
                    format!("the text {} ({:?})", preview(text), crate::prompt(&purpose))
                })
            })
            .collect()
    }

    fn count_tokens(&self, text: &str) -> usize {
        match self.recording.tokens.get(&tokens_key(text)) {
            Some(&count) => count as usize,
            None => panic!(
                "RecordedEmbedder: the token count of {} wasn't recorded (the chunker \
                 changed?): {RERECORD}",
                preview(text)
            ),
        }
    }

    fn embed_images(
        &self,
        images: &[PathBuf],
        tokens: ImageTokens,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        images
            .iter()
            .map(|path| {
                let key = image_key(&read_image(path)?, tokens);
                self.vector(&key, || {
                    format!("the image {} ({tokens:?})", path.display())
                })
            })
            .collect()
    }

    fn embed_audio(&self, windows: &[Vec<f32>]) -> Result<Vec<Vec<f32>>, EmbedError> {
        windows
            .iter()
            .map(|w| {
                self.vector(&audio_key(w), || {
                    format!("an audio window of {} samples", w.len())
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cosine;

    #[test]
    fn half_precision_matches_the_half_crate() {
        let mut values = vec![
            0.0f32,
            -0.0,
            1.0,
            -1.0,
            0.5,
            65504.0,
            65520.0,
            1e6,
            -1e6,
            6.1e-5,
            5.96e-8,
            2.98e-8,
            1e-9,
            0.1,
            0.33333,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ];
        // A sweep through the range embeddings live in, and around rounding ties.
        values.extend((0..20_000).map(|i| (i as f32 - 10_000.0) * 1.37e-4));
        values.extend((0..2_000).map(|i| 1.0 + i as f32 * 2f32.powi(-11) / 2.0));
        for x in values {
            let ours = f32_to_f16(x);
            #[cfg(target_os = "macos")]
            assert_eq!(ours, half::f16::from_f32(x).to_bits(), "{x}");
            let back = f16_to_f32(ours);
            #[cfg(target_os = "macos")]
            assert_eq!(
                back.to_bits(),
                half::f16::from_bits(ours).to_f32().to_bits(),
                "{x}"
            );
            if x.is_finite() && x.abs() < 60_000.0 {
                assert!((back - x).abs() <= x.abs() * 1e-3 + 6e-8, "{x} -> {back}");
            }
        }
        assert!(f16_to_f32(f32_to_f16(f32::NAN)).is_nan());
    }

    #[test]
    fn a_recording_replays_what_was_recorded_and_refuses_the_rest() {
        let dir = std::env::temp_dir().join(format!("margin-embed-rec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("diagram.png");
        std::fs::write(&image, "Fourier series diagram").unwrap();

        let recorder = Recorder::new(FakeEmbedder);
        let texts = vec![
            "The Calvin cycle fixes carbon.".to_string(),
            "Gibbs".to_string(),
        ];
        let titled = Purpose::document(Some("Photosynthesis > Calvin cycle"));
        let docs = recorder.embed(&texts, titled.clone()).unwrap();
        let query = recorder.embed(&texts[..1], Purpose::Query).unwrap();
        let pictures = recorder
            .embed_images(std::slice::from_ref(&image), ImageTokens::High)
            .unwrap();
        let sound = recorder.embed_audio(&[vec![0.1, 0.2]]).unwrap();
        assert_eq!(recorder.count_tokens("three little words"), 3);
        let inputs = vec![
            ("piped one".to_string(), Purpose::DOCUMENT),
            ("piped two".to_string(), Purpose::Query),
        ];
        let mut piped = Vec::new();
        recorder
            .embed_pipelined(&inputs, 1, &mut |_, v| {
                piped.extend(v);
                Ok(())
            })
            .unwrap();

        let path = dir.join("recorded/embeddings.bin");
        let recording = recorder.recording();
        assert_eq!(recording.len(), (7, 1));
        recording.save(&path).unwrap();
        // Deterministic bytes.
        assert_eq!(std::fs::read(&path).unwrap(), recording.to_bytes());

        let replay = RecordedEmbedder::open(&path).unwrap();
        let close = |a: &[Vec<f32>], b: &[Vec<f32>]| {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| cosine(x, y) > 0.9999)
        };
        assert!(close(&replay.embed(&texts, titled.clone()).unwrap(), &docs));
        assert!(close(
            &replay.embed(&texts[..1], Purpose::Query).unwrap(),
            &query
        ));
        assert!(close(
            &replay
                .embed_images(std::slice::from_ref(&image), ImageTokens::High)
                .unwrap(),
            &pictures
        ));
        assert!(close(
            &replay.embed_audio(&[vec![0.1, 0.2]]).unwrap(),
            &sound
        ));
        assert_eq!(replay.count_tokens("three little words"), 3);
        let mut replayed = Vec::new();
        replay
            .embed_pipelined(&inputs, 2, &mut |_, v| {
                replayed.extend(v);
                Ok(())
            })
            .unwrap();
        assert!(close(&replayed, &piped));

        // The same text with another title, as a query, or new text: a loud miss.
        for (text, purpose) in [
            (&texts[0], Purpose::DOCUMENT),
            (&texts[1], Purpose::Query),
            (&"never seen".to_string(), titled),
        ] {
            let err = replay
                .embed(std::slice::from_ref(text), purpose)
                .unwrap_err();
            assert!(
                err.to_string().contains("margin-eval record-embeddings"),
                "{err}"
            );
        }
        assert!(
            replay
                .embed_images(std::slice::from_ref(&image), ImageTokens::Low)
                .is_err()
        );
        assert!(replay.embed_audio(&[vec![0.3]]).is_err());
        let miss = std::panic::catch_unwind(|| replay.count_tokens("four little new words"));
        assert!(miss.is_err());

        // Another embedder version is refused; so is a damaged file.
        let mut old = recording.clone();
        old.version = "embeddinggemma-2-coreml/title-none".into();
        let err = RecordedEmbedder::new(old.clone()).unwrap_err();
        assert!(err.to_string().contains("record-embeddings"), "{err}");
        let mut merged = Recording::new();
        assert!(merged.merge(old).is_err());
        merged.merge(recording.clone()).unwrap();
        assert_eq!(merged, recording);
        let bytes = recording.to_bytes();
        assert!(Recording::from_bytes(&bytes[..bytes.len() - 1]).is_err());
        assert!(Recording::from_bytes(b"MRGNEMB0").is_err());
        assert!(RecordedEmbedder::open(&dir.join("missing.bin")).is_err());
    }

    #[test]
    fn fake_vectors_are_unit_length_and_deterministic() {
        let dir = std::env::temp_dir().join(format!("margin-embed-fake-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text_image = dir.join("diagram.png");
        std::fs::write(&text_image, "Fourier series diagram").unwrap();
        let binary_image = dir.join("photo.png");
        std::fs::write(&binary_image, [0x89, 0x50, 0xff, 0x00]).unwrap();

        let images = FakeEmbedder
            .embed_images(&[text_image.clone(), binary_image], ImageTokens::High)
            .unwrap();
        let query = &FakeEmbedder
            .embed(&["Fourier diagram".into()], Purpose::Query)
            .unwrap()[0];
        assert!(cosine(query, &images[0]) > 0.5);
        assert!(cosine(query, &images[1]) < 0.1);
        for v in &images {
            assert!((v.iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-5);
        }
        let audio = FakeEmbedder
            .embed_audio(&[vec![0.1, 0.2], vec![0.1, 0.2], vec![0.3]])
            .unwrap();
        assert_eq!(audio[0], audio[1]);
        assert_eq!(audio[0].len(), DIM);
        assert!(
            FakeEmbedder
                .embed_images(&[dir.join("missing.png")], ImageTokens::High)
                .is_err()
        );
    }
}
