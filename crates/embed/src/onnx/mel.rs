//! The audio encoder's front end: log-mel features as `Gemma4AudioFeatureExtractor`
//! (transformers, `models/gemma4/feature_extraction_gemma4.py`; the ONNX export's
//! `preprocessor_config.json` holds the same defaults) computes them, reimplemented with
//! `realfft` so the ONNX audio encoder gets what it was exported with:
//!
//! - 16 kHz mono, padded with zeros to a multiple of 128 samples (`pad_to_multiple_of`);
//! - 160 zeros on the left (semicausal: `frame_length / 2`);
//! - frames of 321 samples every 160 (`unfold(size = frame_length + 1)`), of which the
//!   first 320 are used (no preemphasis);
//! - a periodic Hann window of 320, `rfft` of 512 (zero-padded), the magnitude (not the
//!   power);
//! - 128 triangular HTK mel filters over 0–8 kHz, no normalisation;
//! - `ln(mel + 0.001)` (the floor is added, not clamped);
//! - a frame is valid when its last sample (the 321st) is real audio, and invalid frames
//!   are zeroed.
//!
//! `tests/fixtures/mel/speech.mel.bin` holds features recorded from that reference in
//! Python (`make_fixture.py` beside it, with transformers' own window and filter bank);
//! `the_features_match_the_recorded_reference` compares them.

use std::sync::{Arc, OnceLock};

use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};

/// Mel bins per frame.
pub(crate) const MEL_BINS: usize = 128;
/// Samples a frame's window spans (20 ms).
pub(crate) const FRAME_LENGTH: usize = 320;
/// Samples between frames (10 ms).
pub(crate) const HOP: usize = 160;
/// FFT size: the next power of two above the frame.
pub(crate) const FFT_LENGTH: usize = 512;
/// The input is padded to a multiple of this many samples.
pub(crate) const PAD_MULTIPLE: usize = 128;
const MEL_FLOOR: f64 = 1e-3;
const MIN_FREQUENCY: f64 = 0.0;
const MAX_FREQUENCY: f64 = 8_000.0;
/// Samples one frame is cut from (`frame_length + 1`).
const UNFOLD: usize = FRAME_LENGTH + 1;
const PAD_LEFT: usize = FRAME_LENGTH / 2;

/// Log-mel features of one clip.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MelFeatures {
    /// `frames × MEL_BINS`, row-major; invalid frames are zeros.
    pub values: Vec<f32>,
    /// One per frame: whether it covers real audio.
    pub mask: Vec<bool>,
}

impl MelFeatures {
    pub fn frames(&self) -> usize {
        self.mask.len()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn valid_frames(&self) -> usize {
        self.mask.iter().filter(|m| **m).count()
    }
}

/// The window, the filter bank and the FFT plan: built once, shared.
struct FrontEnd {
    window: Vec<f64>,
    /// `(FFT_LENGTH / 2 + 1) × MEL_BINS`, row-major (frequency bin, mel bin).
    filters: Vec<f64>,
    fft: Arc<dyn RealToComplex<f64>>,
}

fn front_end() -> &'static FrontEnd {
    static FRONT: OnceLock<FrontEnd> = OnceLock::new();
    FRONT.get_or_init(|| FrontEnd {
        window: hann_window(FRAME_LENGTH),
        filters: mel_filter_bank(
            FFT_LENGTH / 2 + 1,
            MEL_BINS,
            MIN_FREQUENCY,
            MAX_FREQUENCY,
            crate::SAMPLE_RATE as f64,
        ),
        fft: RealFftPlanner::<f64>::new().plan_fft_forward(FFT_LENGTH),
    })
}

/// Periodic Hann: `0.5 − 0.5 cos(2πn / N)`, computed in f32 as the reference stores it.
fn hann_window(n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| {
            let w = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos();
            w as f32 as f64
        })
        .collect()
}

fn hz_to_mel(hz: f64) -> f64 {
    2595.0 * (1.0 + hz / 700.0).log10()
}

fn mel_to_hz(mel: f64) -> f64 {
    700.0 * (10f64.powf(mel / 2595.0) - 1.0)
}

/// `numpy.linspace(start, end, n)`.
fn linspace(start: f64, end: f64, n: usize) -> Vec<f64> {
    if n == 1 {
        return vec![start];
    }
    let step = (end - start) / (n - 1) as f64;
    let mut v: Vec<f64> = (0..n).map(|i| start + i as f64 * step).collect();
    v[n - 1] = end;
    v
}

