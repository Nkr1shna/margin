//! Generated golden questions (§3.1): the `Write`-route generator writes serious
//! questions per corpus section (and multi-hop questions over pairs of sections), each
//! quoting its answer span; the spans are checked against the corpus here,
//! deterministically; a grader on the other provider (when both are configured) grades
//! each 0–3 for seriousness; questions under 2 are dropped. No human review (owner
//! Q8): every candidate and its grade is logged (`evals/golden/synth-log.jsonl`), so
//! the distribution, the drop rate and the grader's canaries are the evidence.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::corpus::{CorpusDoc, Extraction};
use crate::golden::{GoldenScope, QueryKind, Relevant, RetrievalQuery, matches_anchor, source_text};
use crate::index::EvalIndex;
use crate::judge::{GeneratedQuestion, Judge, QuestionCase, Section, SeriousnessGrade};

/// `evals/golden/synth-log.jsonl`.
pub const SYNTH_LOG: &str = "golden/synth-log.jsonl";
/// Sections shorter than this carry too little for a serious question.
pub const SECTION_MIN_CHARS: usize = 200;

/// The corpus's sections as the variant's chunker cuts them: chunks grouped by
/// document, section path and page, in order.
pub fn sections(
    index: &EvalIndex<'_>,
    docs: &[CorpusDoc],
    extraction: &Extraction,
) -> anyhow::Result<Vec<Section>> {
    let mut out = Vec::new();
    for doc in docs {
        let mut groups: Vec<((String, Option<u32>), String)> = Vec::new();
        for c in index.chunks(doc, extraction)? {
            let key = (c.section_path.clone(), c.page);
            match groups.last_mut() {
                Some((k, text)) if *k == key => {
                    text.push_str("\n\n");
                    text.push_str(&c.text);
                }
                _ => groups.push((key, c.text.clone())),
            }
        }
        for ((section_path, page), text) in groups {
            if text.chars().count() >= SECTION_MIN_CHARS {
                out.push(Section {
                    document: doc.path.clone(),
                    section_path: if section_path.is_empty() {
                        doc.name().to_string()
                    } else {
                        section_path
                    },
                    page,
                    text,
                });
            }
        }
    }
    Ok(out)
}

/// The generation requests: each section alone, and each pair of neighbouring sections
/// of one document together (for multi-hop questions).
pub fn requests(sections: &[Section]) -> Vec<Vec<Section>> {
    let mut out: Vec<Vec<Section>> = sections.iter().map(|s| vec![s.clone()]).collect();
    for w in sections.windows(2) {
        if w[0].document == w[1].document {
            out.push(w.to_vec());
        }
    }
    out
}

/// Problems with a generated question's spans against the corpus itself.
pub fn span_problems(corpus: &Path, q: &GeneratedQuestion) -> Vec<String> {
    let mut out = Vec::new();
    if q.spans.is_empty() {
        out.push("quotes no answer span".to_string());
    }
    for s in &q.spans {
        match source_text(corpus, &s.document, s.page) {
            Ok(text) if matches_anchor(&text, &s.quote) => {}
            Ok(_) => out.push(format!("span not in {}: \"{}\"", s.document, s.quote)),
            Err(e) => out.push(format!("{}: {e}", s.document)),
        }
    }
    out
}

/// One candidate, whatever became of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub question: GeneratedQuestion,
    /// The generator's and this module's checks; non-empty means it wasn't graded.
    pub problems: Vec<String>,
    pub grade: Option<SeriousnessGrade>,
    /// The golden id it was kept as.
    pub kept_as: Option<String>,
}

/// What a generation run did.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SynthSummary {
    pub requests: usize,
    pub generated: usize,
    pub failed_checks: usize,
    /// Count per grade 0–3.
    pub grades: [usize; 4],
    pub kept: usize,
    pub kinds: BTreeMap<String, usize>,
}

/// The golden row for a kept question.
pub fn golden_row(
    id: &str,
    q: &GeneratedQuestion,
    grade: &SeriousnessGrade,
    docs: &[CorpusDoc],
) -> RetrievalQuery {
    let image = q.spans.iter().any(|s| {
        docs.iter()
            .any(|d| d.path == s.document && d.kind == margin_shared::SourceKind::Image)
    });
    RetrievalQuery {
        id: id.to_string(),
        query: q.question.clone(),
        scope: GoldenScope::Corpus,
        relevant: q
            .spans
            .iter()
            .map(|s| Relevant {
                document: s.document.clone(),
                page: s.page,
                anchor: Some(s.quote.clone()),
            })
            .collect(),
        kind: if image {
            QueryKind::CrossModal
        } else {
            QueryKind::Semantic
        },
        generated_by: q.generator.clone(),
        seriousness: Some(grade.grade),
        grader: Some(grade.grader.clone()),
    }
}

