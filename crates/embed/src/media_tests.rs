//! Reading real images, audio and video with the macOS frameworks. Test files are made on
//! the spot with Core Graphics, `sips`, `say` and `afconvert`; the video tests use
//! `lecture.mp4` / `silent.mov` from the media folder (`make_video.sh` there) and are
//! skipped without them. Transcription runs only when `fluidaudiocli` is installed.

use std::path::{Path, PathBuf};
use std::process::Command;

use objc2_core_foundation::{CFDictionary, CFNumber, CFString, CFType, CFURL};
use objc2_image_io::{CGImageDestination, kCGImagePropertyOrientation};

use crate::{ImageTokens, SourceKind, extract};

fn temp_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("margin-embed-media-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(cmd: &mut Command) {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Red on the left, blue on the right, as the pixel at the top-left and top-right.
fn corners(path: &Path) -> ([u8; 3], [u8; 3], (usize, usize)) {
    let image = crate::image::load(path, None).unwrap();
    let size = crate::image::size(&image);
    let pixels = crate::image::rgba(&image, 8, 8).unwrap();
    let px = |i: usize| [pixels[i], pixels[i + 1], pixels[i + 2]];
    (px(4), px(7 * 4), size)
}

fn is_red(p: [u8; 3]) -> bool {
    p[0] > 180 && p[2] < 80
}

fn is_blue(p: [u8; 3]) -> bool {
    p[2] > 180 && p[0] < 80
}

#[test]
fn reads_every_image_format_upright() {
    let dir = temp_dir("images");
    let png = dir.join("two.png");
    crate::image::write(
        &crate::image::tests::two_colors(96, 48),
        &png,
        crate::image::Format::Png,
    )
    .unwrap();
    for (format, ext) in [
        ("jpeg", "jpg"),
        ("heic", "heic"),
        ("gif", "gif"),
        ("tiff", "tiff"),
        ("bmp", "bmp"),
    ] {
        let out = dir.join(format!("two.{ext}"));
        run(Command::new("/usr/bin/sips")
            .args(["-s", "format", format])
            .arg(&png)
            .arg("--out")
            .arg(&out));
        let (left, right, size) = corners(&out);
        assert_eq!(size, (96, 48), "{ext}");
        assert!(is_red(left) && is_blue(right), "{ext}: {left:?} {right:?}");
        let doc = extract(&out, SourceKind::Image, &dir).unwrap();
        assert_eq!(doc.pictures.len(), 1);
        assert_eq!(doc.pictures[0].path, out);
        assert_eq!(doc.pictures[0].tokens, ImageTokens::High);
        assert!(doc.pages.is_empty() && doc.sound.is_none());
    }

    // WebP: ImageIO reads it but can't write it, so the media folder has one (Pillow).
    if let Some(webp) = media("diagram.webp") {
        let doc = extract(&webp, SourceKind::Image, &dir).unwrap();
        assert_eq!(doc.pictures.len(), 1);
        let image = crate::image::load(&webp, None).unwrap();
        assert_eq!(crate::image::size(&image), (900, 900));
    }

    // A JPEG stored sideways with EXIF orientation 6 ("rotate 90° clockwise to view"):
    // read upright, it is 48 wide and 96 tall with red on top.
    let sideways = dir.join("sideways.jpg");
    let url = CFURL::from_file_path(&sideways).unwrap();
    let uti = CFString::from_static_str("public.jpeg");
    let destination = unsafe { CGImageDestination::with_url(&url, &uti, 1, None) }.unwrap();
    let six = CFNumber::new_i32(6);
    let six: &CFType = six.as_ref();
    let properties = CFDictionary::from_slices(&[unsafe { kCGImagePropertyOrientation }], &[six]);
    unsafe {
        destination.add_image(
            &crate::image::tests::two_colors(96, 48),
            Some(properties.as_opaque()),
        );
        assert!(destination.finalize());
    }
    let image = crate::image::load(&sideways, None).unwrap();
    assert_eq!(crate::image::size(&image), (48, 96));
    let pixels = crate::image::rgba(&image, 8, 8).unwrap();
    let top = [pixels[16], pixels[17], pixels[18]];
    let bottom = [
        pixels[7 * 32 + 16],
        pixels[7 * 32 + 17],
        pixels[7 * 32 + 18],
    ];
    assert!(is_red(top) && is_blue(bottom), "{top:?} {bottom:?}");

    let broken = dir.join("broken.png");
    std::fs::write(&broken, "not an image").unwrap();
    assert!(extract(&broken, SourceKind::Image, &dir).is_err());
}

#[test]
fn decodes_audio_formats_to_16k_mono() {
    let dir = temp_dir("audio");
    let aiff = dir.join("speech.aiff");
    run(Command::new("/usr/bin/say")
        .arg("-o")
        .arg(&aiff)
        .arg("Sound travels as a pressure wave through the air."));
    let reference = crate::av::sound(&aiff).unwrap().unwrap();
    let seconds = reference.len() as f64 / 16_000.0;
    assert!((1.5..10.0).contains(&seconds), "{seconds} s");
    for (format, data, ext) in [
        ("WAVE", "LEI16@44100", "wav"),
        ("m4af", "aac", "m4a"),
        ("caff", "LEF32@22050", "caf"),
        ("flac", "flac", "flac"),
    ] {
        let out = dir.join(format!("speech.{ext}"));
        run(Command::new("/usr/bin/afconvert")
            .args(["-f", format, "-d", data, "-c", "2"])
            .arg(&aiff)
            .arg(&out));
        let doc = extract(&out, SourceKind::Audio, &dir).unwrap();
        let sound = doc.sound.unwrap();
        let other = sound.len() as f64 / 16_000.0;
        // AAC adds priming and padding; everything else is sample-exact up to rounding.
        assert!(
            (other - seconds).abs() < 0.1,
            "{ext}: {other} s vs {seconds} s"
        );
        assert!(sound.iter().any(|s| s.abs() > 0.05), "{ext} is silent");
        assert!(sound.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
        assert_eq!(doc.duration_ms, Some(crate::sample_ms(sound.len())));
        assert!(doc.pictures.is_empty() && doc.pages.is_empty());
    }
    let broken = dir.join("broken.mp3");
    std::fs::write(&broken, "not audio").unwrap();
    assert!(extract(&broken, SourceKind::Audio, &dir).is_err());
}

fn media(name: &str) -> Option<PathBuf> {
    let dir = match std::env::var_os("MARGIN_EMBED_MEDIA_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => {
            PathBuf::from(std::env::var_os("HOME")?).join("Library/Caches/margin-embed-test/media")
        }
    };
    let path = dir.join(name);
    if path.exists() {
        Some(path)
    } else {
        eprintln!("skipping: no {} (run make_video.sh)", path.display());
        None
    }
}

#[test]
fn takes_frames_and_the_soundtrack_from_videos() {
    let Some(lecture) = media("lecture.mp4") else {
        return;
    };
    let dir = temp_dir("video");
    let doc = extract(&lecture, SourceKind::Video, &dir).unwrap();
    // 12 s, give or take the AAC track's edges.
    let duration = doc.duration_ms.unwrap();
    assert!((11_800..=12_200).contains(&duration), "{duration}");
    let times: Vec<u64> = doc.pictures.iter().map(|p| p.at_ms.unwrap()).collect();
    assert_eq!(times, crate::frame_times(duration).1);
    assert_eq!(&times[..2], &[2_500, 7_500]);
    for p in &doc.pictures {
        assert_eq!(p.path, dir.join(crate::frame_file(p.at_ms.unwrap())));
        assert_eq!(p.tokens, ImageTokens::Medium);
        let image = crate::image::load(&p.path, None).unwrap();
        assert_eq!(crate::image::size(&image), (640, 360));
    }
    // The first frame is the white diagram, the last the darker coast photo.
    let brightness = |path: &Path| {
        let image = crate::image::load(path, None).unwrap();
        let pixels = crate::image::rgba(&image, 16, 9).unwrap();
        pixels.iter().map(|&b| b as u64).sum::<u64>() / pixels.len() as u64
    };
    assert!(brightness(&doc.pictures[0].path) > brightness(&doc.pictures[2].path) + 40);
    let sound = doc.sound.unwrap();
    assert!(
        (sound.len() as i64 - 12 * 16_000).abs() < 16_000,
        "{}",
        sound.len()
    );
    // The speech is at the start; the end is silence.
    assert!(sound[..5 * 16_000].iter().any(|s| s.abs() > 0.05));
    assert!(sound[8 * 16_000..].iter().all(|s| s.abs() < 0.01));

    // A video without sound.
    let Some(silent) = media("silent.mov") else {
        return;
    };
    let doc = extract(&silent, SourceKind::Video, &dir).unwrap();
    assert_eq!(doc.pictures.len(), 3);
    assert_eq!(doc.sound, None);

    // An audio file isn't a video.
    let Some(speech) = media("speech.wav") else {
        return;
    };
    assert!(extract(&speech, SourceKind::Video, &dir).is_err());
}

#[test]
fn renders_pdf_pages_as_pictures() {
    let dir = temp_dir("pdf");
    let txt = dir.join("text.txt");
    std::fs::write(
        &txt,
        format!("First page.\n{}Second page.\n", "\n".repeat(80)),
    )
    .unwrap();
    let out = Command::new("/usr/sbin/cupsfilter")
        .arg(&txt)
        .output()
        .unwrap();
    assert!(out.status.success());
    let pdf = dir.join("doc.pdf");
    std::fs::write(&pdf, out.stdout).unwrap();
    let media = dir.join("media");
    let doc = extract(&pdf, SourceKind::Pdf, &media).unwrap();
    assert!(doc.pages.len() >= 2);
    assert_eq!(doc.pictures.len(), doc.pages.len());
    for (i, p) in doc.pictures.iter().enumerate() {
        let n = i as u32 + 1;
        assert_eq!(p.page, Some(n));
        assert_eq!(p.path, media.join(crate::page_file(n)));
        let image = crate::image::load(&p.path, None).unwrap();
        let (w, h) = crate::image::size(&image);
        // US Letter at 144 dpi.
        assert_eq!((w, h), (1224, 1584));
    }
    assert_eq!(&std::fs::read(&doc.pictures[0].path).unwrap()[1..4], b"PNG");
}

#[test]
fn transcribes_speech_with_word_times() {
    if !crate::real_model_tests() {
        return;
    }
    let Some(cli) = crate::find_fluidaudiocli() else {
        eprintln!("skipping: fluidaudiocli isn't installed");
        return;
    };
    let dir = temp_dir("transcribe");
    let aiff = dir.join("speech.aiff");
    run(Command::new("/usr/bin/say").arg("-o").arg(&aiff).arg(
        "Photosynthesis turns light into chemical energy. The Calvin cycle fixes carbon dioxide.",
    ));
    let samples = crate::av::sound(&aiff).unwrap().unwrap();
    let transcript = crate::transcribe(&cli, &samples, &dir).unwrap();
    let text = transcript
        .words
        .iter()
        .map(|w| w.text.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    eprintln!("{text}");
    assert!(text.contains("photosynthesis"), "{text}");
    assert!(text.contains("calvin cycle"), "{text}");
    assert!(
        transcript
            .words
            .windows(2)
            .all(|w| w[0].start_ms <= w[1].start_ms)
    );
    let end = transcript.words.last().unwrap().end_ms;
    assert!(end <= crate::sample_ms(samples.len()) + 500, "{end}");
    // The temporary files are gone.
    assert!(!dir.join("transcribe.wav").exists());
    assert!(!dir.join("transcribe.json").exists());
    let chunks = transcript.chunks(&|s| s.split_whitespace().count(), 480);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].start_ms, Some(transcript.words[0].start_ms));

    // Silence transcribes to nothing rather than failing.
    let silence = crate::transcribe(&cli, &vec![0.0; 32_000], &dir).unwrap();
    assert!(silence.words.len() <= 1, "{silence:?}");
}