/// transformers' `mel_filter_bank(..., norm=None, mel_scale="htk")`: triangles between
/// mel-spaced corner frequencies, evaluated at the FFT bins' frequencies.
fn mel_filter_bank(
    frequency_bins: usize,
    mel_bins: usize,
    min_hz: f64,
    max_hz: f64,
    sample_rate: f64,
) -> Vec<f64> {
    let corners: Vec<f64> = linspace(hz_to_mel(min_hz), hz_to_mel(max_hz), mel_bins + 2)
        .into_iter()
        .map(mel_to_hz)
        .collect();
    let fft_freqs = linspace(0.0, (sample_rate as usize / 2) as f64, frequency_bins);
    let mut filters = vec![0.0; frequency_bins * mel_bins];
    for (i, &f) in fft_freqs.iter().enumerate() {
        for j in 0..mel_bins {
            let down = -(corners[j] - f) / (corners[j + 1] - corners[j]);
            let up = (corners[j + 2] - f) / (corners[j + 2] - corners[j + 1]);
            filters[i * mel_bins + j] = down.min(up).max(0.0);
        }
    }
    filters
}

/// Frames a clip of `samples` samples yields (after padding to `PAD_MULTIPLE`).
pub(crate) fn frame_count(samples: usize) -> usize {
    let padded = samples.div_ceil(PAD_MULTIPLE) * PAD_MULTIPLE + PAD_LEFT;
    if padded < UNFOLD {
        0
    } else {
        (padded - UNFOLD) / HOP + 1
    }
}

/// Frames of a clip of `samples` samples that cover only real audio: frame i ends at
/// padded sample `160 i + 320`, which is real when it is below `160 + samples`.
pub(crate) fn valid_frame_count(samples: usize) -> usize {
    let real_end = PAD_LEFT + samples;
    if real_end < UNFOLD {
        return 0;
    }
    frame_count(samples).min((real_end - UNFOLD) / HOP + 1)
}

/// Audio tokens the encoder makes of a clip of `samples` samples: the valid frames
/// through two stride-2 convolutions (`Gemma4Processor._compute_audio_num_tokens`).
pub(crate) fn audio_token_count(samples: usize) -> usize {
    let mut t = valid_frame_count(samples);
    if t == 0 {
        return 0;
    }
    for _ in 0..2 {
        t = (t - 1) / 2 + 1;
    }
    t
}

