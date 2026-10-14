//! The fixture corpus (`evals/corpus`) as documents to index: which files are documents
//! (the folder tree through the same allow-list devices apply), what was read from
//! each, and a hash of the whole corpus that keys eval runs.
//!
//! What a PDF or an image *reads as* comes from PDFKit and Vision on the owner's Mac,
//! which neither CI nor another macOS version reproduces byte for byte, so
//! `margin-eval record-embeddings` records the extraction next to the vectors
//! (`evals/recorded/extracted.json`) and CI replays it. Text documents are read from
//! the file itself.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use margin_embed::{Extracted, ImageTokens, Page, Picture, SourceKind};
use margin_shared::device::{folder_file_type, is_skipped_name};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The corpus folder holding the text the PDFs are drawn from: not documents.
const SOURCES_DIR: &str = "sources";
/// The attached-folder tree.
pub const FOLDER_DIR: &str = "folder";

/// One document of the corpus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusDoc {
    /// `/`-separated path inside `evals/corpus`; also its document id in the eval index.
    pub path: String,
    pub kind: SourceKind,
    /// Under `folder/`: searched by folder-scoped queries as well.
    pub in_folder: bool,
}

impl CorpusDoc {
    /// The file name, the embedding title of a chunk without a section path.
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

/// The documents of `corpus`, in path order: every file Margin reads outside
/// `sources/`, and inside `folder/` only what the folder allow-list lets through
/// (`margin_shared::device::folder_file_type`, hidden and secret-looking names skipped).
pub fn documents(corpus: &Path) -> anyhow::Result<Vec<CorpusDoc>> {
    let mut docs = Vec::new();
    for rel in files(corpus)? {
        let parts: Vec<&str> = rel.split('/').collect();
        if parts[0] == SOURCES_DIR {
            continue;
        }
        let in_folder = parts[0] == FOLDER_DIR;
        let kind = if in_folder {
            if parts[1..].iter().any(|p| is_skipped_name(p)) {
                continue;
            }
            match folder_file_type(parts[parts.len() - 1]) {
                Some(t) => t.source_kind(),
                None => continue,
            }
        } else {
            match SourceKind::for_name(&rel) {
                Some(k) => k,
                None => continue,
            }
        };
        docs.push(CorpusDoc {
            path: rel,
            kind,
            in_folder,
        });
    }
    Ok(docs)
}

/// Every file under `dir`, as sorted `/`-separated relative paths.
fn files(dir: &Path) -> anyhow::Result<Vec<String>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> anyhow::Result<()> {
        for entry in
            std::fs::read_dir(dir).with_context(|| format!("couldn't list {}", dir.display()))?
        {
            let path = entry?.path();
            if path.is_dir() {
                walk(root, &path, out)?;
            } else {
                let rel = path.strip_prefix(root)?;
                let parts: Vec<String> = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect();
                out.push(parts.join("/"));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    out.sort();
    Ok(out)
}

/// sha256 over every corpus file's path and bytes: two runs over different corpora
/// never compare as like for like.
pub fn corpus_hash(corpus: &Path) -> anyhow::Result<String> {
    let mut h = Sha256::new();
    for rel in files(corpus)? {
        let bytes = std::fs::read(corpus.join(&rel))?;
        h.update(rel.as_bytes());
        h.update([0]);
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
    }
    Ok(hex::encode(h.finalize()))
}

/// The token count every eval chunker run uses, in recording and in replay: about four
/// characters a token, never fewer tokens than words. Not the Gemma tokenizer (its
/// files are tens of MB, more than the corpus); what matters is that the recording and
/// CI cut the same chunks, and a chunk this counts under `CHUNK_TOKENS` stays well
/// under the model's 512.
pub fn approx_tokens(text: &str) -> usize {
    text.chars()
        .count()
        .div_ceil(4)
        .max(text.split_whitespace().count())
}

/// What was read from a PDF or an image, as recorded.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExtractedDoc {
    /// A PDF's pages (text layer, or OCR for a page without one), an image's OCR text.
    pub pages: Vec<PageText>,
    /// Its pictures: the image itself (`page: None`) or each rendered PDF page.
    pub pictures: Vec<PictureRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageText {
    pub number: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PictureRef {
    pub page: Option<u32>,
}

/// The extraction of every PDF and image of the corpus.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Extraction {
    /// What read them ("pdfkit+vision on macOS 27.2"), or "fixture" for
    /// `Extraction::fixture`.
    pub reader: String,
    pub documents: BTreeMap<String, ExtractedDoc>,
}

/// `evals/recorded/extracted.json`.
pub const EXTRACTED_FILE: &str = "recorded/extracted.json";

impl Extraction {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "couldn't read {} (run `margin-eval record-embeddings` on the service Mac)",
                path.display()
            )
        })?;
        serde_json::from_str(&text).with_context(|| format!("bad {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)? + "\n")
            .with_context(|| format!("couldn't write {}", path.display()))
    }

    /// A stand-in built from what the corpus was drawn from, for the harness's own
    /// tests (never for metrics): each PDF's pages from `sources/<name>.txt`, every
    /// page with its picture; each image with no text and its picture.
    pub fn fixture(corpus: &Path, docs: &[CorpusDoc]) -> anyhow::Result<Self> {
        let mut documents = BTreeMap::new();
        for doc in docs {
            match doc.kind {
                SourceKind::Pdf => {
                    let text = crate::golden::source_text(corpus, &doc.path, None)?;
                    let pages: Vec<PageText> = text
                        .split("\n\\f\n")
                        .enumerate()
                        .map(|(i, t)| PageText {
                            number: Some(i as u32 + 1),
                            text: t.trim().to_string(),
                        })
                        .collect();
                    let pictures = pages
                        .iter()
                        .map(|p| PictureRef { page: p.number })
                        .collect();
                    documents.insert(doc.path.clone(), ExtractedDoc { pages, pictures });
                }
                SourceKind::Image => {
                    documents.insert(
                        doc.path.clone(),
                        ExtractedDoc {
                            pages: Vec::new(),
                            pictures: vec![PictureRef { page: None }],
                        },
                    );
                }
                _ => {}
            }
        }
        Ok(Self {
            reader: "fixture".into(),
            documents,
        })
    }

    /// `doc` as the chunker takes it. Text documents are read from the corpus; PDFs and
    /// images must have been recorded.
    pub fn extracted(&self, corpus: &Path, doc: &CorpusDoc) -> anyhow::Result<Extracted> {
        match doc.kind {
            SourceKind::Pdf | SourceKind::Image => {
                let rec = self.documents.get(&doc.path).with_context(|| {
                    format!(
                        "{} has no recorded extraction (re-run `margin-eval record-embeddings`)",
                        doc.path
                    )
                })?;
                Ok(Extracted {
                    pages: rec
                        .pages
                        .iter()
                        .map(|p| Page {
                            number: p.number,
                            text: p.text.clone(),
                        })
                        .collect(),
                    pictures: rec
                        .pictures
                        .iter()
                        .map(|p| Picture {
                            path: picture_path(corpus, doc, p.page),
                            page: p.page,
                            at_ms: None,
                            tokens: ImageTokens::High,
                        })
                        .collect(),
                    sound: None,
                    duration_ms: None,
                    format: margin_embed::TextFormat::Plain,
                })
            }
            _ => {
                let bytes = std::fs::read(corpus.join(&doc.path))
                    .with_context(|| format!("couldn't read {}", doc.path))?;
                let text = String::from_utf8_lossy(&bytes).into_owned();
                // As the service's `extract` reads it: the format from the name.
                let format = margin_embed::TextFormat::detect(&doc.path, &text);
                Ok(Extracted {
                    pages: vec![Page { number: None, text }],
                    format,
                    ..Default::default()
                })
            }
        }
    }
}

/// Where a picture of `doc` is: the image file itself, or (for a PDF page) the file
/// name the service renders it to, `margin_embed::page_file`.
fn picture_path(corpus: &Path, doc: &CorpusDoc, page: Option<u32>) -> PathBuf {
    match page {
        Some(n) if doc.kind == SourceKind::Pdf => PathBuf::from(margin_embed::page_file(n)),
        _ => corpus.join(&doc.path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::golden::evals_dir;

    #[test]
    fn the_corpus_documents_and_the_folder_allow_list() {
        let corpus = evals_dir().join("corpus");
        let docs = documents(&corpus).unwrap();
        let paths: Vec<&str> = docs.iter().map(|d| d.path.as_str()).collect();
        for want in [
            "bayes.md",
            "heat-engines.rst",
            "sorting.html",
            "pdf/glacier-scan.pdf",
            "images/calvin-cycle.png",
            "folder/notes/week1.md",
            "folder/notes/README",
            "folder/papers/rc-circuits.pdf",
            "folder/figures/lens-ray-diagram.png",
        ] {
            assert!(paths.contains(&want), "{want} missing from {paths:?}");
        }
        for never in [
            "folder/src/main.rs",
            "folder/data/results.json",
            "folder/archive.zip",
            "folder/.hidden.md",
            "folder/credentials.txt",
            "sources/rc-circuits.txt",
        ] {
            assert!(!paths.contains(&never), "{never} must stay invisible");
        }
        assert!(docs.iter().filter(|d| d.in_folder).count() == 4);
        let pdf = docs
            .iter()
            .find(|d| d.path == "pdf/rc-circuits.pdf")
            .unwrap();
        assert_eq!(pdf.kind, SourceKind::Pdf);
        assert_eq!(pdf.name(), "rc-circuits.pdf");
    }

    #[test]
    fn the_corpus_hash_is_stable_and_sees_changes() {
        let corpus = evals_dir().join("corpus");
        let a = corpus_hash(&corpus).unwrap();
        assert_eq!(a, corpus_hash(&corpus).unwrap());
        assert_eq!(a.len(), 64);
        let dir = std::env::temp_dir().join(format!("margin-evals-hash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.md"), "one").unwrap();
        let one = corpus_hash(&dir).unwrap();
        std::fs::write(dir.join("a.md"), "two").unwrap();
        assert_ne!(one, corpus_hash(&dir).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fixture_extraction_reads_pdfs_by_page() {
        let corpus = evals_dir().join("corpus");
        let docs = documents(&corpus).unwrap();
        let ex = Extraction::fixture(&corpus, &docs).unwrap();
        let glacier = docs
            .iter()
            .find(|d| d.path == "pdf/glacier-scan.pdf")
            .unwrap();
        let e = ex.extracted(&corpus, glacier).unwrap();
        assert_eq!(e.pages[0].number, Some(1));
        assert!(e.pages[0].text.contains("ablation zone"));
        assert_eq!(e.pictures.len(), e.pages.len());
        let md = docs.iter().find(|d| d.path == "bayes.md").unwrap();
        assert!(
            ex.extracted(&corpus, md).unwrap().pages[0]
                .text
                .contains('#')
        );
        // A missing recording is an error, never an empty document.
        let empty = Extraction::default();
        assert!(empty.extracted(&corpus, glacier).is_err());
    }

    #[test]
    fn approx_tokens_counts_characters_and_words() {
        assert_eq!(approx_tokens(""), 0);
        assert_eq!(approx_tokens("abcd"), 1);
        assert_eq!(approx_tokens("a b c d e"), 5);
        assert_eq!(approx_tokens("photosynthesis"), 4);
    }
}
