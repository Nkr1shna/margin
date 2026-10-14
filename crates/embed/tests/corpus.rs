//! The ingest invariants of `docs/agent-rag-architecture.md` §3.3 that live in this crate,
//! checked on the eval corpus (`evals/corpus`): every sentence lands in exactly one
//! chunk's non-overlap region, chunks keep the hard cap, sections and lines are right,
//! tables and code blocks stay whole, hashes are stable and change only where the text
//! does, and a recording replays the corpus exactly. Fakes and recordings only: the
//! token counter is `FakeEmbedder`'s, and no real model is loaded. The PDF and image
//! tests use PDFKit and Vision on macOS, not the embedding model; their Linux twins
//! (pdfium, tesseract, `image`, ffmpeg) run only with `MARGIN_REAL_TOOLS=1`, and the ONNX
//! media encoders on the corpus only with `MARGIN_REAL_MODEL_TESTS=1` and a model.

use std::path::{Path, PathBuf};

use margin_embed::fake::{FakeEmbedder, RecordedEmbedder, Recorder};
use margin_embed::{
    CHUNK_TOKENS, Chunk, Embedder, Extracted, Purpose, SectionsCfg, SourceKind, TextFormat,
};

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/corpus")
}

/// The prose documents, with the format each must be read as.
const PROSE: [(&str, TextFormat); 8] = [
    ("bayes.md", TextFormat::Markdown),
    ("fourier-series.md", TextFormat::Markdown),
    ("photosynthesis.md", TextFormat::Markdown),
    ("tcp-congestion.md", TextFormat::Markdown),
    ("heat-engines.rst", TextFormat::Rst),
    ("sorting.html", TextFormat::Html),
    ("folder/notes/week1.md", TextFormat::Markdown),
    ("folder/notes/README", TextFormat::Plain),
];

/// A fresh folder per call: tests run in parallel, and a name shared between them (each
/// `read` uses "media") let one test delete the folder while another created it.
fn temp_dir(name: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "margin-embed-corpus-{name}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn read(name: &str) -> Extracted {
    margin_embed::extract(&corpus().join(name), SourceKind::Text, &temp_dir("media")).unwrap()
}

fn count(text: &str) -> usize {
    FakeEmbedder.count_tokens(text)
}

fn chunk(doc: &Extracted, cfg: &SectionsCfg) -> Vec<Chunk> {
    margin_embed::chunk_sections(doc, SourceKind::Text, &count, cfg)
}

/// Small enough that every corpus document is cut into many chunks.
const SMALL: SectionsCfg = SectionsCfg {
    max_tokens: 90,
    target_tokens: 70,
};

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Sentences of at least six words, split independently of the chunker (at ". ", "? ",
/// "! " and line ends), skipping table rows and code.
fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || t.starts_with('|') || line.starts_with(char::is_whitespace) {
            continue;
        }
        let mut rest = t;
        while !rest.is_empty() {
            let cut = ["。", ". ", "? ", "! "]
                .iter()
                .filter_map(|p| rest.find(p).map(|i| i + 1))
                .min()
                .unwrap_or(rest.len());
            let sentence = rest[..cut].trim();
            if sentence.split_whitespace().count() >= 6 {
                out.push(normalize(sentence));
            }
            rest = rest[cut..].trim_start();
        }
    }
    out
}

