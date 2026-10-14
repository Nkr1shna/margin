//! The golden sets (`evals/golden/*.jsonl`) and the other checked-in eval files:
//! their types, loading, and the checks that keep them honest (every anchor exists in
//! the corpus, every variant parses as a `PipelineConfig`).

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// `evals/` at the repository root.
pub fn evals_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals")
}

/// What a query's retrieval is scoped to: the whole corpus, or the `folder/` tree as an
/// attached folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum GoldenScope {
    Corpus,
    Folder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryKind {
    /// Exact terms, symbols, numbers.
    Lexical,
    /// The question's words aren't the answer's.
    Semantic,
    /// Answered by a picture, a scanned page or a figure.
    #[serde(rename = "cross-modal")]
    CrossModal,
    /// Nothing in the corpus answers it.
    Negative,
}

impl QueryKind {
    pub const ALL: [QueryKind; 4] = [
        QueryKind::Lexical,
        QueryKind::Semantic,
        QueryKind::CrossModal,
        QueryKind::Negative,
    ];
}

/// Something a relevant hit must match: a hit is relevant when its text contains
/// `anchor` (whitespace-normalised, case-insensitive) or, without an anchor, when it is
/// a picture of `document` (and of `page`, when given).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relevant {
    /// Path inside `evals/corpus`.
    pub document: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
}

/// One line of `evals/golden/retrieval.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrievalQuery {
    pub id: String,
    pub query: String,
    pub scope: GoldenScope,
    pub relevant: Vec<Relevant>,
    pub kind: QueryKind,
    /// "hand", or the model that wrote it (`synth-questions`).
    pub generated_by: String,
    /// The grader's 0–3 seriousness grade (generated questions only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seriousness: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grader: Option<String>,
}

/// One line of `evals/golden/courses.jsonl` (written with R1-evals).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CourseRequest {
    pub id: String,
    pub prompt: String,
    pub documents: Vec<String>,
    pub themes: Vec<String>,
    /// Per lesson (in outline order), documents it must cite.
    #[serde(default)]
    pub must_cite: Vec<Vec<String>>,
}

/// Reads a JSONL file of `T`, one per non-empty line.
pub fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> anyhow::Result<Vec<T>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("couldn't read {}", path.display()))?;
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| {
            serde_json::from_str(l).with_context(|| format!("{}:{}", path.display(), i + 1))
        })
        .collect()
}

/// Lower-case with runs of whitespace collapsed, for anchor matching.
pub fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Whether a hit's text satisfies an anchor.
pub fn matches_anchor(text: &str, anchor: &str) -> bool {
    normalize(text).contains(&normalize(anchor))
}

