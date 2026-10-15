//! The ONNX audio encoder (`onnx/audio_encoder*.onnx`): log-mel features (`mel.rs`) of
//! up to one 10 s window in, `audio_features` rows out, as Transformers.js runs Gemma's
//! audio encoder (`input_features` `[1, frames, 128]` and `input_features_mask`
//! `[1, frames]`, `audio_features` read back). The window's tokens are its valid mel
//! frames through two stride-2 convolutions (`mel::audio_token_count`: 25 per second,
//! 250 for a full window); those rows go into the text graph at `<|audio>` + that many
//! audio tokens + `<audio|>` (`mod.rs`).
//!
//! Input and output names are read from the graph when it loads; a batch axis is added
//! when the graph has one. `real_model_embeds_corpus_pictures_and_speech` (`tests/corpus.rs`) checks them
//! on the real export.

use std::path::Path;

use ort::session::Session;
use ort::value::TensorElementType;

use super::mel::{self, MEL_BINS, MelFeatures};
use super::vision::{Port, output_name, run_rows, take_rows};
use crate::{EmbedError, WINDOW_SAMPLES};

/// The encoder: one clip's features in, its rows of `FEATURE_WIDTH` out (row-major; at
/// least `mel::audio_token_count` of them).
pub(crate) trait AudioGraph: Send {
    fn encode(&mut self, features: &MelFeatures) -> Result<Vec<f32>, EmbedError>;
}

/// The encoder rows of one window of 16 kHz mono `samples` (cut to `WINDOW_SAMPLES`):
/// `(rows, count)`.
pub(crate) fn window_rows(
    graph: &mut dyn AudioGraph,
    samples: &[f32],
) -> Result<(Vec<f32>, usize), EmbedError> {
    let samples = &samples[..samples.len().min(WINDOW_SAMPLES)];
    let count = mel::audio_token_count(samples.len());
    if count == 0 {
        return Err(EmbedError::Embed("the audio is too short to embed".into()));
    }
    let features = mel::features(samples);
    let rows = take_rows(graph.encode(&features)?, count, "audio")?;
    Ok((rows, count))
}

fn is_float(ty: TensorElementType) -> bool {
    matches!(ty, TensorElementType::Float32 | TensorElementType::Float16)
}

/// EmbeddingGemma 2's audio encoder.
pub(crate) struct OrtAudioGraph {
    session: Session,
    features: Port,
    mask: Option<Port>,
    output: String,
}

impl OrtAudioGraph {
    pub(crate) fn load(path: &Path, threads: usize) -> Result<Self, EmbedError> {
        let session = super::session::load(path, threads)?;
        let features = Port::of(&session, |n, ty, _| n == "input_features" && is_float(ty))
            .or_else(|| Port::of(&session, |n, ty, _| is_float(ty) && !n.contains("mask")))
            .ok_or_else(|| EmbedError::Load("the audio encoder has no feature input".into()))?;
        if features
            .shape
            .last()
            .is_some_and(|w| *w > 0 && *w as usize != MEL_BINS)
        {
            return Err(EmbedError::Load(format!(
                "the audio encoder takes {:?} mel bins, expected {MEL_BINS}",
                features.shape.last()
            )));
        }
        let mask = Port::of(&session, |n, _, _| n != features.name && n.contains("mask"));
        let output = output_name(&session, "audio_features")?;
        Ok(Self {
            session,
            features,
            mask,
            output,
        })
    }
}

impl AudioGraph for OrtAudioGraph {
    fn encode(&mut self, features: &MelFeatures) -> Result<Vec<f32>, EmbedError> {
        let frames = features.frames();
        let mut inputs = vec![(
            self.features.name.clone(),
            self.features
                .floats(&[frames, MEL_BINS], features.values.clone())?
                .into(),
        )];
        if let Some(mask) = &self.mask {
            let values = features.mask.iter().map(|m| *m as i64).collect();
            inputs.push((mask.name.clone(), mask.ints(&[frames], values)?.into()));
        }
        run_rows(&mut self.session, inputs, &self.output.clone())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::onnx::FEATURE_WIDTH;

    /// A stand-in encoder: one row per 4 frames (rounded up), each the mean of its
    /// frames' features; records the frame counts it saw.
    #[derive(Default)]
    pub(crate) struct StubAudio {
        pub seen: Vec<(usize, usize)>,
    }

    impl AudioGraph for StubAudio {
        fn encode(&mut self, features: &MelFeatures) -> Result<Vec<f32>, EmbedError> {
            self.seen.push((features.frames(), features.valid_frames()));
            let rows = features.frames().div_ceil(4);
            let mut out = vec![0f32; rows * FEATURE_WIDTH];
            for r in 0..rows {
                let frames = &features.values
                    [r * 4 * MEL_BINS..((r * 4 + 4) * MEL_BINS).min(features.values.len())];
                out[r * FEATURE_WIDTH] = frames.iter().sum::<f32>() / frames.len() as f32;
                out[r * FEATURE_WIDTH + 1] = r as f32;
            }
            Ok(out)
        }
    }

    #[test]
    fn a_window_becomes_its_token_rows() {
        let fixture = crate::onnx::mel::tests::fixture();
        let mut stub = StubAudio::default();
        let (rows, count) = window_rows(&mut stub, &fixture.samples).unwrap();
        // 40 037 samples: 250 valid frames, 63 tokens.
        assert_eq!(stub.seen, vec![(250, 250)]);
        assert_eq!(count, 63);
        assert_eq!(rows.len(), 63 * FEATURE_WIDTH);
        // Longer input is cut to one window: 999 frames, 250 tokens.
        let long = vec![0.1f32; WINDOW_SAMPLES + 50_000];
        let (_, count) = window_rows(&mut stub, &long).unwrap();
        assert_eq!(count, 250);
        assert_eq!(stub.seen[1], (999, 999));
        // Under one frame is too short.
        assert!(window_rows(&mut stub, &[0.0; 100]).is_err());
        // An encoder returning too few rows is an error.
        struct Short;
        impl AudioGraph for Short {
            fn encode(&mut self, _: &MelFeatures) -> Result<Vec<f32>, EmbedError> {
                Ok(vec![0.0; FEATURE_WIDTH])
            }
        }
        assert!(window_rows(&mut Short, &fixture.samples).is_err());
    }
}
