//! Audio and video off macOS, with the `ffmpeg`/`ffprobe` CLIs (apt `ffmpeg`), in place
//! of AVFoundation (`av.rs`; `docs/plan-linux-service.md` §2.5). Subprocesses, not
//! bindings, so no FFmpeg headers are needed to build.
//!
//! - Probe: `ffprobe -show_entries format=duration:stream=codec_type:…` as JSON; cover
//!   art in an audio file (`attached_pic`) isn't video.
//! - Sound: every audio stream mixed (`amix`) and converted to 16 kHz mono 32-bit float,
//!   read from ffmpeg's stdout.
//! - Frames: one `ffmpeg -ss <t> -i <file> -frames:v 1` per time, as PNG on stdout
//!   (ffmpeg turns rotated phone videos upright itself), shrunk to `FRAME_MAX_SIDE`.
// On macOS these build (the `linux-readers` feature) for their tests; Linux reads with them.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use image::DynamicImage;

use crate::{EmbedError, SAMPLE_RATE};

pub const FFMPEG: &str = "ffmpeg";
pub const FFPROBE: &str = "ffprobe";
/// Override where the programs are looked for (paths to them).
pub const FFMPEG_ENV: &str = "MARGIN_FFMPEG";
pub const FFPROBE_ENV: &str = "MARGIN_FFPROBE";

/// Longer side of a sampled video frame, in pixels (as `av.rs`).
pub(crate) const FRAME_MAX_SIDE: u32 = 1280;

/// The two programs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ffmpeg {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

/// `ffmpeg` and `ffprobe` (at `$MARGIN_FFMPEG`/`$MARGIN_FFPROBE`, on PATH or in the
/// usual install folders), when both are there.
pub fn find_ffmpeg() -> Option<Ffmpeg> {
    Some(Ffmpeg {
        ffmpeg: crate::extract::find_program(FFMPEG, FFMPEG_ENV)?,
        ffprobe: crate::extract::find_program(FFPROBE, FFPROBE_ENV)?,
    })
}

/// What a media file holds.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Probe {
    pub duration_ms: Option<u64>,
    /// Audio streams.
    pub audio: usize,
    /// Whether there is a video stream that isn't cover art.
    pub video: bool,
}

/// Reads `ffprobe -of json` output.
pub(crate) fn parse_probe(json: &[u8]) -> Result<Probe, EmbedError> {
    let value: serde_json::Value = serde_json::from_slice(json)
        .map_err(|e| EmbedError::Extract(format!("ffprobe's output: {e}")))?;
    let duration_ms = value
        .pointer("/format/duration")
        .and_then(|d| match d {
            serde_json::Value::String(s) => s.parse::<f64>().ok(),
            serde_json::Value::Number(n) => n.as_f64(),
            _ => None,
        })
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map(|s| (s * 1000.0).round() as u64);
    let mut probe = Probe {
        duration_ms,
        ..Default::default()
    };
    for stream in value
        .get("streams")
        .and_then(|s| s.as_array())
        .into_iter()
        .flatten()
    {
        let cover = stream
            .pointer("/disposition/attached_pic")
            .and_then(|v| v.as_i64())
            == Some(1);
        match stream.get("codec_type").and_then(|t| t.as_str()) {
            Some("audio") => probe.audio += 1,
            Some("video") if !cover => probe.video = true,
            _ => {}
        }
    }
    Ok(probe)
}

/// Runs `cmd`, returning its stdout, or its stderr as the error.
fn run(cmd: &mut Command, what: &str) -> Result<Vec<u8>, EmbedError> {
    let out = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| EmbedError::Extract(format!("couldn't run {what}: {e}")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        return Err(EmbedError::Extract(if err.is_empty() {
            format!("{what} failed ({})", out.status)
        } else {
            format!("{what}: {err}")
        }));
    }
    Ok(out.stdout)
}

/// Seconds for an ffmpeg time argument.
fn seconds(ms: u64) -> String {
    format!("{}.{:03}", ms / 1000, ms % 1000)
}

impl Ffmpeg {
    pub(crate) fn probe(&self, path: &Path) -> Result<Probe, EmbedError> {
        let out = run(
            Command::new(&self.ffprobe)
                .args(["-v", "error", "-show_entries"])
                .arg("format=duration:stream=codec_type:stream_disposition=attached_pic")
                .args(["-of", "json"])
                .arg(path),
            "ffprobe",
        )?;
        parse_probe(&out)
    }

