//! Audio and video with AVFoundation.
//!
//! - Sound: every audio track of a file (an audio file, or a video's soundtrack) mixed
//!   and converted by `AVAssetReaderAudioMixOutput` to 16 kHz mono 32-bit float PCM, the
//!   format the audio encoder and the speech recognizer take.
//! - Frames: `AVAssetImageGenerator`, with the track's preferred transform applied (so
//!   portrait phone videos are upright), at most 1280 px on the longer side.

use std::path::Path;
use std::sync::mpsc;

use block2::RcBlock;
use objc2::Message;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVAsset, AVAssetImageGenerator, AVAssetReader, AVAssetReaderAudioMixOutput,
    AVAssetReaderOutput, AVAssetReaderStatus, AVAssetTrack, AVMediaType, AVMediaTypeAudio,
    AVMediaTypeVideo, AVURLAsset,
};
use objc2_core_foundation::{CFRetained, CGSize, Type};
use objc2_core_graphics::CGImage;
use objc2_core_media::CMTime;
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSString, NSURL};

use crate::{EmbedError, SAMPLE_RATE};

/// Longer side of a sampled video frame, in pixels.
pub(crate) const FRAME_MAX_SIDE: f64 = 1280.0;

/// `kAudioFormatLinearPCM` ('lpcm').
const LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");

fn asset(path: &Path) -> Result<Retained<AVURLAsset>, EmbedError> {
    let url = NSURL::from_file_path(path)
        .ok_or_else(|| EmbedError::Extract(format!("bad path {}", path.display())))?;
    Ok(unsafe { AVURLAsset::URLAssetWithURL_options(&url, None) })
}

fn describe(error: &NSError) -> String {
    error.localizedDescription().to_string()
}

/// The asset's tracks of `kind`, loaded asynchronously and waited for.
fn tracks(
    asset: &AVAsset,
    kind: &AVMediaType,
) -> Result<Retained<NSArray<AVAssetTrack>>, EmbedError> {
    let (send, receive) = mpsc::channel();
    let handler = RcBlock::new(
        move |tracks: *mut NSArray<AVAssetTrack>, error: *mut NSError| {
            // SAFETY: AVFoundation passes either the tracks or an error, alive for the call;
            // retaining the array keeps it past it.
            let result = match (unsafe { tracks.as_ref() }, unsafe { error.as_ref() }) {
                (Some(tracks), _) => Ok(tracks.retain()),
                (None, Some(error)) => Err(describe(error)),
                (None, None) => Err("no tracks".to_string()),
            };
            let _ = send.send(SendTracks(result));
        },
    );
    unsafe { asset.loadTracksWithMediaType_completionHandler(kind, &handler) };
    let SendTracks(result) = receive
        .recv()
        .map_err(|_| EmbedError::Extract("AVFoundation never answered".into()))?;
    result.map_err(|e| EmbedError::Extract(format!("couldn't read the file: {e}")))
}

/// Carries the loaded tracks from AVFoundation's queue to the waiting thread.
struct SendTracks(Result<Retained<NSArray<AVAssetTrack>>, String>);
// SAFETY: the array is immutable and only touched by the receiving thread once sent.
unsafe impl Send for SendTracks {}