#[test]
fn every_sentence_is_in_exactly_one_chunks_new_part() {
    for (name, format) in PROSE {
        let doc = read(name);
        assert_eq!(doc.format, format, "{name}");
        let text = &doc.pages[0].text;
        for cfg in [SectionsCfg::default(), SMALL] {
            let chunks = chunk(&doc, &cfg);
            assert!(!chunks.is_empty(), "{name}");
            let texts: Vec<String> = chunks.iter().map(|c| normalize(&c.text)).collect();
            for sentence in sentences(text) {
                let holders: Vec<usize> = (0..texts.len())
                    .filter(|&i| texts[i].contains(&sentence))
                    .collect();
                match holders.as_slice() {
                    [_] => {}
                    // The overlap: the end of one chunk and the start of the next, in one
                    // section.
                    [a, b] => {
                        assert_eq!(*b, a + 1, "{name}: {sentence:?} in chunks {a} and {b}");
                        assert!(texts[*a].ends_with(&sentence), "{name}: {sentence:?}");
                        assert!(texts[*b].starts_with(&sentence), "{name}: {sentence:?}");
                        assert_eq!(chunks[*a].section_path, chunks[*b].section_path);
                    }
                    _ => panic!("{name} ({cfg:?}): {sentence:?} is in chunks {holders:?}"),
                }
            }
            for c in &chunks {
                assert!(
                    count(&c.text) <= cfg.max_tokens,
                    "{name}: {} tokens",
                    count(&c.text)
                );
            }
        }
        // The default settings make few, full chunks: no runts in a long section.
        let chunks = chunk(&doc, &SectionsCfg::default());
        assert!(chunks.iter().all(|c| count(&c.text) <= CHUNK_TOKENS));
    }
}

#[test]
fn sections_and_lines_point_at_the_source() {
    for (name, format) in PROSE {
        let doc = read(name);
        let text = &doc.pages[0].text;
        let lines: Vec<&str> = text.lines().collect();
        // Only text before the first heading (an HTML page's <title>) has no section.
        let mut seen_heading = false;
        for c in chunk(&doc, &SMALL) {
            seen_heading |= !c.section_path.is_empty();
            let (a, b) = (c.start_line.unwrap() as usize, c.end_line.unwrap() as usize);
            assert!(1 <= a && a <= b && b <= lines.len(), "{name}: {a}..{b}");
            let span = lines[a - 1..b].join("\n");
            for line in c.text.lines().map(str::trim).filter(|l| !l.is_empty()) {
                assert!(
                    span.contains(line),
                    "{name}: {line:?} not in lines {a}..{b}"
                );
            }
            if format != TextFormat::Plain && seen_heading {
                assert!(!c.section_path.is_empty(), "{name}: {:?}", c.text);
            }
            assert_eq!(c.hash, margin_embed::chunk_hash(&c.section_path, &c.text));
        }
    }
    let paths = |name: &str| -> Vec<String> {
        chunk(&read(name), &SectionsCfg::default())
            .into_iter()
            .map(|c| c.section_path)
            .collect()
    };
    let photosynthesis = paths("photosynthesis.md");
    assert!(photosynthesis.contains(
        &"Photosynthesis > The light-dependent reactions > Photosystem II and the splitting of \
          water"
            .to_string()
    ));
    assert!(photosynthesis.contains(&"Photosynthesis > C4 and CAM plants".to_string()));
    let heat = paths("heat-engines.rst");
    assert!(
        heat.contains(&"Heat engines > The second law".to_string()),
        "{heat:?}"
    );
    assert!(
        heat.iter().all(|p| p.starts_with("Heat engines")),
        "{heat:?}"
    );
    let sorting = paths("sorting.html");
    assert!(
        sorting.contains(&"Comparison sorting: algorithms and limits > Quicksort".to_string()),
        "{sorting:?}"
    );
    assert!(paths("folder/notes/README").iter().all(String::is_empty));
}