    /// The arguments that decode `audio` streams of `path` to 16 kHz mono f32 on stdout.
    pub(crate) fn sound_args(path: &Path, audio: usize) -> Vec<std::ffi::OsString> {
        let mut args: Vec<std::ffi::OsString> = ["-nostdin", "-v", "error", "-i"]
            .iter()
            .map(Into::into)
            .collect();
        args.push(path.as_os_str().to_owned());
        if audio > 1 {
            let inputs: String = (0..audio).map(|i| format!("[0:a:{i}]")).collect();
            args.push("-filter_complex".into());
            args.push(format!("{inputs}amix=inputs={audio}:normalize=0[mix]").into());
            args.push("-map".into());
            args.push("[mix]".into());
        } else {
            args.push("-map".into());
            args.push("0:a:0".into());
        }
        for a in [
            "-vn",
            "-ac",
            "1",
            "-ar",
            &SAMPLE_RATE.to_string(),
            "-f",
            "f32le",
            "-",
        ] {
            args.push(a.into());
        }
        args
    }

    /// Every audio stream of `path` mixed to 16 kHz mono; `None` without audio.
    pub(crate) fn sound(&self, path: &Path, probe: &Probe) -> Result<Option<Vec<f32>>, EmbedError> {
        if probe.audio == 0 {
            return Ok(None);
        }
        let out = run(
            Command::new(&self.ffmpeg).args(Self::sound_args(path, probe.audio)),
            "ffmpeg",
        )?;
        Ok(Some(
            out.as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect(),
        ))
    }

    /// The arguments that write the frame at `ms` of `path` as PNG to stdout, its longer
    /// side at most `FRAME_MAX_SIDE` (never enlarged, even sides).
    pub(crate) fn frame_args(path: &Path, ms: u64) -> Vec<std::ffi::OsString> {
        let m = FRAME_MAX_SIDE;
        let scale =
            format!("scale='if(gte(iw,ih),min({m},iw),-2)':'if(gte(iw,ih),-2,min({m},ih))'");
        let mut args: Vec<std::ffi::OsString> =
            vec!["-nostdin".into(), "-v".into(), "error".into()];
        args.push("-ss".into());
        args.push(seconds(ms).into());
        args.push("-i".into());
        args.push(path.as_os_str().to_owned());
        for a in [
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-vf",
            &scale,
            "-f",
            "image2pipe",
            "-c:v",
            "png",
            "-",
        ] {
            args.push(a.into());
        }
        args
    }

