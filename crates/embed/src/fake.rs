//! A deterministic stand-in for tests: hashed bag of words, so texts sharing words are
//! similar. Not a real model.

use crate::{DIM, EmbedError, Embedder, Purpose};

#[derive(Debug, Default, Clone)]
pub struct FakeEmbedder;

fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
}

impl Embedder for FakeEmbedder {
    fn embed(&self, texts: &[String], _purpose: Purpose) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0f32; DIM];
                for w in words(t) {
                    let h = w.bytes().fold(0xcbf29ce484222325u64, |h, b| {
                        (h ^ b as u64).wrapping_mul(0x100000001b3)
                    });
                    v[(h % DIM as u64) as usize] += 1.0;
                }
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm > 0.0 {
                    v.iter_mut().for_each(|x| *x /= norm);
                }
                v
            })
            .collect())
    }

    fn count_tokens(&self, text: &str) -> usize {
        words(text).count()
    }
}
