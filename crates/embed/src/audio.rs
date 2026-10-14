//! The audio encoder, `EmbeddingGemma2Audio.mlpackage`, as FluidUse's
//! `EmbeddingGemma2Audio.swift` runs it:
//!
//! - `waveform` [1, 160160] fp32: 160 zero samples, then up to 10 s of 16 kHz mono audio,
//!   zero-padded (the log-mel features are computed inside the model);
//! - `frame_mask` [1, 1000] fp16: frame i is valid when `i × 160 + 321 ≤ 160 + samples`;
//! - output `audio_tokens` [1, 250, 512], of which the first (valid frames + 3) / 4 are
//!   real (two stride-2 convolutions). Those go to the text model wrapped in `<|audio>` …
//!   `<audio|>` (`coreml::Model::embed_media`).
//!
//! Run on the CPU and GPU, as the model card and FluidUse do.

use std::path::Path;
use std::sync::Mutex;

use half::f16;
use objc2::rc::{Retained, autoreleasepool};
use objc2_core_ml::{MLComputeUnits, MLModel, MLMultiArrayDataType};
use serde::Deserialize;

use crate::EmbedError;
use crate::coreml::{load_package, multi_array, output_array, predict, read_rows, to_f16};

/// `config.json`'s `audio` section.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Config {
    package: String,
    sample_rate: usize,
    window_seconds: usize,
    waveform_samples: usize,
    left_pad_samples: usize,
    frames: usize,
    pub(crate) boa_token_id: u32,
    pub(crate) eoa_token_id: u32,
}

pub(crate) struct Audio {
    model: Mutex<Retained<MLModel>>,
    window: usize,
    left_pad: usize,
    frames: usize,
}

// SAFETY: `MLModel` may be used from any thread; it is only reached through the `Mutex`.
unsafe impl Send for Audio {}
unsafe impl Sync for Audio {}

impl Audio {
    pub(crate) fn load(dir: &Path, config: &Config) -> Result<Self, EmbedError> {
        let window = config.sample_rate * config.window_seconds;
        if config.sample_rate != crate::SAMPLE_RATE as usize
            || window != crate::WINDOW_SAMPLES
            || config.waveform_samples != window + config.left_pad_samples
        {
            return Err(EmbedError::Load(format!(
                "unexpected audio window: {} Hz, {} s, {} samples",
                config.sample_rate, config.window_seconds, config.waveform_samples
            )));
        }
        let mut models = load_package(dir, &config.package, &[], MLComputeUnits::CPUAndGPU)?;
        Ok(Audio {
            model: Mutex::new(models.remove(0)),
            window,
            left_pad: config.left_pad_samples,
            frames: config.frames,
        })
    }

    /// The encoder's rows for up to 10 s of 16 kHz mono samples (longer input is cut).
    pub(crate) fn encode(&self, samples: &[f32]) -> Result<Vec<f16>, EmbedError> {
        let count = samples.len().min(self.window);
        let valid_frames = valid_frames(count, self.left_pad, self.window, self.frames);
        let tokens = token_count(valid_frames);
        if tokens == 0 {
            return Err(EmbedError::Embed("the audio is too short to embed".into()));
        }
        let mut waveform = vec![0f32; self.window + self.left_pad];
        waveform[self.left_pad..self.left_pad + count].copy_from_slice(&samples[..count]);
        let mask: Vec<f16> = (0..self.frames)
            .map(|i| {
                if i < valid_frames {
                    f16::ONE
                } else {
                    f16::ZERO
                }
            })
            .collect();
        let model = self.model.lock().unwrap_or_else(|e| e.into_inner());
        autoreleasepool(|_| {
            let waveform = multi_array(
                &[1, self.window + self.left_pad],
                &waveform,
                MLMultiArrayDataType::Float32,
            )?;
            let frame_mask = multi_array(&[1, self.frames], &mask, MLMultiArrayDataType::Float16)?;
            let output = predict(
                &model,
                &[("waveform", &waveform), ("frame_mask", &frame_mask)],
            )?;
            let array = output_array(&output, "audio_tokens")?;
            let hidden = unsafe { array.shape() }
                .lastObject()
                .map_or(0, |n| n.as_isize() as usize);
            Ok(to_f16(&read_rows(&array, tokens, hidden)?))
        })
    }
}

/// Mel frames with real audio in a window of `samples` samples: frame i covers padded
/// samples [160 i, 160 i + 321) (the 160-sample left pad included) and counts only when
/// all of them are real.
fn valid_frames(samples: usize, left_pad: usize, window: usize, frames: usize) -> usize {
    let real = left_pad + samples.min(window);
    if real < 321 {
        0
    } else {
        frames.min((real - 321) / 160 + 1)
    }
}

/// Audio tokens for `valid_frames` frames: token k is valid when frame 4k is.
fn token_count(valid_frames: usize) -> usize {
    valid_frames.div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_and_tokens_follow_fluiduse() {
        let frames = |s| valid_frames(s, 160, 160_000, 1000);
        assert_eq!(frames(0), 0);
        assert_eq!(frames(160), 0);
        assert_eq!(frames(161), 1);
        // The last frame would need a sample past the window.
        assert_eq!(frames(160_000), 999);
        assert_eq!(frames(500_000), 999);
        assert_eq!(token_count(frames(160_000)), 250);
        assert_eq!(token_count(1), 1);
        assert_eq!(token_count(5), 2);
        // Half a second: 49 whole frames, 13 tokens.
        assert_eq!(frames(8000), 49);
        assert_eq!(token_count(49), 13);
    }
}