    /// The frame at each of `times` (ms), handed to `on_frame` in order; a time with no
    /// frame (past the last) is skipped.
    pub(crate) fn frames(
        &self,
        path: &Path,
        times: &[u64],
        on_frame: &mut dyn FnMut(u64, DynamicImage) -> Result<(), EmbedError>,
    ) -> Result<(), EmbedError> {
        for &ms in times {
            let png = run(
                Command::new(&self.ffmpeg).args(Self::frame_args(path, ms)),
                "ffmpeg",
            )?;
            if png.is_empty() {
                continue;
            }
            let image = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
                .map_err(|e| EmbedError::Extract(format!("ffmpeg's frame: {e}")))?;
            on_frame(ms, crate::image_linux::shrink(image, FRAME_MAX_SIDE))?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::image_linux::tests::temp_dir;

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Stand-ins for the two programs: `ffprobe` prints `probe_json`; `ffmpeg` writes
    /// `samples` seconds of a constant 0.25 as f32 when asked for sound, and, asked for a
    /// frame, `frame_png` (empty past `last_ms`). Each records its arguments.
    pub(crate) fn fake_ffmpeg(
        dir: &Path,
        probe_json: &str,
        frame_png: &Path,
        last_ms: u64,
    ) -> Ffmpeg {
        let probe = dir.join("probe.json");
        std::fs::write(&probe, probe_json).unwrap();
        let ffprobe = script(dir, "ffprobe", &format!("cat '{}'\n", probe.display()));
        // 0.25f32 little-endian is 00 00 80 3e; 16 000 samples per second, 2 seconds.
        let ffmpeg = script(
            dir,
            "ffmpeg",
            &format!(
                "echo \"$@\" >> '{log}'\n\
                 case \"$*\" in\n\
                 *f32le*) i=0; while [ $i -lt 32000 ]; do printf '\\000\\000\\200\\076'; i=$((i+1)); done ;;\n\
                 *image2pipe*) t=$(echo \"$*\" | sed 's/.*-ss \\([0-9]*\\)\\.\\([0-9]*\\).*/\\1\\2/');\n\
                   if [ \"$t\" -le {last_ms} ]; then cat '{png}'; fi ;;\n\
                 *) echo 'unexpected arguments' >&2; exit 1 ;;\n\
                 esac\n",
                log = dir.join("ffmpeg.log").display(),
                png = frame_png.display(),
            ),
        );
        Ffmpeg { ffmpeg, ffprobe }
    }

    #[test]
    fn probes_streams_and_ignores_cover_art() {
        let probe = parse_probe(
            br#"{"streams": [
                {"codec_type": "video", "disposition": {"attached_pic": 1}},
                {"codec_type": "audio"}, {"codec_type": "audio"}, {"codec_type": "data"}],
                "format": {"duration": "12.3456"}}"#,
        )
        .unwrap();
        assert_eq!(
            probe,
            Probe {
                duration_ms: Some(12_346),
                audio: 2,
                video: false
            }
        );
        let video = parse_probe(
            br#"{"streams": [{"codec_type": "video", "disposition": {"attached_pic": 0}}],
                 "format": {"duration": "N/A"}}"#,
        )
        .unwrap();
        assert!(video.video);
        assert_eq!((video.audio, video.duration_ms), (0, None));
        assert!(parse_probe(b"not json").is_err());
    }

    #[test]
    fn sound_and_frame_arguments() {
        let one: Vec<String> = Ffmpeg::sound_args(Path::new("/a b.mp3"), 1)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            one.join(" "),
            "-nostdin -v error -i /a b.mp3 -map 0:a:0 -vn -ac 1 -ar 16000 -f f32le -"
        );
        let two: Vec<String> = Ffmpeg::sound_args(Path::new("x.mkv"), 2)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(two.contains(&"[0:a:0][0:a:1]amix=inputs=2:normalize=0[mix]".to_string()));
        let frame: Vec<String> = Ffmpeg::frame_args(Path::new("v.mp4"), 12_345)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(&frame[3..6], ["-ss", "12.345", "-i"]);
        assert!(frame.iter().any(|a| a.contains("min(1280,iw)")));
        assert_eq!(seconds(5), "0.005");
    }

    #[test]
    fn reads_sound_and_frames_through_the_programs() {
        let dir = temp_dir("ffmpeg");
        let png = dir.join("frame.png");
        crate::image_linux::write_png(&crate::image_linux::tests::two_colors(1920, 1080), &png)
            .unwrap();
        let tools = fake_ffmpeg(
            &dir,
            r#"{"streams": [{"codec_type": "video"}, {"codec_type": "audio"}],
                "format": {"duration": "2.0"}}"#,
            &png,
            1_500,
        );
        let probe = tools.probe(Path::new("clip.mp4")).unwrap();
        assert_eq!(probe.duration_ms, Some(2_000));
        let sound = tools.sound(Path::new("clip.mp4"), &probe).unwrap().unwrap();
        assert_eq!(sound.len(), 32_000);
        assert!(sound.iter().all(|s| *s == 0.25));
        let mut got = Vec::new();
        tools
            .frames(
                Path::new("clip.mp4"),
                &[500, 1_000, 1_900],
                &mut |ms, image| {
                    got.push((ms, image.width(), image.height()));
                    Ok(())
                },
            )
            .unwrap();
        // Past the end gives nothing; frames are shrunk to 1280 on the longer side.
        assert_eq!(got, vec![(500, 1280, 720), (1_000, 1280, 720)]);
        let silent = Probe { audio: 0, ..probe };
        assert_eq!(tools.sound(Path::new("clip.mp4"), &silent).unwrap(), None);
        // A failing ffmpeg is an error with what it said.
        let broken = Ffmpeg {
            ffmpeg: script(&dir, "broken", "echo 'Invalid data found' >&2; exit 1\n"),
            ffprobe: tools.ffprobe.clone(),
        };
        let err = broken.sound(Path::new("x"), &probe).unwrap_err();
        assert!(err.to_string().contains("Invalid data found"), "{err}");
    }
}