#[test]
fn tables_and_code_blocks_stay_whole() {
    let block = |name: &str, first: &str, last: &str| {
        let doc = read(name);
        let text = &doc.pages[0].text;
        let start = text
            .find(first)
            .unwrap_or_else(|| panic!("{name}: {first:?}"));
        let end = text[start..].find(last).unwrap() + start + last.len();
        let block = &text[start..end];
        for cfg in [SectionsCfg::default(), SMALL] {
            let chunks = chunk(&doc, &cfg);
            assert!(
                chunks.iter().any(|c| c.text.contains(block)),
                "{name} ({cfg:?}): the block isn't whole in any chunk"
            );
        }
    };
    block(
        "fourier-series.md",
        "| Signal (period 2π)",
        "cos(2nt)/(4n² − 1) |",
    );
    block("fourier-series.md", "```python", "/ n\n```");
    block("photosynthesis.md", "| Pathway |", "pineapple, cacti |");
    block("sorting.html", "| Algorithm | Best |", "| Counting sort");
    block("heat-engines.rst", ".. math::", "1 - Q_c / Q_h");
}

#[test]
fn hashes_are_stable_and_change_only_with_their_section() {
    let path = corpus().join("photosynthesis.md");
    let original = std::fs::read_to_string(&path).unwrap();
    let dir = temp_dir("edit");
    let write = |text: &str| {
        let file = dir.join("photosynthesis.md");
        std::fs::write(&file, text).unwrap();
        margin_embed::extract(&file, SourceKind::Text, &dir).unwrap()
    };
    let before = chunk(&write(&original), &SMALL);
    // Re-reading an unchanged file gives the same chunks: nothing to embed again.
    assert_eq!(before, chunk(&write(&original), &SMALL));
    assert_eq!(before, chunk(&read("photosynthesis.md"), &SMALL));

    // One sentence of one section changes.
    let edited_text = original.replacen("Photorespiration", "Photo-respiration", 1);
    assert_ne!(edited_text, original);
    let after = chunk(&write(&edited_text), &SMALL);
    let old: std::collections::HashSet<&str> = before.iter().map(|c| c.hash.as_str()).collect();
    let changed: Vec<&Chunk> = after
        .iter()
        .filter(|c| !old.contains(c.hash.as_str()))
        .collect();
    assert!(!changed.is_empty());
    assert!(
        changed
            .iter()
            .all(|c| c.section_path == "Photosynthesis > Photorespiration"
                || c.section_path == "Photosynthesis > Photo-respiration"),
        "{:?}",
        changed.iter().map(|c| &c.section_path).collect::<Vec<_>>()
    );
    let kept = after.len() - changed.len();
    assert!(
        kept >= before.len() - 4,
        "only {kept} of {} kept",
        before.len()
    );
}

#[test]
fn a_recording_of_the_corpus_replays_it_exactly() {
    let docs: Vec<(&str, Extracted)> = PROSE.iter().map(|(n, _)| (*n, read(n))).collect();
    let recorder = Recorder::new(FakeEmbedder);
    let chunk_with = |embedder: &dyn Embedder, doc: &Extracted| {
        margin_embed::chunk_sections(doc, SourceKind::Text, &|t| embedder.count_tokens(t), &SMALL)
    };
    let inputs = |name: &str, chunks: &[Chunk]| -> Vec<(String, Purpose)> {
        chunks
            .iter()
            .map(|c| {
                let title = margin_embed::chunk_title(name, c);
                (c.text.clone(), Purpose::document(Some(&title)))
            })
            .collect()
    };
    let embed_all = |embedder: &dyn Embedder| -> Vec<(Vec<Chunk>, Vec<Vec<f32>>)> {
        docs.iter()
            .map(|(name, doc)| {
                let chunks = chunk_with(embedder, doc);
                let mut vectors = Vec::new();
                embedder
                    .embed_pipelined(&inputs(name, &chunks), 8, &mut |first, v| {
                        assert_eq!(first, vectors.len());
                        vectors.extend(v);
                        Ok(())
                    })
                    .unwrap();
                (chunks, vectors)
            })
            .collect()
    };
    let recorded = embed_all(&recorder);
    let queries = vec!["Why is the Carnot efficiency an upper bound?".to_string()];
    let query = recorder.embed(&queries, Purpose::Query).unwrap();

    let file = temp_dir("recording").join("embeddings.bin");
    recorder.recording().save(&file).unwrap();
    let replay = RecordedEmbedder::open(&file).unwrap();
    let replayed = embed_all(&replay);
    for ((chunks_a, vectors_a), (chunks_b, vectors_b)) in recorded.iter().zip(&replayed) {
        assert_eq!(chunks_a, chunks_b);
        assert_eq!(vectors_a.len(), vectors_b.len());
        for (a, b) in vectors_a.iter().zip(vectors_b) {
            assert!(margin_embed::cosine(a, b) > 0.9999);
        }
    }
    let again = replay.embed(&queries, Purpose::Query).unwrap();
    assert!(margin_embed::cosine(&query[0], &again[0]) > 0.9999);

    // A text the recording never saw fails loudly.
    let err = replay
        .embed(&["A sentence nobody recorded.".to_string()], Purpose::Query)
        .unwrap_err();
    assert!(err.to_string().contains("record-embeddings"), "{err}");
}

