//! WAV files: silent narration for `Voice::Silent`, and checks on the files PocketTTS
//! writes.

/// Sample rate of the silent WAVs (PocketTTS writes 24 kHz too).
pub(crate) const SILENT_RATE: u32 = 24_000;

/// How long a line takes to say, roughly: 2.6 words a second and a breath.
/// `margin_voice.py` uses the same estimate for its collecting voice.
pub(crate) fn estimated_seconds(text: &str) -> f64 {
    (0.4 + text.split_whitespace().count() as f64 / 2.6).max(0.8)
}

/// A mono 16-bit PCM WAV of `seconds` of silence.
pub(crate) fn silent_wav(seconds: f64) -> Vec<u8> {
    let frames = (seconds * f64::from(SILENT_RATE)).round() as u32;
    let data_len = frames * 2;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&SILENT_RATE.to_le_bytes());
    out.extend_from_slice(&(SILENT_RATE * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.resize(44 + data_len as usize, 0);
    out
}

/// Whether `bytes` look like a WAV file with some audio in it.
pub(crate) fn is_wav_with_audio(bytes: &[u8]) -> bool {
    bytes.len() > 44 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silent_wav_has_a_valid_header_and_length() {
        let wav = silent_wav(1.5);
        assert!(is_wav_with_audio(&wav));
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");
        let data_len = u32::from_le_bytes(wav[40..44].try_into().unwrap());
        assert_eq!(data_len, 36_000 * 2);
        assert_eq!(wav.len(), 44 + data_len as usize);
        let riff_len = u32::from_le_bytes(wav[4..8].try_into().unwrap());
        assert_eq!(riff_len as usize, wav.len() - 8);
        assert!(wav[44..].iter().all(|&b| b == 0));
    }

    #[test]
    fn estimate_grows_with_words_and_has_a_floor() {
        assert_eq!(estimated_seconds(""), 0.8);
        assert!(
            (estimated_seconds("one two three four five six") - (0.4 + 6.0 / 2.6)).abs() < 1e-9
        );
    }
}