/// The text an anchor in `document` (page `page`) must be found in, as written for the
/// corpus: a text file itself (HTML without its tags), a PDF's page from
/// `corpus/sources/<name>.txt` (pages separated by a `\f` line), a drawing's labels from
/// `tools/make_corpus.swift`.
pub fn source_text(corpus: &Path, document: &str, page: Option<u32>) -> anyhow::Result<String> {
    let read = |p: PathBuf| {
        std::fs::read_to_string(&p).with_context(|| format!("couldn't read {}", p.display()))
    };
    let ext = document.rsplit('.').next().unwrap_or("");
    match ext {
        "pdf" => {
            let stem = Path::new(document)
                .file_stem()
                .and_then(|s| s.to_str())
                .context("pdf name")?;
            let text = read(corpus.join("sources").join(format!("{stem}.txt")))?;
            let pages: Vec<&str> = text.split("\n\\f\n").collect();
            Ok(match page {
                Some(n) => pages
                    .get(n as usize - 1)
                    .with_context(|| format!("{document} has no page {n}"))?
                    .to_string(),
                None => text,
            })
        }
        "png" | "jpg" | "jpeg" => read(corpus.join("../tools/make_corpus.swift")),
        "html" | "htm" => {
            let html = read(corpus.join(document))?;
            let mut out = String::new();
            let mut in_tag = false;
            for c in html.chars() {
                match c {
                    '<' => in_tag = true,
                    '>' => {
                        in_tag = false;
                        out.push(' ');
                    }
                    c if !in_tag => out.push(c),
                    _ => {}
                }
            }
            Ok(out)
        }
        _ => read(corpus.join(document)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use margin_shared::pipeline::PipelineConfig;
    use std::collections::{HashMap, HashSet};

    fn golden() -> Vec<RetrievalQuery> {
        read_jsonl(&evals_dir().join("golden/retrieval.jsonl")).unwrap()
    }

    #[test]
    fn the_hand_written_queries_are_complete_and_well_formed() {
        let queries = golden();
        assert!(queries.len() >= 60, "{}", queries.len());
        let ids: HashSet<&str> = queries.iter().map(|q| q.id.as_str()).collect();
        assert_eq!(ids.len(), queries.len(), "ids are unique");
        let mut by_kind: HashMap<QueryKind, usize> = HashMap::new();
        for q in &queries {
            *by_kind.entry(q.kind).or_default() += 1;
            assert_eq!(
                q.kind == QueryKind::Negative,
                q.relevant.is_empty(),
                "{}: negatives and only negatives have no relevant items",
                q.id
            );
            assert!(
                q.query.split_whitespace().count() >= 3,
                "{}: too short",
                q.id
            );
            if q.generated_by != "hand" {
                assert!(q.seriousness.is_some_and(|s| s >= 2), "{}", q.id);
            }
        }
        for kind in QueryKind::ALL {
            assert!(by_kind.get(&kind).copied().unwrap_or(0) >= 8, "{kind:?}");
        }
    }

    #[test]
    fn every_anchor_is_in_its_document() {
        let corpus = evals_dir().join("corpus");
        for q in golden() {
            for r in &q.relevant {
                assert!(
                    corpus.join(&r.document).is_file(),
                    "{}: {} isn't in the corpus",
                    q.id,
                    r.document
                );
                if let Some(anchor) = &r.anchor {
                    let text = source_text(&corpus, &r.document, r.page).unwrap();
                    assert!(
                        matches_anchor(&text, anchor),
                        "{}: \"{anchor}\" isn't in {} (page {:?})",
                        q.id,
                        r.document,
                        r.page
                    );
                }
            }
        }
    }

    #[test]
    fn anchors_match_across_line_breaks_and_case() {
        assert!(matches_anchor("The Carnot\n  ENGINE sets", "carnot engine"));
        assert!(!matches_anchor("Carnot", "carnot engine"));
    }

    #[test]
    fn the_corpus_fits_in_ci() {
        fn size(dir: &Path) -> u64 {
            std::fs::read_dir(dir)
                .unwrap()
                .map(|e| {
                    let e = e.unwrap();
                    let m = e.metadata().unwrap();
                    if m.is_dir() { size(&e.path()) } else { m.len() }
                })
                .sum()
        }
        let bytes = size(&evals_dir().join("corpus"));
        assert!(bytes <= 5 * 1024 * 1024, "{bytes} bytes");
        // The folder tree holds files the allow-list must keep invisible.
        let folder = evals_dir().join("corpus/folder");
        for invisible in [
            "src/main.rs",
            "data/results.json",
            "archive.zip",
            ".hidden.md",
            "credentials.txt",
        ] {
            assert!(folder.join(invisible).is_file(), "{invisible}");
        }
    }

    #[test]
    fn variants_are_pipeline_configs() {
        let dir = evals_dir().join("variants");
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let text = std::fs::read_to_string(&path).unwrap();
            let cfg: PipelineConfig =
                toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            names.push((path.file_stem().unwrap().to_string_lossy().to_string(), cfg));
        }
        names.sort_by(|a, b| a.0.cmp(&b.0));
        let stems: Vec<&str> = names.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(stems, ["baseline", "bm25", "dense", "hybrid"]);
        let baseline = &names[0].1;
        assert_eq!(
            baseline,
            &PipelineConfig::default(),
            "baseline is the shipped default"
        );
        let hashes: HashSet<String> = names.iter().map(|(_, c)| c.hash()).collect();
        assert_eq!(hashes.len(), 4, "every variant differs");
    }

    #[test]
    fn gates_and_prices_parse() {
        let gates: crate::Gates =
            toml::from_str(&std::fs::read_to_string(evals_dir().join("gates.toml")).unwrap())
                .unwrap();
        assert!(gates.retrieval.recall_at_5_max_drop > 0.0);
        let prices: crate::Prices =
            toml::from_str(&std::fs::read_to_string(evals_dir().join("prices.toml")).unwrap())
                .unwrap();
        assert!(!prices.models.is_empty());
        assert!(
            prices
                .models
                .values()
                .all(|p| p.input > 0.0 && p.output > 0.0)
        );
    }
}