#[test]
fn the_corpus_folder_tree_is_what_the_chunker_expects() {
    // Files on the allow-list that are text read as text; the extension-less README is
    // plain prose, `week1.md` Markdown.
    assert_eq!(read("folder/notes/README").format, TextFormat::Plain);
    assert_eq!(read("folder/notes/week1.md").format, TextFormat::Markdown);
    let chunks = chunk(&read("folder/notes/week1.md"), &SectionsCfg::default());
    let paths: Vec<&str> = chunks.iter().map(|c| c.section_path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "Week 1 notes",
            "Week 1 notes > Circuits",
            "Week 1 notes > Signals",
            "Week 1 notes > Questions for the tutorial"
        ]
    );
}

#[cfg(target_os = "macos")]
mod mac {
    //! PDFKit and Vision on the corpus PDFs and images (no embedding model).

    use super::*;

    /// Vision runs very slowly when several tests call it at once; one at a time.
    static VISION: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
        VISION.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The pages of a PDF's source text (pages separated by a `\f` line).
    fn source_pages(name: &str) -> Vec<String> {
        std::fs::read_to_string(corpus().join("sources").join(name))
            .unwrap()
            .split("\n\\f\n")
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn pdfs_are_read_lazily_and_pages_rendered_on_demand() {
        let _vision = one_at_a_time();
        let pdf = corpus().join("pdf/rc-circuits.pdf");
        let media = temp_dir("pdf-media");
        let doc = margin_embed::extract_lazy(&pdf, SourceKind::Pdf, &media).unwrap();
        let sources = source_pages("rc-circuits.txt");
        assert_eq!(doc.pages.len(), sources.len());
        assert!(doc.pictures.is_empty());
        assert_eq!(std::fs::read_dir(&media).unwrap().count(), 0);
        assert_eq!(doc.format, TextFormat::Plain);
        for (page, source) in doc.pages.iter().zip(&sources) {
            let first = source.lines().next().unwrap();
            assert!(
                normalize(&page.text).contains(&normalize(first)),
                "{first:?}"
            );
        }
        // The eager path still renders every page.
        let eager = margin_embed::extract(&pdf, SourceKind::Pdf, &media).unwrap();
        assert_eq!(eager.pictures.len(), eager.pages.len());
        assert_eq!(eager.pages, doc.pages);

        let out = temp_dir("pdf-page").join("page-2.png");
        margin_embed::pdf_page_picture(&pdf, 2, &out).unwrap();
        assert_eq!(
            std::fs::read(&out).unwrap(),
            std::fs::read(&eager.pictures[1].path).unwrap()
        );
        assert!(margin_embed::pdf_page_picture(&pdf, 0, &out).is_err());
        let past = margin_embed::pdf_page_picture(&pdf, 99, &out).unwrap_err();
        assert!(past.to_string().contains("no page 99"), "{past}");

        // Page chunks: page numbers, no lines, no headings.
        let chunks = margin_embed::chunk_sections(&doc, SourceKind::Pdf, &count, &SMALL);
        assert!(
            chunks
                .iter()
                .all(|c| c.page.is_some() && c.start_line.is_none())
        );
        assert!(
            chunks
                .iter()
                .all(|c| c.section_path.is_empty() && !c.hash.is_empty())
        );
    }

    #[test]
    fn a_scanned_pdf_is_read_with_ocr() {
        let _vision = one_at_a_time();
        let doc = margin_embed::extract_lazy(
            &corpus().join("pdf/glacier-scan.pdf"),
            SourceKind::Pdf,
            &temp_dir("scan"),
        )
        .unwrap();
        let text = normalize(
            &doc.pages
                .iter()
                .map(|p| p.text.as_str())
                .collect::<String>(),
        )
        .to_lowercase();
        assert!(text.contains("accumulation zone"), "{text}");
        assert!(text.contains("equilibrium line"), "{text}");
    }

    #[test]
    fn diagram_labels_are_the_images_text_and_photos_have_none() {
        let _vision = one_at_a_time();
        let read = |name: &str| {
            margin_embed::extract(
                &corpus().join("images").join(name),
                SourceKind::Image,
                &temp_dir("img"),
            )
            .unwrap()
        };
        let calvin = read("calvin-cycle.png");
        assert_eq!(calvin.pictures.len(), 1);
        assert_eq!(calvin.pages.len(), 1);
        let text = calvin.pages[0].text.to_lowercase();
        assert!(text.contains("calvin cycle"), "{text}");
        assert!(text.contains("rubisco"), "{text}");
        let carnot = read("carnot-pv-diagram.png");
        assert!(
            carnot.pages[0].text.to_lowercase().contains("adiabatic"),
            "{carnot:?}"
        );
        // The image's text chunks like any OCR text: one unit, no lines.
        let chunks = margin_embed::chunk_sections(
            &calvin,
            SourceKind::Image,
            &count,
            &SectionsCfg::default(),
        );
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].start_line.is_none());
        for scene in ["scene-red-bicycle.png", "scene-sailboat-sunset.png"] {
            let doc = read(scene);
            assert_eq!(doc.pictures.len(), 1);
            assert!(doc.pages.is_empty(), "{scene}: {:?}", doc.pages);
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    //! The Linux readers (pdfium, tesseract, `image`, ffmpeg) on the corpus, through the
    //! public API, with the real programs: only with `MARGIN_REAL_TOOLS=1`, which the
    //! Linux runner sets because it has the apt packages and the pdfium library.

    use super::*;

    fn real_tools() -> bool {
        let on = std::env::var("MARGIN_REAL_TOOLS").is_ok_and(|v| v == "1");
        if !on {
            eprintln!("skipping: the real-tool reader tests need MARGIN_REAL_TOOLS=1");
        }
        on
    }

    fn source_pages(name: &str) -> Vec<String> {
        std::fs::read_to_string(corpus().join("sources").join(name))
            .unwrap()
            .split("\n\\f\n")
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn pdfs_are_read_with_pdfium_and_pages_drawn_on_demand() {
        if !real_tools() {
            return;
        }
        assert!(
            margin_embed::find_pdfium().is_some(),
            "pdfium isn't installed"
        );
        let pdf = corpus().join("pdf/rc-circuits.pdf");
        let media = temp_dir("pdfium-media");
        let doc = margin_embed::extract_lazy(&pdf, SourceKind::Pdf, &media).unwrap();
        let sources = source_pages("rc-circuits.txt");
        assert_eq!(doc.pages.len(), sources.len());
        assert!(doc.pictures.is_empty());
        for (page, source) in doc.pages.iter().zip(&sources) {
            let first = source.lines().next().unwrap();
            assert!(
                normalize(&page.text).contains(&normalize(first)),
                "{first:?}"
            );
        }
        let eager = margin_embed::extract(&pdf, SourceKind::Pdf, &media).unwrap();
        assert_eq!(eager.pictures.len(), eager.pages.len());
        assert_eq!(eager.pages, doc.pages);
        let out = temp_dir("pdfium-page").join("page-2.png");
        margin_embed::pdf_page_picture(&pdf, 2, &out).unwrap();
        assert_eq!(
            std::fs::read(&out).unwrap(),
            std::fs::read(&eager.pictures[1].path).unwrap()
        );
        assert!(margin_embed::pdf_page_picture(&pdf, 99, &out).is_err());
    }

    #[test]
    fn a_scanned_pdf_is_read_with_tesseract() {
        if !real_tools() {
            return;
        }
        assert!(
            margin_embed::find_tesseract().is_some(),
            "tesseract isn't installed"
        );
        let doc = margin_embed::extract_lazy(
            &corpus().join("pdf/glacier-scan.pdf"),
            SourceKind::Pdf,
            &temp_dir("scan"),
        )
        .unwrap();
        let text = normalize(
            &doc.pages
                .iter()
                .map(|p| p.text.as_str())
                .collect::<String>(),
        )
        .to_lowercase();
        assert!(text.contains("accumulation zone"), "{text}");
        assert!(text.contains("equilibrium line"), "{text}");
    }

    #[test]
    fn diagram_labels_are_read_and_photos_have_none() {
        if !real_tools() {
            return;
        }
        let read = |name: &str| {
            margin_embed::extract(
                &corpus().join("images").join(name),
                SourceKind::Image,
                &temp_dir("img"),
            )
            .unwrap()
        };
        let calvin = read("calvin-cycle.png");
        assert_eq!(calvin.pictures.len(), 1);
        let text = calvin.pages[0].text.to_lowercase();
        assert!(text.contains("calvin cycle"), "{text}");
        assert!(
            read("carnot-pv-diagram.png").pages[0]
                .text
                .to_lowercase()
                .contains("adiabatic")
        );
        for scene in ["scene-red-bicycle.png", "scene-sailboat-sunset.png"] {
            let doc = read(scene);
            assert_eq!(doc.pictures.len(), 1);
            assert!(doc.pages.is_empty(), "{scene}: {:?}", doc.pages);
        }
        let jpeg =
            margin_embed::jpeg_for_model(&corpus().join("images/calvin-cycle.png"), 256).unwrap();
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
    }

    #[test]
    fn ffmpeg_reads_the_speech_fixture() {
        if !real_tools() {
            return;
        }
        assert!(
            margin_embed::find_ffmpeg().is_some(),
            "ffmpeg isn't installed"
        );
        // The mel fixture's samples, written as a 16-bit WAV and read back by ffmpeg.
        let samples = speech_samples();
        let wav = temp_dir("wav").join("speech.wav");
        let data = samples.len() as u32 * 2;
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&(36 + data).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        // Chunk size, PCM + mono, rate, bytes per second, block align + bits.
        for v in [16u32, 1 | (1 << 16), 16_000, 32_000, 2 | (16 << 16)] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data.to_le_bytes());
        for s in &samples {
            bytes.extend_from_slice(&((s * 32768.0).round() as i16).to_le_bytes());
        }
        std::fs::write(&wav, bytes).unwrap();
        let doc = margin_embed::extract(&wav, SourceKind::Audio, &temp_dir("audio")).unwrap();
        let sound = doc.sound.unwrap();
        assert_eq!(sound.len(), samples.len());
        let worst = sound
            .iter()
            .zip(&samples)
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(worst < 1e-3, "{worst}");
        assert!(margin_embed::extract(&wav, SourceKind::Video, &temp_dir("v")).is_err());
    }
}

/// The samples of the mel fixture (`tests/fixtures/mel/speech.mel.bin`): 2.5 s of
/// synthesized speech, 16 kHz mono ("The Carnot cycle has two isothermal and two
/// adiabatic steps.").
#[cfg(any(target_os = "linux", feature = "onnx"))]
fn speech_samples() -> Vec<f32> {
    let bytes = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mel/speech.mel.bin"),
    )
    .unwrap();
    assert_eq!(&bytes[..4], b"MELF");
    let n = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    bytes[20..20 + 2 * n]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
        .collect()
}

