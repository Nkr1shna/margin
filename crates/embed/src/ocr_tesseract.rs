//! OCR off macOS: the `tesseract` 5 CLI (apt `tesseract-ocr tesseract-ocr-eng`,
//! Apache-2.0), run as a subprocess the way `fluidaudiocli` is
//! (`docs/plan-linux-service.md` §2.3): the picture is written to a temporary PNG and
//! read with `tesseract <png> stdout --psm 3 -l eng`.
//!
//! Without `tesseract`, OCR gives no text (images and scanned pages ingest with their
//! pictures only) and the service shows a warning (`find_tesseract` is how it knows).
// On macOS these build (the `linux-readers` feature) for their tests; Linux reads with them.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use image::DynamicImage;

use crate::EmbedError;

/// The OCR program's name.
pub const TESSERACT: &str = "tesseract";
/// Overrides where `tesseract` is looked for (a path to the program).
pub const TESSERACT_ENV: &str = "MARGIN_TESSERACT";

/// `tesseract` at `$MARGIN_TESSERACT`, on PATH or in the usual install folders.
pub fn find_tesseract() -> Option<PathBuf> {
    crate::extract::find_program(TESSERACT, TESSERACT_ENV)
}

/// The arguments `tesseract` is run with for `png`: automatic page segmentation, English.
fn args(png: &Path) -> Vec<std::ffi::OsString> {
    vec![
        png.as_os_str().to_owned(),
        "stdout".into(),
        "--psm".into(),
        "3".into(),
        "-l".into(),
        "eng".into(),
    ]
}

/// Tidies `tesseract`'s output: Unix newlines, no form feed, no trailing spaces, at most
/// one blank line in a row, trimmed.
pub(crate) fn tidy(raw: &str) -> String {
    let text = raw.replace("\r\n", "\n").replace(['\r', '\u{c}'], "\n");
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines().map(str::trim_end) {
        if line.is_empty() && lines.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        lines.push(line);
    }
    lines.join("\n").trim().to_string()
}

/// The text `cli` reads in the image file `png`.
pub(crate) fn recognize_file(cli: &Path, png: &Path) -> Result<String, EmbedError> {
    let out = Command::new(cli)
        .args(args(png))
        // Tesseract's OpenMP threads fight the embedder's; one page at a time is enough.
        .env("OMP_THREAD_LIMIT", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| EmbedError::Extract(format!("couldn't run {}: {e}", cli.display())))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(EmbedError::Extract(format!(
            "tesseract failed: {}",
            err.trim()
        )));
    }
    Ok(tidy(&String::from_utf8_lossy(&out.stdout)))
}

/// The text `cli` reads in `image` (written to a temporary PNG, removed afterwards).
pub(crate) fn recognize_with(cli: &Path, image: &DynamicImage) -> Result<String, EmbedError> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let png = std::env::temp_dir().join(format!(
        "margin-ocr-{}-{}.png",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    crate::image_linux::write_png(image, &png)?;
    let text = recognize_file(cli, &png);
    let _ = std::fs::remove_file(&png);
    text
}

/// The text in `image`, or none when `tesseract` isn't installed.
pub(crate) fn recognize(image: &DynamicImage) -> Result<String, EmbedError> {
    match find_tesseract() {
        Some(cli) => recognize_with(&cli, image),
        None => {
            tracing::warn!("tesseract isn't installed: pictures are read without OCR");
            Ok(String::new())
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::image_linux::tests::temp_dir;

    /// A stand-in `tesseract`: checks its arguments, prints `text` (a scanned page's
    /// worth, with tesseract's form feed), and records the PNG it was given.
    pub(crate) fn fake_tesseract(dir: &Path, text: &str) -> PathBuf {
        let cli = dir.join("tesseract");
        let seen = dir.join("seen");
        std::fs::write(
            &cli,
            format!(
                "#!/bin/sh\n\
                 [ \"$2\" = stdout ] && [ \"$3\" = --psm ] && [ \"$4\" = 3 ] && [ \"$6\" = eng ] || exit 2\n\
                 [ \"$OMP_THREAD_LIMIT\" = 1 ] || exit 3\n\
                 head -c 8 \"$1\" | od -c | head -1 > '{}'\n\
                 printf '%s' \"$1\" > '{}.path'\n\
                 printf '%s\\n\\n\\n\\f' '{}'\n",
                seen.display(),
                seen.display(),
                text.replace('\'', "")
            ),
        )
        .unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        cli
    }

    #[test]
    fn runs_tesseract_on_a_png_and_tidies_its_output() {
        let dir = temp_dir("ocr");
        let cli = fake_tesseract(&dir, "The invoice number is 48213.  ");
        let image = crate::image_linux::tests::two_colors(30, 20);
        let text = recognize_with(&cli, &image).unwrap();
        assert_eq!(text, "The invoice number is 48213.");
        // It was handed a PNG, and the PNG is gone.
        let seen = std::fs::read_to_string(dir.join("seen")).unwrap();
        assert!(seen.contains("P   N   G"), "{seen}");
        // Only this call's PNG: other tests in the process (the real-tesseract one) may
        // have their own temporary PNG in flight at the same moment.
        let given = std::fs::read_to_string(dir.join("seen.path")).unwrap();
        assert!(!Path::new(&given).exists(), "{given} was left behind");

        // A failing tesseract is an error with its message.
        let bad = dir.join("bad");
        std::fs::write(
            &bad,
            "#!/bin/sh\necho 'Error opening data file' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = recognize_with(&bad, &image).unwrap_err();
        assert!(err.to_string().contains("Error opening data file"), "{err}");
        assert!(recognize_file(&dir.join("missing"), &dir.join("x.png")).is_err());
    }

    #[test]
    fn tidy_keeps_paragraphs_only() {
        assert_eq!(tidy("a  \r\nb\n\n\n\nc\n\u{c}"), "a\nb\n\nc");
        assert_eq!(tidy("\n\n \u{c}"), "");
    }

    /// The real `tesseract`, when installed (Linux runners set it up; elsewhere this
    /// skips): the corpus diagram's labels read back.
    #[test]
    fn real_tesseract_reads_text_when_installed() {
        let Some(cli) = find_tesseract() else {
            eprintln!("skipping: tesseract isn't installed");
            return;
        };
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../evals/corpus/images/calvin-cycle.png");
        let image = crate::image_linux::load(&path).unwrap();
        let text = recognize_with(&cli, &image).unwrap().to_lowercase();
        assert!(text.contains("calvin"), "{text}");
    }
}