/// Generates, checks and grades; returns every candidate (kept ones with their new
/// golden id, numbered after `existing`) and the summary.
pub async fn generate(
    generator: &dyn Judge,
    grader: &dyn Judge,
    corpus: &Path,
    docs: &[CorpusDoc],
    sections: &[Section],
    existing: &[RetrievalQuery],
    per_request: u32,
    limit: usize,
) -> anyhow::Result<(Vec<Candidate>, Vec<RetrievalQuery>, SynthSummary)> {
    let mut summary = SynthSummary::default();
    let mut candidates = Vec::new();
    let mut rows = Vec::new();
    let mut next = existing
        .iter()
        .filter_map(|q| q.id.strip_prefix('s').and_then(|n| n.parse::<usize>().ok()))
        .max()
        .unwrap_or(0)
        + 1;
    let seen: std::collections::HashSet<String> =
        existing.iter().map(|q| crate::golden::normalize(&q.query)).collect();
    for req in requests(sections).into_iter().take(limit) {
        summary.requests += 1;
        let qs = generator.synth(&req, per_request).await?;
        for q in qs {
            summary.generated += 1;
            *summary.kinds.entry(q.kind.clone()).or_default() += 1;
            let mut problems = q.problems.clone();
            problems.extend(span_problems(corpus, &q));
            if seen.contains(&crate::golden::normalize(&q.question)) {
                problems.push("duplicates an existing golden question".into());
            }
            if !problems.is_empty() {
                summary.failed_checks += 1;
                candidates.push(Candidate {
                    question: q,
                    problems,
                    grade: None,
                    kept_as: None,
                });
                continue;
            }
            let grade = grader
                .seriousness(&QuestionCase {
                    id: String::new(),
                    question: q.question.clone(),
                    answer: q.answer.clone(),
                    spans: q.spans.clone(),
                })
                .await?;
            summary.grades[grade.grade.min(3) as usize] += 1;
            let kept_as = grade.keeps().then(|| {
                let id = format!("s{next:03}");
                next += 1;
                rows.push(golden_row(&id, &q, &grade, docs));
                id
            });
            if kept_as.is_some() {
                summary.kept += 1;
            }
            candidates.push(Candidate {
                question: q,
                problems,
                grade: Some(grade),
                kept_as,
            });
        }
    }
    Ok((candidates, rows, summary))
}

/// Appends rows to a JSONL file.
pub fn append_jsonl<T: Serialize>(path: &Path, rows: &[T]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    for r in rows {
        writeln!(f, "{}", serde_json::to_string(r)?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::documents;
    use crate::golden::evals_dir;
    use crate::index::testing::register_stand_ins;
    use crate::judge::testing::RuleJudge;
    use crate::judge::Span;
    use crate::recorded::FakeVectors;
    use margin_service::db::Store;
    use margin_shared::pipeline::PipelineConfig;

    #[tokio::test]
    async fn generates_checks_grades_and_keeps_serious_questions() {
        let corpus = evals_dir().join("corpus");
        let docs = documents(&corpus).unwrap();
        let ex = Extraction::fixture(&corpus, &docs).unwrap();
        let v = FakeVectors;
        let index = EvalIndex::build(
            Store::memory().await.unwrap(),
            &PipelineConfig::default(),
            register_stand_ins,
            &v,
            &corpus,
            &docs,
            &ex,
        )
        .await
        .unwrap();
        let secs = sections(&index, &docs, &ex).unwrap();
        assert!(secs.len() > 10);
        assert!(secs.iter().all(|s| s.text.len() >= SECTION_MIN_CHARS));
        assert!(requests(&secs).len() > secs.len(), "pairs for multi-hop");

        let writer = RuleJudge::named("writer");
        let grader = RuleJudge::named("grader");
        let existing = vec![RetrievalQuery {
            id: "s007".into(),
            query: "x y z".into(),
            scope: GoldenScope::Corpus,
            relevant: vec![],
            kind: QueryKind::Negative,
            generated_by: "hand".into(),
            seriousness: None,
            grader: None,
        }];
        let (cands, rows, summary) =
            generate(&writer, &grader, &corpus, &docs, &secs, &existing, 1, 6)
                .await
                .unwrap();
        assert_eq!(summary.requests, 6);
        assert_eq!(cands.len(), summary.generated);
        assert!(!rows.is_empty());
        assert_eq!(rows[0].id, "s008", "numbered after the existing rows");
        assert_eq!(rows.len(), summary.kept);
        assert_eq!(summary.grades[3], summary.kept);
        for r in &rows {
            assert_eq!(r.seriousness, Some(3));
            assert_eq!(r.grader.as_deref(), Some("fake:grader"));
            assert_eq!(r.generated_by, "fake:writer");
            assert!(r.relevant[0].anchor.is_some());
        }
    }

    #[test]
    fn spans_are_checked_against_the_corpus() {
        let corpus = evals_dir().join("corpus");
        let q = |doc: &str, page, quote: &str| GeneratedQuestion {
            question: "q".into(),
            kind: "application".into(),
            answer: String::new(),
            spans: vec![Span {
                document: doc.into(),
                section_path: String::new(),
                page,
                quote: quote.into(),
            }],
            problems: vec![],
            generator: "g".into(),
        };
        assert!(span_problems(&corpus, &q("bayes.md", None, "P(H) is the prior")).is_empty());
        assert!(!span_problems(&corpus, &q("bayes.md", None, "P(H) is the posterior")).is_empty());
        assert!(span_problems(&corpus, &q("pdf/rc-circuits.pdf", Some(2), "about 0.69 tau")).is_empty());
        assert!(!span_problems(&corpus, &q("pdf/rc-circuits.pdf", Some(1), "about 0.69 tau")).is_empty());
        assert!(!span_problems(&corpus, &q("nope.md", None, "x")).is_empty());
        let mut none = q("bayes.md", None, "x");
        none.spans.clear();
        assert_eq!(span_problems(&corpus, &none), vec!["quotes no answer span"]);
    }
}