#[cfg(any(target_os = "linux", feature = "onnx"))]
mod onnx_media {
    //! The ONNX image and audio encoders on the corpus pictures and the speech fixture,
    //! with the real model: only with `MARGIN_REAL_MODEL_TESTS=1` and a downloaded ONNX
    //! q8 model in `MARGIN_EMBED_ONNX_DIR` (as `tests/onnx_text.rs`).

    use super::*;
    use margin_embed::{
        Backend, ImageTokens, ModelFiles, OnnxDtype, OnnxEmbedder, OnnxOptions, cosine,
    };

    fn files() -> Option<ModelFiles> {
        if std::env::var("MARGIN_REAL_MODEL_TESTS").is_ok_and(|v| v == "1")
            && let Some(dir) = std::env::var_os("MARGIN_EMBED_ONNX_DIR")
        {
            let files = ModelFiles::with_backend(PathBuf::from(dir), Backend::Onnx(OnnxDtype::Q8));
            if files.is_complete() {
                return Some(files);
            }
        }
        eprintln!(
            "skipping: needs MARGIN_REAL_MODEL_TESTS=1 and a downloaded model in \
             MARGIN_EMBED_ONNX_DIR"
        );
        None
    }

    #[test]
    fn real_model_embeds_corpus_pictures_and_speech() {
        let Some(files) = files() else {
            return;
        };
        // The encoders' graphs, for the record.
        for graph in ["vision_encoder", "audio_encoder"] {
            let path = files.graph_path(graph).unwrap();
            eprintln!(
                "{graph}: {:?}",
                margin_embed::onnx::describe_graph(&path).unwrap()
            );
        }
        let model = OnnxEmbedder::load(&files, &OnnxOptions::default()).unwrap();
        let images: Vec<PathBuf> = [
            "scene-red-bicycle.png",
            "scene-sailboat-sunset.png",
            "calvin-cycle.png",
        ]
        .iter()
        .map(|n| corpus().join("images").join(n))
        .collect();
        let pictures = model.embed_images(&images, ImageTokens::High).unwrap();
        let frames = model
            .embed_images(&images[..1], ImageTokens::Medium)
            .unwrap();
        let queries = model
            .embed(
                &[
                    "a red bicycle".to_string(),
                    "a sailboat at sunset".to_string(),
                    "the Calvin cycle in photosynthesis".to_string(),
                    "the Carnot cycle's isothermal and adiabatic steps".to_string(),
                ],
                Purpose::Query,
            )
            .unwrap();
        for v in pictures.iter().chain(&frames) {
            assert_eq!(v.len(), margin_embed::DIM);
            assert!((v.iter().map(|x| x * x).sum::<f32>().sqrt() - 1.0).abs() < 1e-3);
        }
        // Each picture query finds its own picture first.
        for (q, query) in queries[..3].iter().enumerate() {
            let scores: Vec<f32> = pictures.iter().map(|p| cosine(query, p)).collect();
            let best = (0..3)
                .max_by(|a, b| scores[*a].total_cmp(&scores[*b]))
                .unwrap();
            assert_eq!(best, q, "query {q}: {scores:?}");
        }
        // Fewer tokens for the same picture stays close to it.
        assert!(cosine(&frames[0], &pictures[0]) > 0.8);
        // The spoken sentence is nearer its own topic than another.
        let speech = model.embed_audio(&[speech_samples()]).unwrap();
        let carnot = cosine(&speech[0], &queries[3]);
        let bicycle = cosine(&speech[0], &queries[0]);
        assert!(carnot > bicycle, "{carnot} vs {bicycle}");
    }
}