/// The log-mel features of 16 kHz mono `samples`.
pub(crate) fn features(samples: &[f32]) -> MelFeatures {
    let front = front_end();
    let n = samples.len();
    let padded_len = n.div_ceil(PAD_MULTIPLE) * PAD_MULTIPLE;
    let mut wav = vec![0f64; PAD_LEFT + padded_len];
    for (dst, &s) in wav[PAD_LEFT..].iter_mut().zip(samples) {
        *dst = s as f64;
    }
    let frames = frame_count(n);
    let valid = valid_frame_count(n);
    let bins = FFT_LENGTH / 2 + 1;
    let mut values = vec![0f32; frames * MEL_BINS];
    let mut input = front.fft.make_input_vec();
    let mut spectrum: Vec<Complex<f64>> = front.fft.make_output_vec();
    let mut magnitude = vec![0f64; bins];
    for f in 0..valid {
        let start = f * HOP;
        input.iter_mut().for_each(|x| *x = 0.0);
        for (k, x) in input.iter_mut().take(FRAME_LENGTH).enumerate() {
            *x = wav[start + k] * front.window[k];
        }
        // Lengths come from the plan, so this can't fail.
        front
            .fft
            .process(&mut input, &mut spectrum)
            .expect("FFT buffers are the plan's");
        for (m, c) in magnitude.iter_mut().zip(&spectrum) {
            *m = c.norm();
        }
        let row = &mut values[f * MEL_BINS..(f + 1) * MEL_BINS];
        for (j, out) in row.iter_mut().enumerate() {
            let mut sum = 0.0;
            for (i, m) in magnitude.iter().enumerate() {
                sum += m * front.filters[i * MEL_BINS + j];
            }
            *out = (sum + MEL_FLOOR).ln() as f32;
        }
    }
    MelFeatures {
        values,
        mask: (0..frames).map(|f| f < valid).collect(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The recorded reference: the clip's samples and its features.
    pub(crate) struct Fixture {
        pub samples: Vec<f32>,
        pub features: MelFeatures,
    }

    pub(crate) fn fixture() -> Fixture {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/mel/speech.mel.bin");
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..4], b"MELF");
        let word = |i: usize| u32::from_le_bytes(bytes[4 + 4 * i..8 + 4 * i].try_into().unwrap());
        let (version, n, frames, bins) = (word(0), word(1) as usize, word(2) as usize, word(3));
        assert_eq!((version, bins as usize), (1, MEL_BINS));
        let mut at = 20;
        let samples: Vec<f32> = bytes[at..at + 2 * n]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b) as f32 / 32768.0)
            .collect();
        at += 2 * n;
        let values: Vec<f32> = bytes[at..at + 4 * frames * MEL_BINS]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        at += 4 * frames * MEL_BINS;
        let mask = bytes[at..at + frames].iter().map(|b| *b == 1).collect();
        assert_eq!(at + frames, bytes.len());
        Fixture {
            samples,
            features: MelFeatures { values, mask },
        }
    }

    #[test]
    fn the_features_match_the_recorded_reference() {
        let fixture = fixture();
        let got = features(&fixture.samples);
        let want = &fixture.features;
        assert_eq!(got.mask, want.mask);
        assert_eq!(got.values.len(), want.values.len());
        let mut worst = 0f32;
        for (g, w) in got.values.iter().zip(&want.values) {
            worst = worst.max((g - w).abs());
        }
        // Float rounding in the FFT and the filter bank only.
        assert!(worst < 2e-3, "largest difference {worst}");
        // And it isn't trivially zero: speech has energy.
        assert!(want.values.iter().any(|v| *v > 0.0));
    }

    #[test]
    fn frames_masks_and_tokens_follow_the_extractor() {
        assert_eq!(frame_count(0), 0);
        assert_eq!(valid_frame_count(0), 0);
        assert_eq!(audio_token_count(0), 0);
        assert_eq!(valid_frame_count(160), 0);
        assert_eq!(valid_frame_count(161), 1);
        // One second: 99 frames, 25 tokens (the processor's own example).
        assert_eq!(valid_frame_count(16_000), 99);
        assert_eq!(audio_token_count(16_000), 25);
        // A ten-second window: 999 frames, 250 tokens (the Core ML card's count).
        assert_eq!(frame_count(160_000), 999);
        assert_eq!(valid_frame_count(160_000), 999);
        assert_eq!(audio_token_count(160_000), 250);
        // Padding to 128 adds frames that are masked out and zero.
        let samples = 1_100;
        assert_eq!(frame_count(samples), 7);
        assert_eq!(valid_frame_count(samples), 6);
        let tone: Vec<f32> = (0..samples)
            .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 16_000.0).sin() * 0.5)
            .collect();
        let f = features(&tone);
        assert_eq!(f.frames(), 7);
        assert_eq!(f.valid_frames(), 6);
        assert_eq!(f.mask, vec![true, true, true, true, true, true, false]);
        assert!(f.values[6 * MEL_BINS..].iter().all(|v| *v == 0.0));
        // 440 Hz is 549 HTK mel; the filters are 22 mel apart, so it peaks in filter 24.
        let row = &f.values[2 * MEL_BINS..3 * MEL_BINS];
        let peak = (0..MEL_BINS)
            .max_by(|a, b| row[*a].total_cmp(&row[*b]))
            .unwrap();
        assert!((23..=25).contains(&peak), "peak bin {peak}");
        assert!(row[peak] > 0.0);
    }

    #[test]
    fn the_filter_bank_is_htk_triangles() {
        let filters = mel_filter_bank(257, MEL_BINS, 0.0, 8000.0, 16_000.0);
        // Weights within [0, 1]; the lowest filters are narrower than an FFT bin (31.25 Hz)
        // and may catch none, as in the reference; the rest all have weight.
        assert!(filters.iter().all(|w| (0.0..=1.0).contains(w)));
        for j in 8..MEL_BINS - 1 {
            assert!(
                (0..257).any(|i| filters[i * MEL_BINS + j] > 0.0),
                "filter {j}"
            );
        }
        assert!((hz_to_mel(mel_to_hz(1234.5)) - 1234.5).abs() < 1e-9);
        let w = hann_window(320);
        assert_eq!(w[0], 0.0);
        assert!((w[160] - 1.0).abs() < 1e-6);
        assert_eq!(linspace(0.0, 1.0, 3), vec![0.0, 0.5, 1.0]);
    }
}