fn media_type(kind: Option<&'static AVMediaType>) -> Result<&'static AVMediaType, EmbedError> {
    kind.ok_or_else(|| EmbedError::Extract("AVFoundation's media types are missing".into()))
}

/// The file's sound as 16 kHz mono samples; `None` when it has no audio track.
pub(crate) fn sound(path: &Path) -> Result<Option<Vec<f32>>, EmbedError> {
    autoreleasepool(|_| {
        let asset = asset(path)?;
        let audio = tracks(&asset, media_type(unsafe { AVMediaTypeAudio })?)?;
        if audio.count() == 0 {
            return Ok(None);
        }
        let reader = unsafe { AVAssetReader::assetReaderWithAsset_error(&asset) }
            .map_err(|e| EmbedError::Extract(format!("can't read the sound: {}", describe(&e))))?;
        let keys = [
            NSString::from_str("AVFormatIDKey"),
            NSString::from_str("AVSampleRateKey"),
            NSString::from_str("AVNumberOfChannelsKey"),
            NSString::from_str("AVLinearPCMBitDepthKey"),
            NSString::from_str("AVLinearPCMIsFloatKey"),
            NSString::from_str("AVLinearPCMIsBigEndianKey"),
            NSString::from_str("AVLinearPCMIsNonInterleaved"),
        ];
        let values = [
            NSNumber::new_u32(LINEAR_PCM),
            NSNumber::new_f64(SAMPLE_RATE as f64),
            NSNumber::new_u32(1),
            NSNumber::new_u32(32),
            NSNumber::new_bool(true),
            NSNumber::new_bool(false),
            NSNumber::new_bool(false),
        ];
        let keys: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
        let values: Vec<&AnyObject> = values
            .iter()
            .map(|v| AsRef::<AnyObject>::as_ref(&**v))
            .collect();
        let settings = NSDictionary::from_slices(&keys, &values);
        let output = unsafe {
            AVAssetReaderAudioMixOutput::assetReaderAudioMixOutputWithAudioTracks_audioSettings(
                &audio,
                Some(&settings),
            )
        };
        let output: &AVAssetReaderOutput = &output;
        if !unsafe { reader.canAddOutput(output) } {
            return Err(EmbedError::Extract(
                "can't convert the sound to 16 kHz mono".into(),
            ));
        }
        unsafe { reader.addOutput(output) };
        if !unsafe { reader.startReading() } {
            let why = unsafe { reader.error() }
                .map(|e| describe(&e))
                .unwrap_or_default();
            return Err(EmbedError::Extract(format!("can't read the sound: {why}")));
        }
        let mut samples: Vec<f32> = Vec::new();
        loop {
            let more = autoreleasepool(|_| {
                let Some(buffer) = (unsafe { output.copyNextSampleBuffer() }) else {
                    return false;
                };
                if let Some(block) = unsafe { buffer.data_buffer() } {
                    let length = unsafe { block.data_length() };
                    let start = samples.len();
                    samples.resize(start + length / 4, 0.0);
                    // SAFETY: the destination holds `length / 4` floats.
                    let status = unsafe {
                        block.copy_data_bytes(
                            0,
                            (length / 4) * 4,
                            std::ptr::NonNull::new_unchecked(samples[start..].as_mut_ptr().cast()),
                        )
                    };
                    if status != 0 {
                        samples.truncate(start);
                    }
                }
                true
            });
            if !more {
                break;
            }
        }
        if unsafe { reader.status() } == AVAssetReaderStatus::Failed {
            let why = unsafe { reader.error() }
                .map(|e| describe(&e))
                .unwrap_or_default();
            return Err(EmbedError::Extract(format!(
                "couldn't decode the sound: {why}"
            )));
        }
        // Float PCM out of AAC decoding and resampling can overshoot full scale slightly
        // (and a broken frame can hold NaN); the encoders expect [-1, 1].
        for s in &mut samples {
            *s = if s.is_finite() { s.clamp(-1.0, 1.0) } else { 0.0 };
        }
        Ok(Some(samples))
    })
}

/// Length of the file's media in milliseconds.
pub(crate) fn duration_ms(path: &Path) -> Result<u64, EmbedError> {
    autoreleasepool(|_| {
        let asset = asset(path)?;
        let seconds = unsafe { asset.duration().seconds() };
        if !seconds.is_finite() || seconds < 0.0 {
            return Err(EmbedError::Extract("the video has no length".into()));
        }
        Ok((seconds * 1000.0).round() as u64)
    })
}

/// Whether the file has a video track.
pub(crate) fn has_video(path: &Path) -> Result<bool, EmbedError> {
    autoreleasepool(|_| {
        let asset = asset(path)?;
        Ok(tracks(&asset, media_type(unsafe { AVMediaTypeVideo })?)?.count() > 0)
    })
}

/// Pictures of the video at `times_ms`, upright and at most `FRAME_MAX_SIDE` px, each
/// passed to `each(time_ms, image)` in order. A time the video has no picture for is
/// skipped with a warning.
pub(crate) fn frames(
    path: &Path,
    times_ms: &[u64],
    each: &mut dyn FnMut(u64, &CGImage) -> Result<(), EmbedError>,
) -> Result<(), EmbedError> {
    autoreleasepool(|_| {
        let asset = asset(path)?;
        let generator = unsafe { AVAssetImageGenerator::assetImageGeneratorWithAsset(&asset) };
        unsafe {
            generator.setAppliesPreferredTrackTransform(true);
            generator.setMaximumSize(CGSize::new(FRAME_MAX_SIDE, FRAME_MAX_SIDE));
            // Within a quarter second of the time asked for is close enough and much
            // faster than decoding up to the exact frame.
            let tolerance = CMTime::with_seconds(0.25, 600);
            generator.setRequestedTimeToleranceBefore(tolerance);
            generator.setRequestedTimeToleranceAfter(tolerance);
        }
        for &ms in times_ms {
            let (send, receive) = mpsc::channel();
            let handler = RcBlock::new(
                move |image: *mut CGImage, _actual: CMTime, error: *mut NSError| {
                    // SAFETY: AVFoundation passes an image or an error, alive for the call;
                    // retaining the image keeps it past it.
                    let result = match (unsafe { image.as_ref() }, unsafe { error.as_ref() }) {
                        (Some(image), _) => Ok(Type::retain(image)),
                        (None, Some(error)) => Err(describe(error)),
                        (None, None) => Err("no picture".to_string()),
                    };
                    let _ = send.send(SendImage(result));
                },
            );
            let time = unsafe { CMTime::with_seconds(ms as f64 / 1000.0, 600) };
            unsafe {
                generator.generateCGImageAsynchronouslyForTime_completionHandler(time, &handler)
            };
            let SendImage(result) = receive
                .recv()
                .map_err(|_| EmbedError::Extract("AVFoundation never answered".into()))?;
            match result {
                Ok(image) => each(ms, &image)?,
                Err(e) => tracing::warn!("no video frame at {ms} ms: {e}"),
            }
        }
        Ok(())
    })
}

/// Carries a generated frame from AVFoundation's queue to the waiting thread.
struct SendImage(Result<CFRetained<CGImage>, String>);
// SAFETY: a CGImage is immutable and thread-safe.
unsafe impl Send for SendImage {}
