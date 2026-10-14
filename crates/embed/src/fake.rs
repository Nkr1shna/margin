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

/// Real EmbeddingGemma vectors recorded once by `margin-eval record-embeddings`
/// (`evals/recorded/embeddings.bin`, f16, keyed by `embedder_version + sha256(prefix +
/// text)`), served in CI so retrieval metrics are real without the Core ML model. A text
/// that wasn't recorded is an error, never a made-up vector, so a chunker change that
/// produces new chunk texts requires a deliberate re-record.
#[derive(Debug)]
pub struct RecordedEmbedder {
    _private: (),
}

impl RecordedEmbedder {
    /// Opens a recording.
    pub fn open(_path: &std::path::Path) -> Result<Self, EmbedError> {
        unimplemented!("R1, embed: RecordedEmbedder::open")
    }
}

impl Embedder for RecordedEmbedder {
    fn embed(&self, _texts: &[String], _purpose: Purpose) -> Result<Vec<Vec<f32>>, EmbedError> {
        unimplemented!("R1, embed: RecordedEmbedder::embed")
    }

    fn count_tokens(&self, _text: &str) -> usize {
        unimplemented!("R1, embed: RecordedEmbedder::count_tokens")
    }

    fn embed_images(
        &self,
        _images: &[PathBuf],
        _tokens: ImageTokens,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        unimplemented!("R1, embed: RecordedEmbedder::embed_images")
    }

    fn embed_audio(&self, _windows: &[Vec<f32>]) -> Result<Vec<Vec<f32>>, EmbedError> {
        unimplemented!("R1, embed: RecordedEmbedder::embed_audio")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cosine;

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
