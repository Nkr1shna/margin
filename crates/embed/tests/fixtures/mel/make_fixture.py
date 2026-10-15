"""Records the reference log-mel features `onnx/mel.rs` is tested against.

The reference follows `Gemma4AudioFeatureExtractor` (transformers,
`models/gemma4/feature_extraction_gemma4.py`) step by step for one clip, using
transformers' own `window_function` and `mel_filter_bank` (so the filter bank and the
window are not ours): pad to a multiple of 128 samples (`pad_to_multiple_of`), 160 zeros
on the left (semicausal), frames of 321 samples every 160, the first 320 of each (no
preemphasis), periodic Hann, `rfft(n=512)`, magnitude, HTK mel (128 bins, 0-8 kHz, no
norm), `log(mel + 0.001)`, the frame mask read at each frame's last sample, and masked
frames zeroed.

Usage (macOS, from this folder):

    say -v Samantha -o speech.aiff "The Carnot cycle has two isothermal and two adiabatic steps."
    afconvert -f WAVE -d LEI16@16000 -c 1 speech.aiff speech.wav
    python3 make_fixture.py speech.wav speech.mel.bin

Output layout (little-endian): b"MELF", u32 version (1), u32 samples, u32 frames,
u32 bins, then the samples as i16, the features as f32 (frames x bins, row-major), and
the mask as one byte per frame.
"""

import struct
import sys
import wave

import numpy as np
from transformers.audio_utils import mel_filter_bank, window_function

SAMPLE_RATE = 16_000
FRAME_LENGTH = 320
HOP = 160
FFT = 512
BINS = 128
MEL_FLOOR = 1e-3
PAD_MULTIPLE = 128
# 2.5 s and a bit: a length that isn't a multiple of 128, so padding is exercised.
KEEP = 40_037


def features(samples):
    n = len(samples)
    padded_len = -(-n // PAD_MULTIPLE) * PAD_MULTIPLE
    wav = np.zeros(padded_len, dtype=np.float64)
    wav[:n] = samples
    attention = np.zeros(padded_len, dtype=np.int32)
    attention[:n] = 1

    pad_left = FRAME_LENGTH // 2
    wav = np.concatenate([np.zeros(pad_left), wav])
    attention = np.concatenate([np.zeros(pad_left, dtype=np.int32), attention])

    size = FRAME_LENGTH + 1
    frames = (len(wav) - size) // HOP + 1
    starts = np.arange(frames) * HOP
    unfolded = np.stack([wav[s : s + size] for s in starts])
    frames_to_process = unfolded[:, :-1]

    window = window_function(FRAME_LENGTH).astype(np.float32)
    stft = np.fft.rfft(frames_to_process * window, n=FFT, axis=-1)
    magnitude = np.abs(stft)
    mel = mel_filter_bank(
        num_frequency_bins=FFT // 2 + 1,
        num_mel_filters=BINS,
        min_frequency=0.0,
        max_frequency=8000.0,
        sampling_rate=SAMPLE_RATE,
        norm=None,
        mel_scale="htk",
    )
    log_mel = np.log(magnitude @ mel + MEL_FLOOR)
    ends = starts + size - 1
    mask = attention[ends].astype(bool)
    log_mel = (log_mel * mask[:, None]).astype(np.float32)
    return log_mel, mask


def main(src, out):
    with wave.open(src, "rb") as w:
        assert w.getframerate() == SAMPLE_RATE and w.getnchannels() == 1
        assert w.getsampwidth() == 2
        raw = np.frombuffer(w.readframes(w.getnframes()), dtype="<i2")[:KEEP]
    samples = raw.astype(np.float64) / 32768.0
    log_mel, mask = features(samples)
    with open(out, "wb") as f:
        f.write(b"MELF")
        f.write(struct.pack("<IIII", 1, len(raw), log_mel.shape[0], log_mel.shape[1]))
        f.write(raw.astype("<i2").tobytes())
        f.write(log_mel.astype("<f4").tobytes())
        f.write(mask.astype(np.uint8).tobytes())
    print(f"{len(raw)} samples, {log_mel.shape[0]} frames, {int(mask.sum())} valid")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
